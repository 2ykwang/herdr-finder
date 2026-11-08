use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalColors {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
}

/// Crossterm 0.29 decodes OSC introducers and terminators as Alt-key events.
/// Keep one input reader and consume complete OSC 10/11 frames, including
/// unsupported color formats, so protocol bytes never become search text.
pub struct ColorProbe {
    enabled: bool,
    pending: Vec<Event>,
    since: Instant,
    foreground: Option<[u8; 3]>,
    background: Option<[u8; 3]>,
}

impl ColorProbe {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: Vec::new(),
            since: Instant::now(),
            foreground: None,
            background: None,
        }
    }

    pub fn request(&mut self, output: &mut impl Write) {
        if self.enabled
            && output
                .write_all(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\")
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
            std::mem::take(&mut self.pending)
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
                if let Some((kind, color)) = parse_color(&payload) {
                    if kind == 10 {
                        self.foreground = Some(color);
                    } else {
                        self.background = Some(color);
                    }
                    self.pending.clear();
                    return (
                        ready,
                        self.foreground
                            .zip(self.background)
                            .map(|(foreground, background)| TerminalColors {
                                foreground,
                                background,
                            }),
                    );
                }
                if payload.starts_with("10;") || payload.starts_with("11;") {
                    self.pending.clear();
                    return (ready, None);
                }
            } else if valid_prefix(&payload) && self.pending.len() < 48 {
                return (ready, None);
            }
        }
        let last = self.pending.pop().expect("the current event is buffered");
        ready.append(&mut self.pending);
        if self.starts_reply(&last) {
            self.since = now;
            self.pending.push(last);
        } else {
            ready.push(last);
        }
        (ready, None)
    }

    fn starts_reply(&self, event: &Event) -> bool {
        is_key(event, KeyCode::Char(']'), KeyModifiers::ALT)
            || ((self.foreground.is_none() || self.background.is_none())
                && is_key(event, KeyCode::Esc, KeyModifiers::NONE))
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
            }) if ch.is_ascii()
                && (*modifiers == KeyModifiers::NONE || *modifiers == KeyModifiers::SHIFT) =>
            {
                payload.push(*ch)
            }
            _ => return (None, false),
        }
    }
    (Some(payload), false)
}

fn valid_prefix(payload: &str) -> bool {
    ["10;", "11;"]
        .iter()
        .any(|header| header.starts_with(payload) || payload.starts_with(header))
}

fn parse_color(payload: &str) -> Option<(u8, [u8; 3])> {
    let (kind, rgb) = payload.split_once(";rgb:")?;
    let kind = match kind {
        "10" => 10,
        "11" => 11,
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
                    background: [18, 52, 86]
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
        assert_eq!(output, b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
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
    fn an_alt_key_before_a_reply_is_preserved_and_escape_is_immediate_after_both_replies() {
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
        assert_eq!(probe.feed(escape.clone(), Instant::now()).0, vec![escape]);
    }
}
