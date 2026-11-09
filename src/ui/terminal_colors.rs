use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalColors {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub yellow: Option<[u8; 3]>,
    pub cyan: Option<[u8; 3]>,
}

/// Crossterm 0.29 decodes OSC introducers and terminators as Alt-key events.
/// Keep one input reader and consume complete OSC 4/10/11 frames, including
/// unsupported color formats, so protocol bytes never become search text.
pub struct ColorProbe {
    enabled: bool,
    pending: Vec<Event>,
    oversized: bool,
    since: Instant,
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
    yellow: Option<[u8; 3]>,
    cyan: Option<[u8; 3]>,
}

impl ColorProbe {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: Vec::new(),
            oversized: false,
            since: Instant::now(),
            foreground: None,
            background: None,
            yellow: None,
            cyan: None,
        }
    }

    pub fn request(&mut self, output: &mut impl Write) {
        if self.enabled
            && output
                .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]4;3;?\x1b\\\x1b]4;6;?\x1b\\")
                .and_then(|()| output.flush())
                .is_err()
        {
            self.enabled = false;
        }
    }

    pub fn expire(&mut self, now: Instant) -> Vec<Event> {
        let limit = if self.pending.len() == 1 && self.pending[0] == Event::Key(KeyCode::Esc.into())
        {
            Duration::from_millis(12)
        } else {
            Duration::from_millis(150)
        };
        if !self.pending.is_empty() && now.duration_since(self.since) >= limit {
            // An interrupted color reply must not replay its Escape as a close command.
            let protocol = decoded_reply(&self.pending)
                .0
                .is_some_and(|payload| reply_payload(&payload));
            self.oversized = false;
            let pending = std::mem::take(&mut self.pending);
            if protocol { Vec::new() } else { pending }
        } else {
            Vec::new()
        }
    }

    pub fn feed(&mut self, event: Event, now: Instant) -> (Vec<Event>, Option<TerminalColors>) {
        let mut ready = self.expire(now);
        if !self.enabled {
            ready.push(event);
            return (ready, None);
        }
        if self.pending.is_empty() {
            if self.starts_reply(&event) {
                self.since = now;
                self.pending.push(event);
            } else {
                ready.push(event);
            }
            return (ready, None);
        }
        self.pending.push(event);
        let (payload, complete) = decoded_reply(&self.pending);
        if let Some(payload) = payload {
            if complete {
                if !self.oversized
                    && let Some((kind, color)) = parse_color(&payload)
                {
                    match kind {
                        ColorTarget::Foreground => self.foreground = Some(color),
                        ColorTarget::Background => self.background = Some(color),
                        ColorTarget::Yellow => self.yellow = Some(color),
                        ColorTarget::Cyan => self.cyan = Some(color),
                    }
                    self.pending.clear();
                    return (
                        ready,
                        self.foreground
                            .zip(self.background)
                            .map(|(foreground, background)| TerminalColors {
                                foreground,
                                background,
                                yellow: self.yellow,
                                cyan: self.cyan,
                            }),
                    );
                }
                if reply_payload(&payload) {
                    self.pending.clear();
                    self.oversized = false;
                    return (ready, None);
                }
            } else if valid_prefix(&payload) {
                if self.pending.len() >= 48 {
                    // Keep the header and a split terminator while bounding malformed frames.
                    self.oversized = true;
                    let escape = self
                        .pending
                        .last()
                        .is_some_and(|event| is_key(event, KeyCode::Esc, KeyModifiers::NONE));
                    let start = if is_key(&self.pending[0], KeyCode::Esc, KeyModifiers::NONE) {
                        2
                    } else {
                        1
                    };
                    self.pending.truncate(start + 3);
                    if escape {
                        self.pending.push(Event::Key(KeyCode::Esc.into()));
                    }
                }
                return (ready, None);
            }
        }
        let last = self.pending.pop().expect("the current event is buffered");
        ready.append(&mut self.pending);
        self.oversized = false;
        if self.starts_reply(&last) {
            self.since = now;
            self.pending.push(last);
        } else {
            ready.push(last);
        }
        (ready, None)
    }

    fn starts_reply(&self, event: &Event) -> bool {
        // Optional palette replies can arrive after default colors with a split introducer.
        is_key(event, KeyCode::Char(']'), KeyModifiers::ALT)
            || is_key(event, KeyCode::Esc, KeyModifiers::NONE)
    }
}

fn is_key(event: &Event, code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(event, Event::Key(key) if key.code == code && key.modifiers == modifiers && key.kind == KeyEventKind::Press)
}

fn decoded_reply(events: &[Event]) -> (Option<String>, bool) {
    let mut start = 1;
    if is_key(&events[0], KeyCode::Esc, KeyModifiers::NONE) {
        if events.len() < 2 || !is_key(&events[1], KeyCode::Char(']'), KeyModifiers::NONE) {
            return (None, false);
        }
        start = 2;
    }
    let mut payload = String::new();
    for (index, event) in events.iter().enumerate().skip(start) {
        if index + 1 == events.len()
            && (is_key(event, KeyCode::Char('g'), KeyModifiers::CONTROL)
                || is_key(event, KeyCode::Char('\\'), KeyModifiers::ALT))
        {
            return (Some(payload), true);
        }
        if is_key(event, KeyCode::Esc, KeyModifiers::NONE) {
            if index + 1 == events.len() {
                return (Some(payload), false);
            }
            if index + 2 == events.len()
                && is_key(&events[index + 1], KeyCode::Char('\\'), KeyModifiers::NONE)
            {
                return (Some(payload), true);
            }
            return (None, false);
        }
        match event {
            Event::Key(KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            }) if *modifiers == KeyModifiers::NONE || *modifiers == KeyModifiers::SHIFT => {
                payload.push(*ch)
            }
            _ => return (None, false),
        }
    }
    (Some(payload), false)
}

fn valid_prefix(payload: &str) -> bool {
    ["4;", "10;", "11;"]
        .iter()
        .any(|header| header.starts_with(payload) || payload.starts_with(header))
}

fn reply_payload(payload: &str) -> bool {
    ["4;", "10;", "11;"]
        .iter()
        .any(|header| payload.starts_with(header))
}

enum ColorTarget {
    Foreground,
    Background,
    Yellow,
    Cyan,
}

fn parse_color(payload: &str) -> Option<(ColorTarget, [u8; 3])> {
    let (kind, rgb) = payload.split_once(";rgb:")?;
    let kind = match kind {
        "10" => ColorTarget::Foreground,
        "11" => ColorTarget::Background,
        "4;3" => ColorTarget::Yellow,
        "4;6" => ColorTarget::Cyan,
        _ => return None,
    };
    let parts: Vec<_> = rgb.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut color = [0; 3];
    for (component, text) in color.iter_mut().zip(parts) {
        if text.is_empty() || text.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(text, 16).ok()?;
        let max = (1u32 << (text.len() * 4)) - 1;
        *component = ((value * 255 + max / 2) / max) as u8;
    }
    Some((kind, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(payload: &str, split: bool) -> Vec<Event> {
        let mut events = if split {
            vec![
                Event::Key(KeyCode::Esc.into()),
                Event::Key(KeyCode::Char(']').into()),
            ]
        } else {
            vec![Event::Key(KeyEvent::new(
                KeyCode::Char(']'),
                KeyModifiers::ALT,
            ))]
        };
        events.extend(
            payload
                .chars()
                .map(|ch| Event::Key(KeyCode::Char(ch).into())),
        );
        if split {
            events.extend([
                Event::Key(KeyCode::Esc.into()),
                Event::Key(KeyCode::Char('\\').into()),
            ]);
        } else {
            events.push(Event::Key(KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL,
            )));
        }
        events
    }

    fn consume_reply(
        probe: &mut ColorProbe,
        payload: &str,
        split: bool,
        now: Instant,
    ) -> Option<TerminalColors> {
        let mut palette = None;
        for event in reply(payload, split) {
            let (ready, colors) = probe.feed(event, now);
            assert!(ready.is_empty(), "protocol input escaped: {payload}");
            palette = colors.or(palette);
        }
        palette
    }

    #[test]
    fn palette_and_default_colors_work_in_every_reply_order() {
        let replies = [
            "10;rgb:eeee/eeee/eeee",
            "11;rgb:1111/1111/1111",
            "4;3;rgb:dddd/aaaa/4444",
            "4;6;rgb:4444/bbbb/cccc",
        ];
        for split in [false, true] {
            for first in 0..4 {
                for second in 0..4 {
                    if second == first {
                        continue;
                    }
                    for third in 0..4 {
                        if third == first || third == second {
                            continue;
                        }
                        let fourth = (0..4)
                            .find(|index| ![first, second, third].contains(index))
                            .unwrap();
                        let mut probe = ColorProbe::new(true);
                        let mut palette = None;
                        for index in [first, second, third, fourth] {
                            palette =
                                consume_reply(&mut probe, replies[index], split, Instant::now())
                                    .or(palette);
                        }
                        assert_eq!(
                            palette,
                            Some(TerminalColors {
                                foreground: [238; 3],
                                background: [17; 3],
                                yellow: Some([221, 170, 68]),
                                cyan: Some([68, 187, 204]),
                            })
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn late_optional_colors_update_the_palette_without_leaking_escape() {
        let mut probe = ColorProbe::new(true);
        let now = Instant::now();
        assert!(consume_reply(&mut probe, "10;rgb:ff/ff/ff", false, now).is_none());
        let initial = consume_reply(&mut probe, "11;rgb:00/00/00", false, now).unwrap();
        assert!(initial.yellow.is_none() && initial.cyan.is_none());
        let later = now + Duration::from_secs(5);
        let yellow = consume_reply(&mut probe, "4;3;rgb:dd/aa/44", true, later).unwrap();
        assert_eq!(yellow.yellow, Some([221, 170, 68]));
        assert!(yellow.cyan.is_none());
        let cyan = consume_reply(&mut probe, "4;6;rgb:44/bb/cc", true, later).unwrap();
        assert_eq!(cyan.yellow, yellow.yellow);
        assert_eq!(cyan.cyan, Some([68, 187, 204]));
        let typed = Event::Key(KeyCode::Char('x').into());
        assert_eq!(probe.feed(typed.clone(), later).0, vec![typed]);
    }

    #[test]
    fn malformed_and_oversized_palette_frames_are_consumed_without_keys() {
        for split in [false, true] {
            let mut probe = ColorProbe::new(true);
            for payload in [
                "4;3;unknown",
                "4;6;rgb:zz/00/00",
                "4;3;rgb:ff/00",
                "4;6;rgb:fffff/0/0",
                "4;255;rgb:ff/00/00",
                "4;6;색상",
            ] {
                assert!(consume_reply(&mut probe, payload, split, Instant::now()).is_none());
            }
            assert!(
                consume_reply(
                    &mut probe,
                    &format!("4;3;{}", "x".repeat(400)),
                    split,
                    Instant::now()
                )
                .is_none()
            );
            let paste = Event::Paste("ordinary query".into());
            assert_eq!(probe.feed(paste.clone(), Instant::now()).0, vec![paste]);
        }
    }

    #[test]
    fn unterminated_protocol_prefix_times_out_without_becoming_search_input() {
        let mut probe = ColorProbe::new(true);
        let now = Instant::now();
        let mut events = reply("4;6;rgb:00", false);
        events.pop();
        for event in events {
            assert!(probe.feed(event, now).0.is_empty());
        }
        assert!(probe.expire(now + Duration::from_millis(151)).is_empty());
        let key = Event::Key(KeyCode::Char('q').into());
        assert_eq!(
            probe.feed(key.clone(), now + Duration::from_millis(152)).0,
            vec![key]
        );
    }

    #[test]
    fn split_color_replies_and_surrounding_input_preserve_keys_and_paste() {
        for split in [true, false] {
            let mut probe = ColorProbe::new(true);
            let ordinary = vec![
                Event::Key(KeyCode::Char('/').into()),
                Event::Paste("a\u{754c}".into()),
                Event::Key(KeyCode::Left.into()),
            ];
            let mut input = ordinary.clone();
            input.extend(reply("10;rgb:aaaa/bbbb/cccc", split));
            input.push(Event::Key(KeyCode::Down.into()));
            input.extend(reply("11;rgb:12/34/56", split));
            let mut ready = Vec::new();
            let mut palette = None;
            for event in input {
                let (events, colors) = probe.feed(event, Instant::now());
                ready.extend(events);
                palette = colors.or(palette);
            }
            assert_eq!(
                ready,
                [ordinary, vec![Event::Key(KeyCode::Down.into())]].concat()
            );
            assert_eq!(
                palette,
                Some(TerminalColors {
                    foreground: [170, 187, 204],
                    background: [18, 52, 86],
                    yellow: None,
                    cyan: None,
                })
            );
        }
    }

    #[test]
    fn ordinary_alt_keys_malformed_packets_and_timeouts_are_replayed() {
        for events in [
            reply("12;rgb:xyz/12/34", false),
            vec![
                Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT)),
                Event::Key(KeyCode::Char('q').into()),
            ],
        ] {
            let mut probe = ColorProbe::new(true);
            let now = Instant::now();
            let mut ready = Vec::new();
            for event in events.clone() {
                ready.extend(probe.feed(event, now).0);
            }
            ready.extend(probe.expire(now + Duration::from_millis(151)));
            assert_eq!(ready, events);
        }
        let mut probe = ColorProbe::new(true);
        let now = Instant::now();
        probe.feed(Event::Key(KeyCode::Esc.into()), now);
        assert_eq!(
            probe.expire(now + Duration::from_millis(12)),
            vec![Event::Key(KeyCode::Esc.into())]
        );
    }

    #[test]
    fn late_replies_are_consumed_without_a_startup_wait_and_disabled_probe_is_silent() {
        let mut probe = ColorProbe::new(false);
        let mut output = Vec::new();
        probe.request(&mut output);
        assert!(output.is_empty());
        let keys = reply("11;rgb:0/0/0", false);
        for event in keys {
            assert_eq!(probe.feed(event.clone(), Instant::now()).0, vec![event]);
        }
        let mut probe = ColorProbe::new(true);
        probe.request(&mut output);
        assert_eq!(
            output,
            b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]4;3;?\x1b\\\x1b]4;6;?\x1b\\"
        );
        let later = Instant::now() + Duration::from_secs(5);
        for event in reply("11;rgb:0/0/0", false) {
            assert!(probe.feed(event, later).0.is_empty());
        }
    }

    #[test]
    fn unsupported_color_formats_are_not_forwarded_as_search_text() {
        let mut probe = ColorProbe::new(true);
        for event in reply("11;not-a-color", true) {
            let (events, colors) = probe.feed(event, Instant::now());
            assert!(events.is_empty());
            assert!(colors.is_none());
        }
    }

    #[test]
    fn an_alt_key_before_a_reply_and_escape_after_colors_are_preserved() {
        let mut probe = ColorProbe::new(true);
        let alt = Event::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
        let mut ready = probe.feed(alt.clone(), Instant::now()).0;
        for body in ["10;rgb:fff/fff/fff", "11;rgb:0/0/0"] {
            for event in reply(body, false) {
                ready.extend(probe.feed(event, Instant::now()).0);
            }
        }
        assert_eq!(ready, vec![alt]);
        let escape = Event::Key(KeyCode::Esc.into());
        let now = Instant::now();
        assert!(probe.feed(escape.clone(), now).0.is_empty());
        assert_eq!(probe.expire(now + Duration::from_millis(12)), vec![escape]);
    }
}
