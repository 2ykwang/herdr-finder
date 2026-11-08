use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;

pub type Result<T> = std::result::Result<T, FinderError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinderError {
    pub code: String,
    pub message: String,
}

impl FinderError {
    pub fn new(code: impl AsRef<str>, message: impl AsRef<str>) -> Self {
        let code: String = clean_text(code.as_ref(), false).chars().take(80).collect();
        let message: String = clean_text(message.as_ref(), false)
            .chars()
            .take(400)
            .collect();
        Self {
            code: if code.is_empty() {
                "navigator_error".into()
            } else {
                code
            },
            message: if message.is_empty() {
                "Herdr Finder failed".into()
            } else {
                message
            },
        }
    }
}

impl fmt::Display for FinderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FinderError {}

fn invalid(message: &str) -> FinderError {
    FinderError::new(
        "invalid_snapshot",
        format!("Invalid Herdr snapshot: {message}"),
    )
}

pub fn clean_text(value: &str, multiline: bool) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut chars = normalized.chars().peekable();
    let mut safe = String::with_capacity(value.len());

    while let Some(ch) = chars.next() {
        let command = if ch == '\u{1b}' {
            chars.next()
        } else {
            Some(ch)
        };
        match (ch, command) {
            ('\u{1b}', Some('[')) | ('\u{9b}', _) => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
                continue;
            }
            ('\u{1b}', Some(']')) | ('\u{9d}', _) => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' || next == '\u{9c}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
            ('\u{1b}', Some('P' | 'X' | '^' | '_'))
            | ('\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}', _) => {
                while let Some(next) = chars.next() {
                    if next == '\u{9c}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
                continue;
            }
            ('\u{1b}', _) => continue,
            _ => {}
        }
        if matches!(
            ch,
            '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        ) {
            continue;
        }
        match ch {
            '\n' if multiline => safe.push('\n'),
            '\n' | '\t' if !multiline => safe.push(' '),
            '\t' => safe.push_str("    "),
            _ if ch.is_control() => {}
            _ => safe.push(ch),
        }
    }

    if multiline {
        safe.split('\n')
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_matches('\n')
            .to_owned()
    } else {
        safe.split_whitespace().collect::<Vec<_>>().join(" ")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    pub id: String,
    pub label: String,
    pub focused: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tab {
    pub id: String,
    pub workspace_id: String,
    pub label: String,
    pub number: usize,
    pub focused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub terminal_id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub tab_label: String,
    pub title: String,
    pub kind: String,
    pub display_kind: String,
    pub name: String,
    pub status: String,
    pub cwd: String,
    pub focused: bool,
    pub native_reference: Option<(String, String, String)>,
    pub split_index: usize,
    pub split_count: usize,
    pub search_titles: Vec<String>,
    pub tab_number: usize,
    pub revision: u64,
    pub activity_at: Option<DateTime<Utc>>,
    pub activity_source: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            terminal_id: String::new(),
            pane_id: String::new(),
            workspace_id: String::new(),
            tab_id: String::new(),
            tab_label: String::new(),
            title: "Terminal".into(),
            kind: "terminal".into(),
            display_kind: "Terminal".into(),
            name: String::new(),
            status: "unknown".into(),
            cwd: String::new(),
            focused: false,
            native_reference: None,
            split_index: 1,
            split_count: 1,
            search_titles: Vec::new(),
            tab_number: 0,
            revision: 0,
            activity_at: None,
            activity_source: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub workspaces: Vec<Workspace>,
    pub sessions: Vec<Session>,
    pub tabs: Vec<Tab>,
}

fn rows<'a>(data: &'a Value, key: &str) -> Result<&'a [Value]> {
    let values = data
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(&format!("{key} must be an array")))?;
    if values.iter().any(|row| !row.is_object()) {
        return Err(invalid("expected object rows"));
    }
    Ok(values)
}

fn id<'a>(row: &'a Value, key: &str) -> Result<&'a str> {
    let value = row
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(&format!("missing {key}")))?;
    if value.is_empty() || value.len() > 4096 || clean_text(value, false) != value {
        return Err(invalid(&format!("invalid {key}")));
    }
    Ok(value)
}

fn index<'a>(values: &'a [Value], key: &str) -> Result<HashMap<String, &'a Value>> {
    let mut result = HashMap::new();
    for row in values {
        if result.insert(id(row, key)?.to_owned(), row).is_some() {
            return Err(invalid(&format!("duplicate {key}")));
        }
    }
    Ok(result)
}

fn text(row: &Value, key: &str) -> String {
    clean_text(row.get(key).and_then(Value::as_str).unwrap_or(""), false)
}

fn choose(first: String, second: impl FnOnce() -> String) -> String {
    if first.is_empty() { second() } else { first }
}

fn combined(agent: &Value, pane: &Value, key: &str) -> String {
    choose(text(agent, key), || text(pane, key))
}

fn number(row: &Value, key: &str) -> usize {
    row.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
}

fn reference(row: &Value) -> Result<Option<(String, String, String)>> {
    let Some(value) = row.get("agent_session").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(invalid("agent_session must be an object"));
    }
    Ok(Some((
        id(value, "agent")?.into(),
        id(value, "kind")?.into(),
        id(value, "value")?.into(),
    )))
}

fn tab_label(tab: &Value) -> String {
    let label = text(tab, "label");
    if label == number(tab, "number").to_string() {
        String::new()
    } else {
        label
    }
}

fn terminal_title(value: String, cwd: &str) -> String {
    let basename = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    for path in [cwd, basename] {
        if !path.is_empty() && value == path {
            return String::new();
        }
        if !path.is_empty()
            && let Some(title) = value.strip_suffix(&format!(" | {path}"))
        {
            return title.trim().into();
        }
    }
    value
}

fn focused_id<'a>(
    data: &'a Value,
    key: &str,
    indexed: &HashMap<String, &Value>,
) -> Result<Option<&'a str>> {
    match data.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(value) => match value.as_str() {
            Some(value) if indexed.contains_key(value) => Ok(Some(value)),
            _ => Err(invalid(&format!("invalid {key}"))),
        },
    }
}

impl Snapshot {
    pub fn from_native(data: &Value) -> Result<Self> {
        let workspace_rows = rows(data, "workspaces")?;
        let tab_rows = rows(data, "tabs")?;
        let pane_rows = rows(data, "panes")?;
        let workspaces = index(workspace_rows, "workspace_id")?;
        let tabs = index(tab_rows, "tab_id")?;
        let panes = index(pane_rows, "pane_id")?;
        index(pane_rows, "terminal_id")?;
        let agents = index(rows(data, "agents")?, "terminal_id")?;

        for tab in tab_rows {
            if !workspaces.contains_key(id(tab, "workspace_id")?) {
                return Err(invalid("tab has no workspace parent"));
            }
        }
        for pane in pane_rows {
            let parent = tabs
                .get(id(pane, "tab_id")?)
                .ok_or_else(|| invalid("pane has no tab parent"))?;
            if id(parent, "workspace_id")? != id(pane, "workspace_id")? {
                return Err(invalid("pane has inconsistent parents"));
            }
        }
        for agent in agents.values() {
            let pane = panes
                .get(id(agent, "pane_id")?)
                .ok_or_else(|| invalid("agent has no pane"))?;
            for key in ["terminal_id", "workspace_id", "tab_id"] {
                if id(agent, key)? != id(pane, key)? {
                    return Err(invalid("agent and pane identities disagree"));
                }
            }
            let (a, p) = (text(agent, "agent"), text(pane, "agent"));
            if !a.is_empty() && !p.is_empty() && a != p {
                return Err(invalid("agent and pane kinds disagree"));
            }
            let (a, p) = (reference(agent)?, reference(pane)?);
            if a.is_some() && p.is_some() && a != p {
                return Err(invalid("agent and pane references disagree"));
            }
        }
        let fw = focused_id(data, "focused_workspace_id", &workspaces)?;
        let ft = focused_id(data, "focused_tab_id", &tabs)?;
        let fp = focused_id(data, "focused_pane_id", &panes)?;
        if let (Some(w), Some(t)) = (fw, ft)
            && id(tabs[t], "workspace_id")? != w
        {
            return Err(invalid("focused tab has inconsistent parent"));
        }
        if let Some(p) = fp
            && (fw.is_some_and(|w| panes[p]["workspace_id"].as_str() != Some(w))
                || ft.is_some_and(|t| panes[p]["tab_id"].as_str() != Some(t)))
        {
            return Err(invalid("focused pane has inconsistent parents"));
        }
        let workspace_models: Vec<_> = workspace_rows
            .iter()
            .map(|row| Workspace {
                id: row["workspace_id"].as_str().unwrap().into(),
                label: choose(text(row, "label"), || "Workspace".into()),
                focused: fw.map_or(row["focused"] == true, |v| row["workspace_id"] == v),
            })
            .collect();
        let tab_models: Vec<_> = tab_rows
            .iter()
            .map(|row| Tab {
                id: row["tab_id"].as_str().unwrap().into(),
                workspace_id: row["workspace_id"].as_str().unwrap().into(),
                label: tab_label(row),
                number: number(row, "number"),
                focused: ft.map_or(row["focused"] == true, |v| row["tab_id"] == v),
            })
            .collect();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for pane in pane_rows {
            *counts.entry(id(pane, "tab_id")?).or_default() += 1;
        }
        let mut positions: HashMap<&str, usize> = HashMap::new();
        let mut sessions = Vec::with_capacity(pane_rows.len());
        for pane in pane_rows {
            let terminal = id(pane, "terminal_id")?;
            let agent = agents.get(terminal).copied().unwrap_or(&Value::Null);
            let tab_id = id(pane, "tab_id")?;
            let tab = tabs[tab_id];
            let kind = choose(combined(agent, pane, "agent"), || "terminal".into());
            let generic = if kind == "terminal" {
                "Terminal"
            } else {
                &kind
            };
            let display_kind = choose(combined(agent, pane, "display_agent"), || generic.into());
            let name = text(agent, "name");
            let cwd = choose(combined(agent, pane, "foreground_cwd"), || {
                combined(agent, pane, "cwd")
            });
            let label = tab_label(tab);
            // Search aliases retain task metadata hidden by a higher-priority display label.
            let mut aliases = Vec::new();
            for value in [
                text(pane, "label"),
                text(agent, "title"),
                text(pane, "title"),
                terminal_title(text(agent, "terminal_title_stripped"), &cwd),
                terminal_title(text(pane, "terminal_title_stripped"), &cwd),
            ] {
                if !value.is_empty() && !aliases.contains(&value) {
                    aliases.push(value);
                }
            }
            let title = [
                text(pane, "label"),
                combined(agent, pane, "title"),
                terminal_title(combined(agent, pane, "terminal_title_stripped"), &cwd),
                name.clone(),
                label.clone(),
                generic.into(),
            ]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap();
            let status = agent
                .get("agent_status")
                .or_else(|| pane.get("agent_status"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let status = if ["working", "blocked", "done", "idle", "unknown"].contains(&status) {
                status
            } else {
                "unknown"
            };
            let position = positions.entry(tab_id).or_default();
            *position += 1;
            sessions.push(Session {
                terminal_id: terminal.into(),
                pane_id: id(pane, "pane_id")?.into(),
                workspace_id: id(pane, "workspace_id")?.into(),
                tab_id: tab_id.into(),
                tab_label: label,
                title,
                kind,
                display_kind,
                name,
                status: status.into(),
                cwd,
                focused: fp.map_or(pane["focused"] == true, |v| pane["pane_id"] == v),
                native_reference: reference(agent)?.or(reference(pane)?),
                split_index: *position,
                split_count: counts[tab_id],
                search_titles: aliases,
                tab_number: number(tab, "number"),
                revision: pane["revision"].as_u64().unwrap_or(0),
                ..Session::default()
            });
        }
        let workspace_order: HashMap<_, _> = workspace_models
            .iter()
            .enumerate()
            .map(|(n, w)| (w.id.as_str(), n))
            .collect();
        let tab_order: HashMap<_, _> = tab_models
            .iter()
            .enumerate()
            .map(|(n, t)| (t.id.as_str(), n))
            .collect();
        sessions.sort_by_key(|s| {
            (
                workspace_order[s.workspace_id.as_str()],
                tab_order[s.tab_id.as_str()],
            )
        });
        Ok(Self {
            workspaces: workspace_models,
            sessions,
            tabs: tab_models,
        })
    }

    pub fn focus_target(&self, selected: &Session) -> Result<&Session> {
        let current = self
            .sessions
            .iter()
            .find(|s| s.terminal_id == selected.terminal_id)
            .ok_or_else(|| {
                FinderError::new(
                    "missing_terminal",
                    "The selected terminal is no longer available. Refresh and select again.",
                )
            })?;
        if current.kind != selected.kind
            || selected.native_reference.is_some()
                && current.native_reference != selected.native_reference
        {
            return Err(FinderError::new(
                "terminal_changed",
                "The selected terminal's agent session changed. Refresh and select again.",
            ));
        }
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native() -> Value {
        json!({
            "workspaces": [{
                "workspace_id": "ws-z",
                "label": "Service",
                "focused": true,
            }],
            "tabs": [{
                "tab_id": "tab-q",
                "workspace_id": "ws-z",
                "number": 1,
                "label": "review",
            }],
            "panes": [{
                "pane_id": "unrelated-address",
                "terminal_id": "term-a",
                "workspace_id": "ws-z",
                "tab_id": "tab-q",
                "agent": "codex",
                "agent_status": "working",
                "cwd": "/project/repo",
                "focused": true,
            }],
            "agents": [{
                "pane_id": "unrelated-address",
                "terminal_id": "term-a",
                "workspace_id": "ws-z",
                "tab_id": "tab-q",
                "agent": "codex",
                "agent_status": "working",
                "name": "api",
                "title": "Review request handling",
                "terminal_title_stripped": "Terminal task | repo",
            }],
        })
    }

    #[test]
    fn literal_cleanup_strips_controls_and_keeps_multiline_indentation() {
        assert_eq!(
            clean_text(
                "\x1b]0;hidden\x07\x1b[31m[bold]Text[/bold]\x1b[0m\n\t\u{202e}A\0",
                false
            ),
            "[bold]Text[/bold] A"
        );
        assert_eq!(clean_text("safe\x1b]0;unfinished", false), "safe");
        assert_eq!(
            clean_text("first\r\n  second\tline", true),
            "first\n  second    line"
        );
        assert_eq!(clean_text("A\x1bPprivate\x1b\\B", false), "AB");
    }

    #[test]
    fn manual_display_retains_metadata_and_osc_search_titles() {
        let mut data = native();
        data["panes"][0]["label"] = json!("\x1b[31m[bold]Manual[/bold]\x1b[0m");
        data["panes"][0]["title"] = json!("Pane metadata");
        let model = Snapshot::from_native(&data).unwrap();
        let row = &model.sessions[0];
        assert_eq!(row.title, "[bold]Manual[/bold]");
        assert_eq!(
            row.search_titles,
            vec![
                "[bold]Manual[/bold]",
                "Review request handling",
                "Pane metadata",
                "Terminal task"
            ]
        );
    }

    #[test]
    fn title_priority_and_exact_cwd_suffix_have_no_guesses() {
        let mut data = native();
        assert_eq!(
            Snapshot::from_native(&data).unwrap().sessions[0].title,
            "Review request handling"
        );
        data["agents"][0]["title"] = json!("");
        assert_eq!(
            Snapshot::from_native(&data).unwrap().sessions[0].title,
            "Terminal task"
        );
        data["agents"][0]["terminal_title_stripped"] = json!("repo");
        assert_eq!(
            Snapshot::from_native(&data).unwrap().sessions[0].title,
            "api"
        );
        data["agents"][0]["name"] = json!("");
        assert_eq!(
            Snapshot::from_native(&data).unwrap().sessions[0].title,
            "review"
        );
        data["tabs"][0]["label"] = json!("1");
        let model = Snapshot::from_native(&data).unwrap();
        assert_eq!(model.sessions[0].title, "codex");
        assert_eq!(model.tabs[0].label, "");
        assert_eq!(model.tabs[0].number, 1);
        data["agents"][0]["terminal_title_stripped"] = json!("Task | otherrepo");
        assert_eq!(
            Snapshot::from_native(&data).unwrap().sessions[0].title,
            "Task | otherrepo"
        );
    }

    #[test]
    fn all_panes_include_plain_and_unnamed_agents_without_provider_refs() {
        let mut data = native();
        let mut pane = data["panes"][0].clone();
        pane["pane_id"] = json!("plain");
        pane["terminal_id"] = json!("term-b");
        pane["agent"] = Value::Null;
        pane["agent_status"] = json!("unknown");
        data["panes"].as_array_mut().unwrap().push(pane);
        let mut pane = data["panes"][0].clone();
        pane["pane_id"] = json!("unnamed");
        pane["terminal_id"] = json!("term-c");
        data["panes"].as_array_mut().unwrap().push(pane);
        let model = Snapshot::from_native(&data).unwrap();
        assert_eq!(model.sessions.len(), 3);
        assert_eq!(model.sessions[1].kind, "terminal");
        assert_eq!(model.sessions[1].status, "unknown");
        assert!(
            model
                .sessions
                .iter()
                .all(|s| s.native_reference.is_none() && s.split_count == 3)
        );
        assert_eq!(model.sessions[2].split_index, 3);
    }

    #[test]
    fn malformed_graph_and_empty_ids_are_rejected() {
        for (table, key, value) in [
            ("tabs", "workspace_id", "missing"),
            ("panes", "tab_id", "missing"),
            ("panes", "workspace_id", "missing"),
            ("agents", "terminal_id", "replacement"),
            ("agents", "pane_id", "missing"),
            ("panes", "terminal_id", ""),
        ] {
            let mut data = native();
            data[table][0][key] = json!(value);
            assert_eq!(
                Snapshot::from_native(&data).unwrap_err().code,
                "invalid_snapshot"
            );
        }
        for table in ["workspaces", "tabs", "panes", "agents"] {
            let mut data = native();
            let duplicate = data[table][0].clone();
            data[table].as_array_mut().unwrap().push(duplicate);
            assert!(Snapshot::from_native(&data).is_err());
        }
        let mut data = native();
        data["focused_pane_id"] = json!([]);
        assert!(Snapshot::from_native(&data).is_err());
    }

    #[test]
    fn empty_display_labels_never_fall_back_to_opaque_ids() {
        let mut data = native();
        data["workspaces"][0]["label"] = json!("");
        data["tabs"][0]["label"] = json!("1");
        data["agents"] = json!([]);
        data["panes"][0]["agent"] = Value::Null;
        let model = Snapshot::from_native(&data).unwrap();
        assert_eq!(model.workspaces[0].label, "Workspace");
        assert_eq!(model.sessions[0].title, "Terminal");
    }

    #[test]
    fn native_membership_and_order_override_address_prefixes() {
        let mut data = native();
        data["workspaces"].as_array_mut().unwrap().push(json!({
            "workspace_id": "ws-a",
            "label": "second",
        }));
        data["tabs"].as_array_mut().unwrap().insert(
            0,
            json!({
                "tab_id": "looks-like-ws-z",
                "workspace_id": "ws-a",
                "label": "second",
            }),
        );
        let mut pane = data["panes"][0].clone();
        pane["workspace_id"] = json!("ws-a");
        pane["tab_id"] = json!("looks-like-ws-z");
        pane["pane_id"] = json!("ws-z:p99");
        pane["terminal_id"] = json!("term-b");
        data["panes"].as_array_mut().unwrap().insert(0, pane);
        let model = Snapshot::from_native(&data).unwrap();
        assert_eq!(model.sessions[0].terminal_id, "term-a");
        assert_eq!(model.sessions[1].workspace_id, "ws-a");
    }

    #[test]
    fn identity_survives_moves_but_not_replacement_or_provider_change() {
        let original = Snapshot::from_native(&native()).unwrap().sessions.remove(0);
        let mut moved = original.clone();
        moved.pane_id = "new-address".into();
        moved.workspace_id = "other-workspace".into();
        moved.native_reference = Some(("codex".into(), "id".into(), "now-known".into()));
        let mut snapshot = Snapshot {
            sessions: vec![moved.clone()],
            ..Snapshot::default()
        };
        assert_eq!(snapshot.focus_target(&original).unwrap(), &moved);
        snapshot.sessions[0].terminal_id = "replacement".into();
        assert_eq!(
            snapshot.focus_target(&original).unwrap_err().code,
            "missing_terminal"
        );
        snapshot.sessions[0] = moved.clone();
        snapshot.sessions[0].native_reference = None;
        assert_eq!(
            snapshot.focus_target(&moved).unwrap_err().code,
            "terminal_changed"
        );
    }

    #[test]
    fn native_status_revision_and_unknown_dates_are_not_inferred() {
        for status in ["working", "blocked", "done", "idle", "unknown", "future"] {
            let mut data = native();
            data["agents"][0]["agent_status"] = json!(status);
            data["panes"][0]["revision"] = json!(44);
            let model = Snapshot::from_native(&data).unwrap();
            assert_eq!(
                model.sessions[0].status,
                if status == "future" {
                    "unknown"
                } else {
                    status
                }
            );
            assert_eq!(model.sessions[0].revision, 44);
            assert!(model.sessions[0].activity_at.is_none());
        }
    }
}
