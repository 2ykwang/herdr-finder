use crate::model::{FinderError, Result, Session, Snapshot, clean_text};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_CONTENT_CHARS: usize = 65_536;
pub const MAX_CONTENT_LINES: usize = 300;
const POLL_INTERVAL: Duration = Duration::from_millis(25);
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
pub struct Client {
    socket_path: PathBuf,
    timeout: Duration,
    cancelled: Arc<AtomicBool>,
}

impl Client {
    pub fn new(socket_path: impl Into<PathBuf>, timeout: Duration) -> Result<Self> {
        let socket_path = socket_path.into();
        if socket_path.as_os_str().is_empty() {
            return Err(FinderError::new(
                "invalid_socket",
                "A Herdr socket path is required",
            ));
        }
        if timeout.is_zero() || Instant::now().checked_add(timeout).is_none() {
            return Err(FinderError::new(
                "invalid_timeout",
                "Herdr timeout must be positive",
            ));
        }
        Ok(Self {
            socket_path,
            timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn from_env() -> Result<Self> {
        let path = std::env::var_os("HERDR_SOCKET_PATH")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                FinderError::new(
                    "not_in_herdr",
                    "Open Herdr Finder from Herdr. HERDR_SOCKET_PATH is required.",
                )
            })?;
        Self::new(PathBuf::from(path), Duration::from_secs(3))
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    fn check(&self, deadline: Instant) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err(FinderError::new("cancelled", "Herdr request cancelled"));
        }
        if Instant::now() >= deadline {
            return Err(FinderError::new("timeout", "Herdr request timed out"));
        }
        Ok(())
    }

    fn rpc(&self, method: &str, params: Value, deadline: Instant) -> Result<Value> {
        self.check(deadline)?;
        let mut stream = self.connect(deadline)?;
        let result = (|| {
            let request_id = format!(
                "navigator-{}-{}",
                std::process::id(),
                REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            );
            let mut request = serde_json::to_vec(&json!({
                "id": request_id,
                "method": method,
                "params": params,
            }))
            .map_err(|_| FinderError::new("invalid_request", "Cannot encode Herdr request"))?;
            request.push(b'\n');

            let mut written = 0;
            while written < request.len() {
                self.check(deadline)?;
                match stream.write(&request[written..]) {
                    Ok(0) => {
                        return Err(FinderError::new(
                            "unavailable",
                            "Herdr connection closed while writing",
                        ));
                    }
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        return Err(FinderError::new(
                            "unavailable",
                            format!("Cannot write to Herdr: {e}"),
                        ));
                    }
                }
            }
            let mut response = Vec::with_capacity(16_384);
            let mut buffer = [0_u8; 8192];
            loop {
                self.check(deadline)?;
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        return Err(FinderError::new(
                            "invalid_response",
                            "Herdr closed an incomplete response",
                        ));
                    }
                    Ok(n) => {
                        let end = buffer[..n].iter().position(|b| *b == b'\n').map(|n| n + 1);
                        let size = end.unwrap_or(n);
                        if response.len() + size > MAX_RESPONSE_BYTES {
                            return Err(FinderError::new(
                                "response_too_large",
                                "Herdr response exceeded the 8 MiB limit",
                            ));
                        }
                        response.extend_from_slice(&buffer[..size]);
                        if end.is_some() {
                            break;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        continue;
                    }
                    Err(e) => {
                        return Err(FinderError::new(
                            "unavailable",
                            format!("Cannot read from Herdr: {e}"),
                        ));
                    }
                }
            }
            self.check(deadline)?;
            let response: Value = serde_json::from_slice(&response)
                .map_err(|_| FinderError::new("invalid_response", "Herdr returned invalid JSON"))?;
            if response.get("id").and_then(Value::as_str) != Some(&request_id) {
                return Err(FinderError::new(
                    "invalid_response",
                    "Herdr response identity did not match",
                ));
            }
            if let Some(error) = response.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        FinderError::new("invalid_response", "Herdr returned an invalid error")
                    })?;
                return Err(FinderError::new(
                    error["code"].as_str().unwrap_or("api_error"),
                    message,
                ));
            }
            response
                .get("result")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| {
                    FinderError::new("invalid_response", "Herdr response has no result object")
                })
        })();
        let _ = stream.shutdown(Shutdown::Both);
        result
    }

    fn connect(&self, deadline: Instant) -> Result<UnixStream> {
        self.check(deadline)?;
        let bytes = self.socket_path.as_os_str().as_bytes();
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
            return Err(FinderError::new(
                "invalid_socket",
                "Herdr socket path is too long or invalid",
            ));
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (target, byte) in address.sun_path.iter_mut().zip(bytes) {
            *target = *byte as libc::c_char;
        }
        let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1)
            as libc::socklen_t;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            address.sun_len = length as u8;
        }
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }

        // Own the descriptor before fallible configuration so every error path closes it.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let descriptor_flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe {
                libc::fcntl(
                    owned.as_raw_fd(),
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        loop {
            self.check(deadline)?;
            let connected = unsafe {
                libc::connect(
                    owned.as_raw_fd(),
                    std::ptr::from_ref(&address).cast(),
                    length,
                )
            };
            if connected == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EISCONN) => break,
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    // A full Unix listen queue can reject a nonblocking connect
                    // without starting it. Retry only within the same deadline.
                    std::thread::sleep(
                        POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                    );
                    continue;
                }
                Some(libc::EINPROGRESS | libc::EALREADY) => {}
                _ => return Err(io_error(error)),
            }
            loop {
                self.check(deadline)?;
                let mut poll = libc::pollfd {
                    fd: owned.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let wait = POLL_INTERVAL
                    .min(deadline.saturating_duration_since(Instant::now()))
                    .as_millis()
                    .max(1) as i32;
                let ready = unsafe { libc::poll(&mut poll, 1, wait) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(io_error(error));
                }
                if ready == 0 {
                    continue;
                }
                let mut error: libc::c_int = 0;
                let mut size = std::mem::size_of_val(&error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        owned.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        std::ptr::from_mut(&mut error).cast(),
                        &mut size,
                    )
                } < 0
                {
                    return Err(io_error(std::io::Error::last_os_error()));
                }
                if error != 0 {
                    return Err(io_error(std::io::Error::from_raw_os_error(error)));
                }
                break;
            }
            break;
        }
        if unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFL, flags) } < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let stream = UnixStream::from(owned);
        let interval = POLL_INTERVAL
            .min(deadline.saturating_duration_since(Instant::now()))
            .max(Duration::from_millis(1));
        stream
            .set_read_timeout(Some(interval))
            .map_err(|e| FinderError::new("unavailable", e.to_string()))?;
        stream
            .set_write_timeout(Some(interval))
            .map_err(|e| FinderError::new("unavailable", e.to_string()))?;
        Ok(stream)
    }

    fn snapshot_until(&self, deadline: Instant) -> Result<Snapshot> {
        let result = self.rpc("session.snapshot", json!({}), deadline)?;
        if result["type"] != "session_snapshot" {
            return Err(FinderError::new(
                "invalid_response",
                "Herdr returned an invalid snapshot response",
            ));
        }
        Snapshot::from_native(&result["snapshot"])
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        self.snapshot_until(Instant::now() + self.timeout)
    }

    pub fn focus(&self, selected: &Session) -> Result<()> {
        let deadline = Instant::now() + self.timeout;
        let before = self.snapshot_until(deadline)?;
        let current = before.focus_target(selected)?;
        let result = self.rpc(
            "pane.focus",
            json!({ "pane_id": current.pane_id }),
            deadline,
        )?;
        let pane = &result["pane"];
        if result["type"] != "pane_info"
            || !same_address(pane, current)
            || pane["terminal_id"] != current.terminal_id
            || pane["focused"] != true
        {
            return Err(FinderError::new(
                "focus_changed",
                "Focus target changed. Refresh and verify the active terminal.",
            ));
        }
        let after = self.snapshot_until(deadline)?;
        if !after.focus_target(current)?.focused {
            return Err(FinderError::new(
                "focus_changed",
                "Focus changed before it could be confirmed. Select the terminal again.",
            ));
        }
        Ok(())
    }

    pub fn read_contents(&self, selected: &[Session]) -> HashMap<String, String> {
        self.read_batch(selected).unwrap_or_default()
    }

    fn read_batch(&self, selected: &[Session]) -> Result<HashMap<String, String>> {
        if selected.is_empty() {
            return Ok(HashMap::new());
        }
        let deadline = Instant::now() + self.timeout;
        let before = self.snapshot_until(deadline)?;
        let mut seen = HashSet::new();
        let current: Vec<_> = selected
            .iter()
            .filter_map(|s| before.focus_target(s).ok())
            .filter(|s| seen.insert(s.terminal_id.clone()))
            .cloned()
            .collect();
        if current.is_empty() {
            return Ok(HashMap::new());
        }

        // Reserve part of the deadline for the snapshot that verifies the completed reads.
        let read_deadline = Instant::now()
            + deadline
                .saturating_duration_since(Instant::now())
                .mul_f64(0.7);
        let next = AtomicUsize::new(0);
        let collected = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..current.len().min(4) {
                scope.spawn(|| {
                    loop {
                        if self.check(read_deadline).is_err() {
                            break;
                        }
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(session) = current.get(index) else {
                            break;
                        };

                        // Recent reads can send wheel input to idle agent TUIs on
                        // Herdr 0.9.5. Keep every pane on the visible-only path.
                        let result = self.rpc(
                            "pane.read",
                            json!({
                                "pane_id": session.pane_id,
                                "source": "visible",
                                "format": "text",
                                "strip_ansi": true,
                            }),
                            read_deadline,
                        );
                        if let Ok(result) = result {
                            let read = &result["read"];
                            if result["type"] == "pane_read"
                                && same_address(read, session)
                                && read["source"] == "visible"
                                && read["format"] == "text"
                                && let Some(text) = read["text"].as_str()
                            {
                                collected.lock().unwrap().push((index, clean_body(text)));
                            }
                        }
                    }
                });
            }
        });
        let collected = collected.into_inner().unwrap();
        if collected.is_empty() {
            return Ok(HashMap::new());
        }

        // Discard text if the terminal moved or its native identity changed during the read.
        let after = self.snapshot_until(deadline)?;
        let mut results = HashMap::new();
        for (index, text) in collected {
            let session = &current[index];
            if let Ok(verified) = after.focus_target(session)
                && verified.pane_id == session.pane_id
                && verified.workspace_id == session.workspace_id
                && verified.tab_id == session.tab_id
            {
                results.insert(session.terminal_id.clone(), text);
            }
        }
        Ok(results)
    }

    pub fn open_popup(&self) -> Result<()> {
        let result = self.rpc(
            "plugin.pane.open",
            json!({
                "plugin_id": "herdr-finder",
                "entrypoint": "finder",
                "placement": "popup",
                "width": "95%",
                "height": "90%",
                "focus": true,
            }),
            Instant::now() + self.timeout,
        )?;
        if result["type"] != "plugin_pane_opened"
            || result["plugin_pane"]["plugin_id"] != "herdr-finder"
            || result["plugin_pane"]["entrypoint"] != "finder"
        {
            return Err(FinderError::new(
                "invalid_response",
                "Herdr did not confirm the navigator popup",
            ));
        }
        Ok(())
    }
}

fn io_error(error: std::io::Error) -> FinderError {
    FinderError::new("unavailable", format!("Cannot connect to Herdr: {error}"))
}

fn same_address(value: &Value, session: &Session) -> bool {
    value["pane_id"] == session.pane_id
        && value["workspace_id"] == session.workspace_id
        && value["tab_id"] == session.tab_id
}

pub fn clean_body(value: &str) -> String {
    let start = value
        .char_indices()
        .rev()
        .nth(MAX_CONTENT_CHARS - 1)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let cleaned = clean_text(&value[start..], true);
    let lines: Vec<_> = cleaned.split('\n').collect();
    let text = lines[lines.len().saturating_sub(MAX_CONTENT_LINES)..].join("\n");
    let start = text
        .char_indices()
        .rev()
        .nth(MAX_CONTENT_CHARS - 1)
        .map(|(i, _)| i)
        .unwrap_or(0);
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicUsize;
    use std::thread::{self, JoinHandle};

    enum Reply {
        Json(Value),
        Raw(Vec<u8>),
        Stall,
    }

    struct Server {
        _directory: tempfile::TempDir,
        path: PathBuf,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
        requests: Arc<Mutex<Vec<Value>>>,
        closed: Arc<AtomicUsize>,
    }

    impl Server {
        fn new(respond: impl Fn(&Value) -> Reply + Send + Sync + 'static) -> Self {
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let path = directory.path().join("api.sock");
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let closed = Arc::new(AtomicUsize::new(0));
            let (stop_worker, requests_worker, closed_worker) =
                (stop.clone(), requests.clone(), closed.clone());
            let respond = Arc::new(respond);
            let thread = thread::spawn(move || {
                let mut workers = Vec::new();
                while !stop_worker.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let (respond, requests, closed, stop) = (
                                respond.clone(),
                                requests_worker.clone(),
                                closed_worker.clone(),
                                stop_worker.clone(),
                            );
                            workers.push(thread::spawn(move || {
                                stream.set_nonblocking(false).unwrap();
                                stream
                                    .set_read_timeout(Some(Duration::from_millis(500)))
                                    .unwrap();
                                stream
                                    .set_write_timeout(Some(Duration::from_millis(500)))
                                    .unwrap();
                                let mut line = String::new();
                                if BufReader::new(stream.try_clone().unwrap())
                                    .read_line(&mut line)
                                    .is_err()
                                {
                                    return;
                                }
                                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                                    return;
                                };
                                requests.lock().unwrap().push(request.clone());
                                match respond(&request) {
                                    Reply::Json(reply) => {
                                        let mut envelope = reply;
                                        envelope["id"] = request["id"].clone();
                                        let mut bytes = serde_json::to_vec(&envelope).unwrap();
                                        bytes.push(b'\n');
                                        let _ = stream.write_all(&bytes);
                                    }
                                    Reply::Raw(bytes) => {
                                        let _ = stream.write_all(&bytes);
                                    }
                                    Reply::Stall => {
                                        let mut buffer = [0_u8; 8];
                                        loop {
                                            match stream.read(&mut buffer) {
                                                Ok(0) => {
                                                    closed.fetch_add(1, Ordering::Relaxed);
                                                    break;
                                                }
                                                Ok(_) => {}
                                                Err(_) if !stop.load(Ordering::Acquire) => continue,
                                                Err(_) => break,
                                            }
                                        }
                                    }
                                }
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(_) => break,
                    }
                }
                for worker in workers {
                    worker.join().unwrap();
                }
            });
            Self {
                _directory: directory,
                path,
                stop,
                thread: Some(thread),
                requests,
                closed,
            }
        }
        fn client(&self) -> Client {
            Client::new(&self.path, Duration::from_secs(1)).unwrap()
        }

        fn wait_closed(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_millis(300);
            while self.closed.load(Ordering::Acquire) < count && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(self.closed.load(Ordering::Acquire), count);
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn native(count: usize) -> Value {
        let panes: Vec<_> = (0..count)
            .map(|n| {
                json!({
                    "pane_id": format!("pane-{n}"),
                    "terminal_id": format!("term-{n}"),
                    "workspace_id": "workspace-z",
                    "tab_id": "tab-q",
                    "agent_status": "unknown",
                    "focused": false,
                })
            })
            .collect();
        json!({
            "workspaces": [{
                "workspace_id": "workspace-z",
                "label": "Development",
                "focused": true,
            }],
            "tabs": [{
                "tab_id": "tab-q",
                "workspace_id": "workspace-z",
                "label": "1",
                "number": 1,
            }],
            "panes": panes,
            "agents": [],
        })
    }

    fn snapshot(data: Value) -> Reply {
        Reply::Json(json!({
            "result": {
                "type": "session_snapshot",
                "snapshot": data,
            },
        }))
    }

    fn content(pane: &Value) -> Reply {
        Reply::Json(json!({
            "result": {
                "type": "pane_read",
                "read": {
                    "pane_id": pane["pane_id"],
                    "workspace_id": pane["workspace_id"],
                    "tab_id": pane["tab_id"],
                    "source": "visible",
                    "format": "text",
                    "text": "first\n\u{1b}[31mneedle\u{1b}[0m",
                    "revision": 0,
                    "truncated": false,
                },
            },
        }))
    }

    #[test]
    fn snapshot_uses_actual_newline_unix_socket_and_realistic_payload() {
        let mut data = native(1);
        data["padding"] = json!("x".repeat(300_000));
        let server = Server::new(move |_| snapshot(data.clone()));
        assert_eq!(server.client().snapshot().unwrap().sessions.len(), 1);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0]["method"], "session.snapshot");
        assert_eq!(requests[0]["params"], json!({}));
    }

    #[test]
    fn malformed_and_oversized_responses_are_bounded_errors() {
        for (bytes, code) in [
            (b"invalid\n".to_vec(), "invalid_response"),
            (b"{\"id\":\"partial\"}".to_vec(), "invalid_response"),
            (vec![b'x'; MAX_RESPONSE_BYTES + 10], "response_too_large"),
        ] {
            let server = Server::new(move |_| Reply::Raw(bytes.clone()));
            assert_eq!(server.client().snapshot().unwrap_err().code, code);
        }
        let server = Server::new(|_| Reply::Raw(b"{\"id\":\"other\",\"result\":{}}\n".to_vec()));
        assert_eq!(
            server.client().snapshot().unwrap_err().code,
            "invalid_response"
        );
    }

    #[test]
    fn native_errors_keep_code_and_sanitize_literal_message() {
        let server = Server::new(|_| {
            Reply::Json(json!({
                "error": {
                    "code": "not_found",
                    "message": "\u{1b}[31mMissing\u{1b}[0m\n\u{202e}terminal",
                },
            }))
        });
        let error = server.client().snapshot().unwrap_err();
        assert_eq!(error.code, "not_found");
        assert_eq!(error.message, "Missing terminal");
    }

    #[test]
    fn cancelled_and_timed_out_reads_close_without_detached_readers() {
        let server = Server::new(|_| Reply::Stall);
        let client = server.client();
        let worker = client.clone();
        let handle = thread::spawn(move || worker.snapshot());
        while server.requests.lock().unwrap().is_empty() {
            thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        client.cancel();
        assert_eq!(handle.join().unwrap().unwrap_err().code, "cancelled");
        assert!(start.elapsed() < Duration::from_millis(200));
        server.wait_closed(1);
        let client = Client::new(&server.path, Duration::from_millis(50)).unwrap();
        assert_eq!(client.snapshot().unwrap_err().code, "timeout");
        server.wait_closed(2);
    }

    #[test]
    fn focus_uses_fresh_address_and_postflight_identity() {
        let mut before = native(1);
        let selected = Snapshot::from_native(&before).unwrap().sessions.remove(0);
        before["panes"][0]["pane_id"] = json!("moved");
        let mut after = before.clone();
        after["panes"][0]["focused"] = json!(true);
        let snapshots = AtomicUsize::new(0);
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                snapshot(if snapshots.fetch_add(1, Ordering::Relaxed) == 0 {
                    before.clone()
                } else {
                    after.clone()
                })
            } else {
                assert_eq!(request["params"]["pane_id"], "moved");
                Reply::Json(json!({
                    "result": {
                        "type": "pane_info",
                        "pane": after["panes"][0],
                    },
                }))
            }
        });
        server.client().focus(&selected).unwrap();
        assert_eq!(server.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn disappeared_selection_does_not_focus_a_reused_address() {
        let data = native(1);
        let selected = Snapshot::from_native(&data).unwrap().sessions.remove(0);
        let mut replaced = data;
        replaced["panes"][0]["terminal_id"] = json!("neighbor");
        let server = Server::new(move |_| snapshot(replaced.clone()));
        assert_eq!(
            server.client().focus(&selected).unwrap_err().code,
            "missing_terminal"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn content_batch_has_four_workers_two_snapshots_and_visible_only_reads() {
        let data = native(12);
        let selected = Snapshot::from_native(&data).unwrap().sessions;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let peak_worker = peak.clone();
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                return snapshot(data.clone());
            }
            assert_eq!(request["method"], "pane.read");
            assert_eq!(request["params"]["source"], "visible");
            assert!(request["params"].get("lines").is_none());
            let count = active.fetch_add(1, Ordering::Relaxed) + 1;
            peak_worker.fetch_max(count, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(10));
            active.fetch_sub(1, Ordering::Relaxed);
            let pane = data["panes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["pane_id"] == request["params"]["pane_id"])
                .unwrap();
            if pane["terminal_id"] == "term-3" {
                Reply::Json(json!({
                    "error": {
                        "code": "gone",
                        "message": "Gone",
                    },
                }))
            } else {
                content(pane)
            }
        });
        let contents = server.client().read_contents(&selected);
        assert_eq!(contents.len(), 11);
        assert_eq!(contents["term-0"], "first\nneedle");
        assert_eq!(peak.load(Ordering::Relaxed), 4);
        assert_eq!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["method"] == "session.snapshot")
                .count(),
            2
        );
    }

    #[test]
    fn batch_discards_replaced_and_moved_read_addresses() {
        let data = native(3);
        let selected = Snapshot::from_native(&data).unwrap().sessions;
        let mut after = data.clone();
        after["panes"][1]["terminal_id"] = json!("replacement");
        after["panes"][2]["pane_id"] = json!("moved");
        let snapshots = AtomicUsize::new(0);
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                return snapshot(if snapshots.fetch_add(1, Ordering::Relaxed) == 0 {
                    data.clone()
                } else {
                    after.clone()
                });
            }
            let pane = data["panes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["pane_id"] == request["params"]["pane_id"])
                .unwrap();
            content(pane)
        });
        let contents = server.client().read_contents(&selected);
        assert_eq!(contents.len(), 1);
        assert!(contents.contains_key("term-0"));
    }

    #[test]
    fn stalled_pane_does_not_discard_completed_batch_results() {
        let data = native(3);
        let selected = Snapshot::from_native(&data).unwrap().sessions;
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                return snapshot(data.clone());
            }
            if request["params"]["pane_id"] == "pane-1" {
                return Reply::Stall;
            }
            let pane = data["panes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["pane_id"] == request["params"]["pane_id"])
                .unwrap();
            content(pane)
        });
        let client = Client::new(&server.path, Duration::from_millis(200)).unwrap();
        let contents = client.read_contents(&selected);
        assert_eq!(contents.len(), 2);
        server.wait_closed(1);
    }

    #[test]
    fn batch_cancel_joins_all_four_workers_and_closes_their_streams() {
        let data = native(8);
        let selected = Snapshot::from_native(&data).unwrap().sessions;
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                snapshot(data.clone())
            } else {
                Reply::Stall
            }
        });
        let client = server.client();
        let worker = client.clone();
        let handle = thread::spawn(move || worker.read_contents(&selected));
        let deadline = Instant::now() + Duration::from_millis(500);
        while server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["method"] == "pane.read")
            .count()
            < 4
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        client.cancel();
        assert!(handle.join().unwrap().is_empty());
        assert!(start.elapsed() < Duration::from_millis(200));
        server.wait_closed(4);
    }

    #[test]
    fn replacement_between_preflight_and_focus_is_reported() {
        let data = native(1);
        let selected = Snapshot::from_native(&data).unwrap().sessions.remove(0);
        let mut replacement = data["panes"][0].clone();
        replacement["terminal_id"] = json!("neighbor");
        replacement["focused"] = json!(true);
        let server = Server::new(move |request| {
            if request["method"] == "session.snapshot" {
                snapshot(data.clone())
            } else {
                Reply::Json(json!({
                    "result": {
                        "type": "pane_info",
                        "pane": replacement,
                    },
                }))
            }
        });
        assert_eq!(
            server.client().focus(&selected).unwrap_err().code,
            "focus_changed"
        );
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn popup_uses_public_manifest_entrypoint() {
        let server = Server::new(|request| {
            assert_eq!(request["method"], "plugin.pane.open");
            assert_eq!(request["params"]["placement"], "popup");
            assert_eq!(request["params"]["entrypoint"], "finder");
            Reply::Json(json!({
                "result": {
                    "type": "plugin_pane_opened",
                    "plugin_pane": {
                        "plugin_id": "herdr-finder",
                        "entrypoint": "finder",
                    },
                },
            }))
        });
        server.client().open_popup().unwrap();
    }

    #[test]
    fn local_content_cap_is_unicode_safe_after_cleanup_expansion() {
        let text = (0..400)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(clean_body(&text).lines().count(), 300);
        assert!(clean_body(&"\u{d55c}".repeat(100_000)).chars().count() <= MAX_CONTENT_CHARS);
        assert!(clean_body(&"\t".repeat(100_000)).chars().count() <= MAX_CONTENT_CHARS);
        let path = PathBuf::from("x".repeat(300));
        assert_eq!(
            Client::new(path, Duration::from_millis(10))
                .unwrap()
                .snapshot()
                .unwrap_err()
                .code,
            "invalid_socket"
        );
        assert!(Client::new("/tmp/example", Duration::MAX).is_err());
    }
}
