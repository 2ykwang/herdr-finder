mod help;
mod input;
mod messages;
mod render;
mod terminal_colors;
mod theme;

use chrono::Local;
use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Modifier,
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{
    collections::HashMap,
    error::Error,
    io::{self, IsTerminal},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    activity::{ActivityReader, ActivityStamp},
    client::Client,
    model::{FinderError, Session, Snapshot},
    search::ContentDocument,
    state::{FinderState, GroupLevel, RowKind},
};
use input::Editor;
use messages::Messages;
use render::VisualLine;
use terminal_colors::ColorProbe;
use theme::Theme;

enum Update {
    Snapshot(Result<Snapshot, FinderError>),
    Contents(Vec<Session>, HashMap<String, ContentDocument>),
    Activity(Vec<Session>, HashMap<String, ActivityStamp>),
    Focus(Result<(), FinderError>),
}

#[derive(Default)]
struct Geometry {
    search: Rect,
    input: Rect,
    input_start: usize,
    list: Rect,
}

struct IndexedScreen {
    pane_id: String,
    kind: String,
    reference: Option<(String, String, String)>,
    revision: u64,
    read_at: Instant,
}

struct CachedActivity {
    kind: String,
    reference: Option<(String, String, String)>,
    stamp: ActivityStamp,
}

struct App {
    client: Client,
    state: FinderState,
    editor: Editor,
    messages: Messages,
    theme: Theme,

    search_focus: bool,
    help: bool,
    help_scroll: u16,
    quit: bool,

    dirty: bool,
    rebuild: bool,
    reveal: bool,
    lines: Vec<VisualLine>,
    line_width: u16,
    scroll: usize,
    geometry: Geometry,

    error: String,
    content_error: bool,
    opening: bool,

    snapshot_busy: bool,
    index_busy: bool,
    activity_busy: bool,
    next_snapshot: Instant,
    index_due: Option<Instant>,

    indexed: HashMap<String, IndexedScreen>,
    stamps: HashMap<String, CachedActivity>,
    reader: Arc<Mutex<ActivityReader>>,

    sender: mpsc::Sender<Update>,
    receiver: mpsc::Receiver<Update>,
    last_click: Option<(String, u16, u16, Instant)>,
}

impl App {
    fn new(client: Client, prefetch: bool) -> Self {
        let (sender, receiver) = mpsc::channel();
        let reader = ActivityReader::from_env().with_cancel(client.cancellation());
        let mut app = Self {
            client,
            state: FinderState::default(),
            editor: Editor::default(),
            messages: Messages,
            theme: Theme::from_env(),

            search_focus: false,
            help: false,
            help_scroll: 0,
            quit: false,

            dirty: true,
            rebuild: true,
            reveal: true,
            lines: Vec::new(),
            line_width: 0,
            scroll: 0,
            geometry: Geometry::default(),

            error: String::new(),
            content_error: false,
            opening: false,

            snapshot_busy: false,
            index_busy: false,
            activity_busy: false,
            next_snapshot: Instant::now() + Duration::from_secs(2),
            index_due: None,

            indexed: HashMap::new(),
            stamps: HashMap::new(),
            reader: Arc::new(Mutex::new(reader)),

            sender,
            receiver,
            last_click: None,
        };
        if prefetch {
            app.request_snapshot();
        }
        app
    }

    fn request_snapshot(&mut self) {
        if self.snapshot_busy || self.opening {
            return;
        }
        self.snapshot_busy = true;
        let client = self.client.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let _ = sender.send(Update::Snapshot(client.snapshot()));
        });
    }

    fn enrich(&self, mut snapshot: Snapshot) -> Snapshot {
        for session in &mut snapshot.sessions {
            if let Some(cached) = self.stamps.get(&session.terminal_id)
                && cached.kind == session.kind
                && cached.reference == session.native_reference
            {
                session.activity_at = Some(cached.stamp.at);
                session.activity_source = cached.stamp.source.clone();
            }
        }
        snapshot
    }

    fn tick(&mut self) {
        while let Ok(update) = self.receiver.try_recv() {
            self.receive(update);
        }

        let now = Instant::now();
        if now >= self.next_snapshot {
            self.next_snapshot = now + Duration::from_secs(2);
            self.request_snapshot();
        }

        if self.index_due.is_some_and(|due| due <= now) && !self.index_busy {
            self.index_due = None;
            self.request_contents();
        }
    }

    fn receive(&mut self, update: Update) {
        match update {
            Update::Snapshot(result) => {
                self.snapshot_busy = false;
                match result {
                    Ok(snapshot) => {
                        let initial = self.state.snapshot.is_none();
                        self.indexed
                            .retain(|id, _| snapshot.sessions.iter().any(|s| &s.terminal_id == id));
                        self.stamps
                            .retain(|id, _| snapshot.sessions.iter().any(|s| &s.terminal_id == id));
                        let snapshot = self.enrich(snapshot);
                        self.state.update(snapshot);
                        self.error.clear();
                        self.reveal |= initial;
                        self.rebuild = true;
                        self.request_activity();
                        if !self.state.query.trim().is_empty() {
                            self.index_due = Some(Instant::now());
                        }
                    }
                    Err(error) => self.error = error.to_string(),
                }
            }
            Update::Contents(origin, documents) => {
                self.index_busy = false;
                self.content_error = documents.len() != origin.len();
                if let Some(snapshot) = &self.state.snapshot {
                    for before in &origin {
                        if documents.contains_key(&before.terminal_id)
                            && snapshot.sessions.iter().any(|s| {
                                s.terminal_id == before.terminal_id
                                    && s.kind == before.kind
                                    && s.native_reference == before.native_reference
                            })
                        {
                            self.indexed.insert(
                                before.terminal_id.clone(),
                                IndexedScreen {
                                    pane_id: before.pane_id.clone(),
                                    kind: before.kind.clone(),
                                    reference: before.native_reference.clone(),
                                    revision: before.revision,
                                    read_at: Instant::now(),
                                },
                            );
                        }
                    }
                }
                self.rebuild |= self.state.set_contents(documents, &origin);
            }
            Update::Activity(origin, stamps) => {
                self.activity_busy = false;
                if let Some(snapshot) = &self.state.snapshot {
                    for before in &origin {
                        if let Some(stamp) = stamps.get(&before.terminal_id)
                            && snapshot.sessions.iter().any(|s| {
                                s.terminal_id == before.terminal_id
                                    && s.kind == before.kind
                                    && s.native_reference == before.native_reference
                            })
                        {
                            self.stamps.insert(
                                before.terminal_id.clone(),
                                CachedActivity {
                                    kind: before.kind.clone(),
                                    reference: before.native_reference.clone(),
                                    stamp: stamp.clone(),
                                },
                            );
                        }
                    }
                    let enriched = self.enrich(snapshot.clone());
                    if self.state.snapshot.as_ref() != Some(&enriched) {
                        self.state.update(enriched);
                        self.rebuild = true;
                    }
                }
            }
            Update::Focus(result) => {
                self.opening = false;
                match result {
                    Ok(()) => self.quit = true,
                    Err(error) => self.error = error.to_string(),
                }
            }
        }
        self.dirty = true;
    }

    fn request_activity(&mut self) {
        if self.activity_busy {
            return;
        }
        let Some(snapshot) = &self.state.snapshot else {
            return;
        };
        let sessions: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|s| {
                s.native_reference.is_some() && matches!(s.kind.as_str(), "claude" | "codex")
            })
            .cloned()
            .collect();
        if sessions.is_empty() {
            return;
        }
        self.activity_busy = true;
        let reader = self.reader.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let stamps = reader
                .lock()
                .map(|mut reader| reader.read(&sessions))
                .unwrap_or_default();
            let _ = sender.send(Update::Activity(sessions, stamps));
        });
    }

    fn request_contents(&mut self) {
        if self.index_busy || self.opening || self.state.query.trim().is_empty() {
            return;
        }
        let Some(snapshot) = &self.state.snapshot else {
            return;
        };
        let sessions: Vec<_> = snapshot
            .sessions
            .iter()
            .filter(|s| {
                self.indexed.get(&s.terminal_id).is_none_or(|cached| {
                    cached.pane_id != s.pane_id
                        || cached.kind != s.kind
                        || cached.reference != s.native_reference
                        || cached.revision != s.revision
                        || cached.read_at.elapsed() >= Duration::from_secs(5)
                })
            })
            .cloned()
            .collect();
        if sessions.is_empty() {
            return;
        }
        self.index_busy = true;
        self.content_error = false;
        self.dirty = true;
        let client = self.client.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let documents = client
                .read_contents(&sessions)
                .into_iter()
                .map(|(id, text)| (id, ContentDocument::new(text)))
                .collect();
            let _ = sender.send(Update::Contents(sessions, documents));
        });
    }

    fn query_changed(&mut self) {
        self.state.set_query(self.editor.text.clone());
        self.index_due = (!self.editor.text.trim().is_empty())
            .then(|| Instant::now() + Duration::from_millis(100));
        self.rebuild = true;
        self.reveal = true;
        self.dirty = true;
    }

    fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
        self.reveal = true;
        self.dirty = true;
    }

    fn move_group(&mut self, delta: isize, level: GroupLevel) {
        self.state.move_target(delta, level);
        self.reveal = true;
        self.dirty = true;
    }

    fn activate(&mut self) {
        if self.opening {
            return;
        }
        let Some(row) = self.state.selected() else {
            return;
        };
        if row.kind != RowKind::Session {
            self.state.toggle_group(None);
            self.rebuild = true;
            self.reveal = true;
            self.dirty = true;
            return;
        }
        let Some(session) = row.session.clone() else {
            return;
        };
        self.opening = true;
        self.dirty = true;
        let client = self.client.clone();
        let sender = self.sender.clone();
        thread::spawn(move || {
            let _ = sender.send(Update::Focus(client.focus(&session)));
        });
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.key(key),
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::Paste(text) if self.search_focus && !self.opening && !self.help => {
                self.editor.insert(&text);
                self.query_changed();
            }
            Event::Resize(_, _) => {
                self.rebuild = true;
                self.reveal = true;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if !key.is_press() && key.kind != event::KeyEventKind::Repeat {
            return;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if key.kind == event::KeyEventKind::Repeat
            && (matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                || (!self.search_focus && key.code == KeyCode::Char(' ')))
        {
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) => self.help = false,
                KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                _ => {}
            }
            self.dirty = true;
            return;
        }
        match key.code {
            KeyCode::Esc => {
                if self.search_focus && !self.opening {
                    self.search_focus = false;
                    self.dirty = true;
                } else {
                    self.quit = true;
                }
                return;
            }
            KeyCode::F(1) => {
                self.help = true;
                self.help_scroll = 0;
                self.dirty = true;
                return;
            }
            KeyCode::Char('c' | 'q') if control => {
                self.quit = true;
                return;
            }
            KeyCode::Char('r') if control => {
                self.indexed.clear();
                self.index_due = Some(Instant::now());
                self.request_snapshot();
                return;
            }
            _ => {}
        }
        if self.opening {
            return;
        }
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char('p') if control => self.move_selection(-1),
            KeyCode::Char('n') if control => self.move_selection(1),
            KeyCode::Enter => self.activate(),
            KeyCode::PageUp => self.move_selection(-(self.geometry.list.height.max(1) as isize)),
            KeyCode::PageDown => self.move_selection(self.geometry.list.height.max(1) as isize),
            _ if self.search_focus => {
                let before = self.editor.text.clone();
                match key.code {
                    KeyCode::Left if control || alt => self.editor.word_left(),
                    KeyCode::Right if control || alt => self.editor.word_right(),
                    KeyCode::Left => self.editor.left(),
                    KeyCode::Right => self.editor.right(),
                    KeyCode::Home => self.editor.cursor = 0,
                    KeyCode::End => self.editor.cursor = self.editor.text.len(),
                    KeyCode::Char('a') if control => self.editor.cursor = 0,
                    KeyCode::Char('e') if control => self.editor.cursor = self.editor.text.len(),
                    KeyCode::Char('u') if control => self.editor.clear_left(),
                    KeyCode::Char('k') if control => self.editor.clear_right(),
                    KeyCode::Char('w') if control => {
                        let end = self.editor.cursor;
                        self.editor.word_left();
                        self.editor.text.replace_range(self.editor.cursor..end, "");
                    }
                    KeyCode::Backspace => self.editor.backspace(),
                    KeyCode::Delete => self.editor.delete(),
                    KeyCode::Char(c) if !control && !alt => self.editor.insert(&c.to_string()),
                    _ => {}
                }
                if before != self.editor.text {
                    self.query_changed();
                }
            }
            KeyCode::Left if alt => self.move_group(-1, GroupLevel::Workspace),
            KeyCode::Right if alt => self.move_group(1, GroupLevel::Workspace),
            KeyCode::Left => self.move_group(-1, GroupLevel::Tab),
            KeyCode::Right => self.move_group(1, GroupLevel::Tab),
            KeyCode::Char(' ') => {
                self.state.toggle_group(None);
                self.rebuild = true;
                self.reveal = true;
            }
            KeyCode::Char('/') => self.search_focus = true,
            KeyCode::Home | KeyCode::End => {
                let filtering = self.state.is_filtering();
                let row = if key.code == KeyCode::Home {
                    self.state
                        .rows()
                        .iter()
                        .find(|row| !filtering || row.kind == RowKind::Session)
                } else {
                    self.state
                        .rows()
                        .iter()
                        .rev()
                        .find(|row| !filtering || row.kind == RowKind::Session)
                };
                if let Some(key) = row.map(|r| r.key.clone()) {
                    self.state.select(&key);
                    self.reveal = true;
                }
            }
            KeyCode::Char(c @ ('a' | 'b' | 'w' | 'd' | 'i')) if !control && !alt => {
                self.state.set_status(match c {
                    'b' => Some("blocked".to_owned()),
                    'w' => Some("working".to_owned()),
                    'd' => Some("done".to_owned()),
                    'i' => Some("idle".to_owned()),
                    _ => None,
                });
                self.rebuild = true;
                self.reveal = true;
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if self.help || self.opening {
            return;
        }
        let point = (mouse.column, mouse.row);
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.geometry.search.contains(point.into())
        {
            self.search_focus = true;
            let cells = point.0.saturating_sub(self.geometry.input.x) as usize;
            let start = self
                .editor
                .text
                .grapheme_indices(true)
                .map(|(index, _)| index)
                .chain(std::iter::once(self.editor.text.len()))
                .take_while(|index| *index <= self.geometry.input_start)
                .last()
                .unwrap_or(0);
            let tail = &self.editor.text[start..];
            let prefix = render::crop(tail, cells);
            self.editor.cursor = start + prefix.len();
            self.dirty = true;
            return;
        }
        if !self.geometry.list.contains(point.into()) {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + 3).min(
                    self.lines
                        .len()
                        .saturating_sub(self.geometry.list.height as usize),
                )
            }
            MouseEventKind::Down(button @ (MouseButton::Left | MouseButton::Right)) => {
                let index = self.scroll + point.1.saturating_sub(self.geometry.list.y) as usize;
                let Some(key) = self.lines.get(index).and_then(|line| line.key.clone()) else {
                    return;
                };
                if !self.state.select(&key) {
                    return;
                }
                self.search_focus = false;
                if button == MouseButton::Right {
                    self.state.select_parent();
                    self.rebuild = true;
                    self.reveal = true;
                    self.last_click = None;
                } else {
                    let double = self.last_click.as_ref().is_some_and(|(old, x, y, at)| {
                        old == &key
                            && *x == point.0
                            && *y == point.1
                            && at.elapsed() < Duration::from_millis(500)
                    });
                    self.last_click = Some((key, point.0, point.1, Instant::now()));
                    if self
                        .state
                        .selected()
                        .is_some_and(|row| row.kind != RowKind::Session)
                    {
                        if !double {
                            self.state.toggle_group(None);
                            self.rebuild = true;
                            self.reveal = true;
                        }
                    } else if double {
                        self.activate();
                    } else {
                        self.reveal = true;
                    }
                }
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn notice(&self) -> String {
        if self.opening {
            self.messages.get("opening").to_owned()
        } else if !self.error.is_empty() {
            self.error.clone()
        } else if self.state.selection_lost {
            self.messages.get("lost").to_owned()
        } else if !self.state.query.trim().is_empty() && self.index_busy {
            self.messages.get("indexing").to_owned()
        } else if !self.state.query.trim().is_empty() && self.content_error {
            self.messages.get("partial").to_owned()
        } else {
            String::new()
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let notice = self.notice();
        let selected_title = self
            .state
            .selected()
            .map(|row| {
                row.session
                    .as_ref()
                    .map(|session| session.title.as_str())
                    .unwrap_or_else(|| {
                        if row.kind == RowKind::Tab {
                            &row.tab_label
                        } else {
                            &row.workspace.label
                        }
                    })
            })
            .unwrap_or_default()
            .to_owned();

        let margin = if area.width >= 12 {
            2
        } else {
            u16::from(area.width >= 3)
        };
        let padded = |rect: Rect| {
            Rect::new(
                rect.x + margin,
                rect.y,
                rect.width.saturating_sub(margin * 2),
                rect.height,
            )
        };

        let title_width = area.width.saturating_sub(margin * 2);
        let title_lines = render::wrapped_cells(
            &selected_title,
            title_width as usize,
            if area.height < 12 { 1 } else { 3 },
        );
        let title_height = title_lines.len() as u16;

        frame.render_widget(Block::default().style(self.theme.normal()), area);
        let regions = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(u16::from(self.state.status.is_some())),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(title_height),
            Constraint::Length(if area.height < 14 { 0 } else { 2 }),
            Constraint::Length(u16::from(!notice.is_empty())),
            Constraint::Length(1),
        ])
        .split(area);

        self.draw_search(frame, padded(regions[0]));

        for divider in [regions[1], regions[4]] {
            frame.render_widget(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(self.theme.separator()),
                divider,
            );
        }

        if let Some(status) = self.state.status.as_deref() {
            frame.render_widget(
                Paragraph::new(format!(
                    "{}: {} (a {})",
                    self.messages.get("filter"),
                    self.messages.get(status),
                    self.messages.get("all")
                ))
                .style(self.theme.status(status)),
                padded(regions[2]),
            );
        }

        self.draw_list(frame, regions[3]);
        self.draw_selection_details(frame, title_lines, padded(regions[5]), padded(regions[6]));

        frame.render_widget(
            Paragraph::new(notice).style(self.theme.accent()),
            padded(regions[7]),
        );
        frame.render_widget(
            Paragraph::new(self.messages.get(if self.search_focus {
                "keys"
            } else {
                "list_keys"
            }))
            .style(self.theme.muted()),
            padded(regions[8]),
        );

        if self.help {
            self.draw_help(frame, area);
        }
    }

    fn draw_search(&mut self, frame: &mut Frame<'_>, search: Rect) {
        let prefix = render::crop("/ ", search.width as usize);
        let prefix_width = render::width(&prefix) as u16;
        let inner = Rect::new(
            search.x + prefix_width,
            search.y,
            search.width.saturating_sub(prefix_width),
            search.height,
        );

        let mut start = self.editor.cursor;
        let mut used = 0;
        for (index, grapheme) in self.editor.text[..self.editor.cursor]
            .grapheme_indices(true)
            .rev()
        {
            let cells = render::width(grapheme);
            if used + cells >= inner.width.max(1) as usize {
                break;
            }
            used += cells;
            start = index;
        }

        let query = if self.editor.text.is_empty() {
            self.messages.get("search").to_owned()
        } else {
            render::crop(&self.editor.text[start..], inner.width as usize)
        };
        let prompt_style = if self.search_focus {
            self.theme.accent().add_modifier(Modifier::BOLD)
        } else {
            self.theme.muted()
        };

        frame.render_widget(
            Paragraph::new(prefix).style(prompt_style),
            Rect::new(search.x, search.y, prefix_width, search.height),
        );
        frame.render_widget(
            Paragraph::new(query).style(if self.editor.text.is_empty() {
                self.theme.muted()
            } else {
                self.theme.normal()
            }),
            inner,
        );

        if self.search_focus && !self.help && inner.width > 0 && inner.height > 0 {
            let x = render::width(&self.editor.text[start..self.editor.cursor])
                .min(inner.width.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position((inner.x + x, inner.y));
        }

        self.geometry.search = search;
        self.geometry.input = inner;
        self.geometry.input_start = start;
    }

    fn rebuild_lines(&mut self, width: u16) {
        // Spacers have no row key. Preserve their signed offset from a neighboring group.
        let anchor = self
            .lines
            .iter()
            .enumerate()
            .skip(self.scroll)
            .find_map(|(index, line)| line.key.clone().map(|key| (index, key)))
            .or_else(|| {
                self.lines
                    .iter()
                    .enumerate()
                    .take(self.scroll)
                    .rev()
                    .find_map(|(index, line)| line.key.clone().map(|key| (index, key)))
            })
            .map(|(_, key)| {
                let start = self
                    .lines
                    .iter()
                    .position(|line| line.key.as_deref() == Some(&key))
                    .unwrap_or(self.scroll);
                (key, self.scroll as isize - start as isize)
            });

        self.lines = render::tree_lines(self.state.rows(), width as usize, self.theme);
        if !self.reveal
            && let Some((key, offset)) = anchor
            && let Some(index) = self
                .lines
                .iter()
                .position(|line| line.key.as_deref() == Some(&key))
        {
            let height = self
                .lines
                .iter()
                .skip(index)
                .take_while(|line| line.key.as_deref() == Some(&key))
                .count();
            let offset = offset.min(height.saturating_sub(1) as isize);
            self.scroll = index.saturating_add_signed(offset);
        }

        self.line_width = width;
        self.rebuild = false;
    }

    fn draw_list(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.geometry.list = area;
        if self.rebuild || self.line_width != area.width {
            self.rebuild_lines(area.width);
        }

        let viewport = area.height as usize;
        if self.reveal {
            if let Some(key) = self.state.selected_key()
                && let Some(start) = self
                    .lines
                    .iter()
                    .position(|line| line.key.as_deref() == Some(key))
            {
                let end = self
                    .lines
                    .iter()
                    .rposition(|line| line.key.as_deref() == Some(key))
                    .unwrap_or(start);
                if start < self.scroll {
                    self.scroll = start;
                } else if end >= self.scroll + viewport {
                    self.scroll = end.saturating_add(1).saturating_sub(viewport);
                }
            }
            self.reveal = false;
        }

        self.scroll = self.scroll.min(self.lines.len().saturating_sub(viewport));

        if self.lines.is_empty() {
            let key = if self.state.snapshot.is_none() {
                "loading"
            } else if self.index_busy {
                "indexing"
            } else {
                "empty"
            };
            frame.render_widget(
                Paragraph::new(self.messages.get(key)).style(self.theme.muted()),
                area,
            );
        } else {
            let key = self.state.selected_key();
            let mut marked = false;
            let lines: Vec<_> = self
                .lines
                .iter()
                .skip(self.scroll)
                .take(viewport)
                .map(|line| {
                    let mut text = line.text.clone();
                    if key.is_some() && line.key.as_deref() == key {
                        let mut selected = self.theme.selected();
                        selected.fg = None;
                        text.style = text.style.patch(selected);

                        for (index, span) in text.spans.iter_mut().enumerate() {
                            let mut style = selected;
                            if line.is_excerpt && index >= 2 {
                                style.bg = self.theme.excerpt(true).bg;
                            }
                            span.style = span.style.patch(style);
                        }

                        if !marked && let Some(marker) = text.spans.first_mut() {
                            marker.content = render::crop("› ", area.width as usize).into();
                            marker.style = self.theme.accent().add_modifier(Modifier::BOLD);
                            marked = true;
                        }

                        text.spans.push(ratatui::text::Span::styled(
                            " ".repeat((area.width as usize).saturating_sub(text.width())),
                            selected,
                        ));
                    }
                    text
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), area);
        }
    }

    fn draw_selection_details(
        &self,
        frame: &mut Frame<'_>,
        title_lines: Vec<String>,
        title_area: Rect,
        context_area: Rect,
    ) {
        if let Some(row) = self.state.selected() {
            frame.render_widget(
                Paragraph::new(
                    title_lines
                        .into_iter()
                        .map(ratatui::text::Line::raw)
                        .collect::<Vec<_>>(),
                )
                .style(self.theme.normal().add_modifier(Modifier::BOLD)),
                title_area,
            );

            let mut parts = vec![row.workspace.label.clone()];
            if let Some(session) = &row.session {
                if let Some(at) = session.activity_at {
                    parts.insert(
                        0,
                        format!(
                            "{} {}",
                            self.messages.get("conversation"),
                            at.with_timezone(&Local).format("%Y-%m-%d %H:%M")
                        ),
                    );
                }

                let provider_label = if session.display_kind.eq_ignore_ascii_case(&session.kind) {
                    render::provider_label(&session.kind, &session.display_kind)
                } else {
                    session.display_kind.clone()
                };
                for value in [&session.tab_label, &session.name, &provider_label] {
                    if !value.is_empty() && value != &session.title && !parts.contains(value) {
                        parts.push(value.clone());
                    }
                }

                if session.activity_at.is_none() && session.kind != "terminal" {
                    parts.insert(
                        0,
                        self.messages
                            .get(if session.native_reference.is_none() {
                                "date_unlinked"
                            } else {
                                "date_unknown"
                            })
                            .to_owned(),
                    );
                }
            } else if row.kind == RowKind::Workspace {
                parts.clear();
            }

            frame.render_widget(
                Paragraph::new(
                    render::wrapped_cells(
                        &parts.join(" | "),
                        context_area.width as usize,
                        context_area.height as usize,
                    )
                    .into_iter()
                    .map(ratatui::text::Line::raw)
                    .collect::<Vec<_>>(),
                )
                .style(self.theme.muted()),
                context_area,
            );
        }
    }

    fn draw_help(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let width = area.width.saturating_sub(4).max(area.width.min(32)).min(86);
        let lines = help::lines(width.saturating_sub(4) as usize, self.theme);
        let height = (lines.len().saturating_add(4).min(u16::MAX as usize) as u16)
            .min(area.height.saturating_sub(2).max(area.height.min(8)));
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .style(self.theme.normal())
            .borders(Borders::ALL)
            .border_style(self.theme.separator())
            .title(" Help ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let content = Rect::new(
            inner.x + inner.width.min(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        let regions = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(content);
        let max_scroll = lines.len().saturating_sub(regions[0].height as usize);
        self.help_scroll = self
            .help_scroll
            .min(max_scroll.min(u16::MAX as usize) as u16);
        frame.render_widget(
            Paragraph::new(lines)
                .style(self.theme.normal())
                .scroll((self.help_scroll, 0)),
            regions[0],
        );
        frame.render_widget(
            Paragraph::new("─".repeat(regions[1].width as usize)).style(self.theme.separator()),
            regions[1],
        );
        let footer = if content.width >= 25 {
            "↑↓ scroll  Esc / F1 back"
        } else {
            "Esc / F1 back"
        };
        frame.render_widget(
            Paragraph::new(render::crop(footer, regions[2].width as usize))
                .style(self.theme.muted()),
            regions[2],
        );
    }
}

struct Restore;
impl Drop for Restore {
    fn drop(&mut self) {
        // Restore input flags even if stdout is broken and terminal cleanup writes fail.
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            event::DisableMouseCapture,
            event::DisableBracketedPaste
        );
        ratatui::restore();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.client.cancel();
    }
}

pub fn run(client: Client) -> Result<(), Box<dyn Error>> {
    let mut app = App::new(client, true);
    let _restore = Restore;
    let mut terminal = ratatui::try_init()?;
    execute!(
        io::stdout(),
        event::EnableMouseCapture,
        event::EnableBracketedPaste
    )?;
    let mut colors = ColorProbe::new(
        app.theme.colors_enabled() && io::stdin().is_terminal() && io::stdout().is_terminal(),
    );
    let mut queried = false;
    while !app.quit {
        app.tick();
        if app.dirty {
            terminal.draw(|frame| app.draw(frame))?;
            app.dirty = false;
        }
        if !queried {
            // The first frame is visible before any color reply is requested.
            colors.request(&mut io::stdout());
            queried = true;
        }
        for event in colors.expire(Instant::now()) {
            app.event(event);
            if app.quit {
                break;
            }
        }
        if app.quit {
            break;
        }
        if !event::poll(Duration::from_millis(4))? {
            continue;
        }

        // Drain ready input before repainting so a fast input burst produces one frame.
        loop {
            let (events, palette) = colors.feed(event::read()?, Instant::now());
            if let Some(palette) = palette {
                let theme = app.theme.with_terminal_colors(palette);
                if app.theme != theme {
                    app.theme = theme;
                    app.rebuild = true;
                    app.dirty = true;
                }
            }
            for event in events {
                app.event(event);
                if app.quit {
                    break;
                }
            }
            if app.quit || !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
    app.client.cancel();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Tab, Workspace};
    use chrono::{TimeZone, Utc};
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};

    // Two wide syllables and a combining sequence stress byte offsets and cell widths.
    const MIXED_WIDTH_QUERY: &str = "\u{d55c}\u{ae00}e\u{301}";

    fn fixture() -> Snapshot {
        Snapshot {
            workspaces: vec![
                Workspace {
                    id: "a".into(),
                    label: "App".into(),
                    focused: true,
                },
                Workspace {
                    id: "b".into(),
                    label: "Infra".into(),
                    focused: false,
                },
            ],
            tabs: vec![
                Tab {
                    id: "ta".into(),
                    workspace_id: "a".into(),
                    label: "API".into(),
                    number: 1,
                    focused: true,
                },
                Tab {
                    id: "tb".into(),
                    workspace_id: "a".into(),
                    label: "Web".into(),
                    number: 2,
                    focused: false,
                },
                Tab {
                    id: "tc".into(),
                    workspace_id: "b".into(),
                    label: "Ops".into(),
                    number: 1,
                    focused: false,
                },
            ],
            sessions: vec![
                Session {
                    terminal_id: "one".into(),
                    pane_id: "pa".into(),
                    workspace_id: "a".into(),
                    tab_id: "ta".into(),
                    tab_label: "API".into(),
                    title: "Fix payment routing".into(),
                    kind: "codex".into(),
                    name: "builder".into(),
                    cwd: "/shared".into(),
                    focused: true,
                    ..Default::default()
                },
                Session {
                    terminal_id: "two".into(),
                    pane_id: "pb".into(),
                    workspace_id: "a".into(),
                    tab_id: "tb".into(),
                    tab_label: "Web".into(),
                    title: "Review payments".into(),
                    kind: "claude".into(),
                    name: "reviewer".into(),
                    cwd: "/shared".into(),
                    ..Default::default()
                },
                Session {
                    terminal_id: "three".into(),
                    pane_id: "pc".into(),
                    workspace_id: "b".into(),
                    tab_id: "tc".into(),
                    tab_label: "Ops".into(),
                    title: "Tail service logs".into(),
                    kind: "terminal".into(),
                    cwd: "/shared".into(),
                    ..Default::default()
                },
            ],
        }
    }

    fn app() -> App {
        let mut app = App::new(
            Client::new("/nonexistent/herdr-finder-test", Duration::from_millis(5)).unwrap(),
            false,
        );
        app.state.update(fixture());
        app
    }

    fn paint(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        app.key(KeyEvent::new(code, mods));
    }
    fn click(app: &mut App, x: u16, y: u16, button: MouseButton) {
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn slash_enters_search_and_escape_returns_to_browsing_without_clearing_the_query() {
        let mut app = app();
        assert!(!app.search_focus);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!app.search_focus);
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(app.search_focus);
        app.editor.insert("shared");
        app.query_changed();
        let key = app.state.selected_key().unwrap().to_owned();
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.editor.cursor, 5);
        assert_eq!(app.state.selected_key(), Some(key.as_str()));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.search_focus);
        press(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(app.search_focus);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.search_focus && !app.quit);
        assert_eq!(app.editor.text, "shared");
        assert_eq!(app.state.query, "shared");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit);
    }

    #[test]
    fn browsing_arrows_land_on_the_next_tab_session_without_folding() {
        let mut app = app();
        app.state.select("s:one");
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:two"));
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:three"));
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:three"));
        press(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:two"));
        press(&mut app, KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(app.state.selected_key(), Some("s:three"));
        assert!(app.state.collapsed.is_empty() && app.state.collapsed_tabs.is_empty());
    }

    #[test]
    fn active_status_filter_is_visible_and_modified_letters_do_not_change_it() {
        let mut app = app();
        press(&mut app, KeyCode::Char('w'), KeyModifiers::CONTROL);
        press(&mut app, KeyCode::Char('d'), KeyModifiers::ALT);
        assert!(app.state.status.is_none());
        press(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(app.state.status.as_deref(), Some("done"));
        let terminal = paint(&mut app, 40, 24);
        assert!(format!("{:?}", terminal.backend().buffer()).contains("Filter: Done (a All)"));
        press(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.state.status.is_none());
    }

    #[test]
    fn double_clicking_a_group_folds_once_without_reopening_it() {
        let mut app = app();
        paint(&mut app, 80, 24);
        let region = app.geometry.list;
        let header = app
            .lines
            .iter()
            .position(|line| line.key.as_deref() == Some("w:a"))
            .unwrap();
        let y = region.y + header as u16;
        click(&mut app, region.x + 3, y, MouseButton::Left);
        paint(&mut app, 80, 24);
        click(&mut app, region.x + 3, y, MouseButton::Left);
        assert!(app.state.collapsed.contains("a"));
        assert!(!app.opening);
    }

    #[test]
    fn help_and_the_full_selected_row_keep_the_terminal_background() {
        let mut app = app();
        app.theme = Theme::new(true);
        let terminal = paint(&mut app, 80, 24);
        let row = app
            .lines
            .iter()
            .position(|line| line.key.as_deref() == Some("s:one"))
            .unwrap();
        let y = app.geometry.list.y + (row - app.scroll) as u16;
        assert_eq!(
            Some(terminal.backend().buffer()[(79, y)].bg),
            app.theme.selected().bg
        );
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let terminal = paint(&mut app, 80, 24);
        let cell = &terminal.backend().buffer()[(5, 2)];
        assert_eq!(Some(cell.bg), app.theme.normal().bg);
    }

    #[test]
    fn help_sections_remain_reachable_on_narrow_screens_without_changing_search() {
        let mut app = app();
        app.theme = Theme::new(true);
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        press(&mut app, KeyCode::Char('p'), KeyModifiers::NONE);
        let selection = app.state.selected_key().map(str::to_owned);
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        for width in [80, 36] {
            app.help_scroll = 0;
            let terminal = paint(&mut app, width, 24);
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Browse"));
            assert!(text.contains("Esc / F1 back"));
            for _ in 0..100 {
                press(&mut app, KeyCode::Down, KeyModifiers::NONE);
            }
            let terminal = paint(&mut app, width, 24);
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Mouse"));
            assert!(text.contains("Right-click"));
            assert!(text.contains("Esc / F1 back"));
            let last_scroll = app.help_scroll;
            press(&mut app, KeyCode::Down, KeyModifiers::NONE);
            paint(&mut app, width, 24);
            assert_eq!(app.help_scroll, last_scroll);
        }
        assert_eq!(app.state.selected_key(), selection.as_deref());
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.help && app.search_focus && !app.quit);
        assert_eq!(app.editor.text, "p");
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.search_focus && !app.quit);
    }

    #[test]
    fn narrow_popup_keeps_the_selected_title_in_two_lines() {
        // The two-cell ideograph crosses the 31-cell padded footer boundary.
        let wide_title = format!("{}\u{754c}", "x".repeat(30));
        for title in [
            "Verify payment settlement and refund".to_owned(),
            wide_title,
        ] {
            let mut app = app();
            let mut snapshot = fixture();
            snapshot.sessions[0].title = title.clone();
            app.state.update(snapshot);
            let terminal = paint(&mut app, 35, 17);
            let buffer = terminal.backend().buffer();
            let displayed: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(
                displayed
                    .split_whitespace()
                    .collect::<String>()
                    .contains(&title.split_whitespace().collect::<String>())
            );

            // A buffer can hold a wide symbol even when its final cell would exceed the text area.
            for y in app.geometry.list.bottom() + 1..17 {
                for x in 2..33 {
                    assert!(x as usize + render::width(buffer[(x, y)].symbol()) <= 33);
                }
                for x in [0, 1, 33, 34] {
                    assert_eq!(buffer[(x, y)].symbol(), " ");
                }
            }
        }
    }

    #[test]
    fn body_matches_keep_a_muted_quote_and_terminal_background_when_selected() {
        let mut app = app();
        app.theme = Theme::new(true);
        app.state.set_query("needle".into());
        let origin = app.state.snapshot.as_ref().unwrap().sessions.clone();
        app.state.set_contents(
            HashMap::from([
                ("one".into(), ContentDocument::new("first needle context")),
                ("three".into(), ContentDocument::new("other needle context")),
            ]),
            &origin,
        );
        app.state.select("s:one");
        let terminal = paint(&mut app, 80, 40);
        for key in ["s:one", "s:three"] {
            let index = app
                .lines
                .iter()
                .position(|line| line.key.as_deref() == Some(key))
                .unwrap()
                + 1;
            let y = app.geometry.list.y + (index - app.scroll) as u16;
            let bar = &terminal.backend().buffer()[(app.geometry.list.x + 10, y)];
            let content = &terminal.backend().buffer()[(app.geometry.list.x + 12, y)];
            assert_eq!(bar.symbol(), "│");
            assert_eq!(content.bg, Color::Reset);
            assert!(content.modifier.contains(Modifier::DIM));
            assert!(!content.modifier.contains(Modifier::BOLD));
            let highlighted = (app.geometry.list.x..app.geometry.list.right())
                .map(|x| &terminal.backend().buffer()[(x, y)])
                .filter(|cell| cell.modifier.contains(Modifier::UNDERLINED))
                .collect::<Vec<_>>();
            assert_eq!(highlighted.len(), "needle".len());
            assert!(
                highlighted
                    .iter()
                    .all(|cell| cell.modifier.contains(Modifier::BOLD))
            );
        }
    }

    #[test]
    fn clicking_after_deleting_a_scrolled_unicode_query_uses_the_current_text() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        app.editor.insert(&MIXED_WIDTH_QUERY.repeat(100));
        app.query_changed();
        paint(&mut app, 40, 24);
        assert!(app.geometry.input_start > 0);
        press(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        let input = app.geometry.input;
        click(&mut app, input.x + 1, input.y, MouseButton::Left);
        assert!(app.search_focus);
        assert!(app.editor.text.is_empty());
        assert_eq!(app.editor.cursor, 0);
    }

    #[test]
    fn date_reason_is_visible_before_long_context_and_plain_terminals_omit_it() {
        let mut app = app();
        let mut snapshot = fixture();
        snapshot.workspaces[0].label = "Long project workspace name".repeat(3);
        snapshot.sessions[0].tab_label = "Detailed service tab name".repeat(3);
        app.state.update(snapshot);
        let terminal = paint(&mut app, 40, 24);
        assert!(format!("{:?}", terminal.backend().buffer()).contains("Date unlinked"));
        app.state.select("s:three");
        let terminal = paint(&mut app, 40, 24);
        assert!(!format!("{:?}", terminal.backend().buffer()).contains("Date unlinked"));
    }

    #[test]
    fn filtered_browsing_never_selects_an_inactive_header() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        app.editor.insert("shared");
        app.query_changed();
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut app, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:one"));
        press(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:two"));
        press(&mut app, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(app.state.selected_key(), Some("s:three"));
        assert!(!app.state.select("t:tc"));
    }

    #[test]
    fn tree_render_uses_inline_counts_badges_and_muted_dates_without_an_inner_title() {
        let mut app = app();
        app.theme = Theme::new(true);
        let mut snapshot = fixture();
        snapshot.sessions[0].activity_at =
            Some(Utc.with_ymd_and_hms(2026, 11, 8, 7, 43, 0).unwrap());
        app.state.update(snapshot);
        let terminal = paint(&mut app, 100, 40);
        let text = format!("{:?}", terminal.backend().buffer());
        assert!(!text.contains("Herdr Finder"));
        assert!(!text.contains("Workspace navigator"));
        let workspace = app
            .lines
            .iter()
            .find(|line| line.key.as_deref() == Some("w:a"))
            .unwrap();
        assert_eq!(workspace.text.to_string(), "  ▾ App (2)");
        assert_eq!(workspace.text.spans[1].style.fg, app.theme.workspace().fg);
        let tab = app
            .lines
            .iter()
            .find(|line| line.key.as_deref() == Some("t:ta"))
            .unwrap();
        assert_eq!(tab.text.to_string(), "    ▾ API (1)");
        assert_eq!(tab.text.spans[1].style.fg, app.theme.tab().fg);
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.to_string().contains("[Codex]"))
        );
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.to_string().contains("[Claude]"))
        );
        let date = app
            .lines
            .iter()
            .find(|line| line.text.to_string().contains("2026-11-08"))
            .unwrap();
        assert_eq!(date.text.style.fg, app.theme.muted().fg);
        let date_index = app
            .lines
            .iter()
            .position(|line| std::ptr::eq(line, date))
            .unwrap();
        let y = app.geometry.list.y + (date_index - app.scroll) as u16;
        let cell = &terminal.backend().buffer()[(app.geometry.list.x + 8, y)];
        assert_eq!(cell.symbol(), "2");
        assert!(!cell.modifier.contains(Modifier::REVERSED));
        assert_eq!(Some(cell.fg), app.theme.muted().fg);
        assert_eq!(Some(cell.bg), app.theme.selected().bg);
        assert_ne!(cell.bg, Color::Rgb(255, 255, 255));
    }

    #[test]
    fn terminal_resize_rebuilds_actual_width_and_keeps_known_dates_and_literal_snippets() {
        let mut app = app();
        let mut snapshot = fixture();
        snapshot.sessions[0].activity_at =
            Some(Utc.with_ymd_and_hms(2026, 11, 8, 7, 43, 0).unwrap());
        app.state.update(snapshot);
        app.state.set_query("needle".into());
        let origin = app.state.snapshot.as_ref().unwrap().sessions.clone();
        app.state.set_contents(
            HashMap::from([(
                "one".into(),
                ContentDocument::new("[bold]literal[/bold] needle with context"),
            )]),
            &origin,
        );
        for width in [40, 160, 80, 40, 12] {
            app.rebuild = true;
            app.reveal = true;
            paint(&mut app, width, 24);
            assert!(
                app.lines
                    .iter()
                    .all(|line| line.text.width() <= width as usize)
            );
            assert!(app.lines.iter().any(|line| {
                line.text.spans.iter().any(|span| {
                    span.style
                        .add_modifier
                        .contains(Modifier::BOLD | Modifier::UNDERLINED)
                })
            }));
            assert!(
                app.lines
                    .iter()
                    .any(|line| line.text.to_string().contains("2026-11-08"))
                    || width < 16
            );
        }
    }

    #[test]
    fn mouse_selection_and_right_click_select_the_session_and_parent() {
        let mut app = app();
        app.state.select("s:three");
        app.reveal = true;
        paint(&mut app, 100, 40);
        let y = app
            .lines
            .iter()
            .position(|line| line.key.as_deref() == Some("s:three"))
            .unwrap()
            - app.scroll;
        let region = app.geometry.list;
        click(
            &mut app,
            region.x + 8,
            region.y + y as u16,
            MouseButton::Left,
        );
        assert_eq!(app.state.selected_key(), Some("s:three"));
        assert!(!app.opening);
        click(
            &mut app,
            region.x + 8,
            region.y + y as u16,
            MouseButton::Right,
        );
        assert_eq!(app.state.selected().unwrap().kind, RowKind::Tab);
        assert!(!app.opening);
    }

    #[test]
    fn borderless_search_dividers_and_padded_footer_keep_the_three_regions_distinct() {
        for width in [40, 80, 160] {
            let mut app = app();
            let terminal = paint(&mut app, width, 24);
            let buffer = terminal.backend().buffer();
            let rows: Vec<String> = (0..24)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            assert!(rows[0].starts_with("  / Search agents and terminals"));
            assert!(
                !rows
                    .iter()
                    .any(|row| row.contains("┌") || row.contains("WS  ‹"))
            );
            let dividers: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.chars().all(|c| c == '─'))
                .map(|(y, _)| y)
                .collect();
            assert_eq!(dividers.len(), 2);
            assert_eq!(dividers[0], 1);
            for y in dividers[1] + 1..24 {
                for x in [0, 1, width - 2, width - 1] {
                    assert_eq!(buffer[(x, y as u16)].symbol(), " ");
                }
            }
            let prompt = app.geometry.search;
            click(&mut app, prompt.x, prompt.y, MouseButton::Left);
            assert!(app.search_focus);
            press(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
            assert_eq!(app.editor.text, "x");
        }
    }

    #[test]
    fn background_snapshot_preserves_manual_scroll_and_disappeared_selection_is_inert() {
        let mut app = app();
        let mut snapshot = fixture();
        let seed = snapshot.sessions[0].clone();
        snapshot.sessions = (0..80)
            .map(|i| Session {
                terminal_id: format!("terminal-{i}"),
                pane_id: format!("pane-{i}"),
                title: format!("Task {i}"),
                focused: i == 0,
                ..seed.clone()
            })
            .collect();
        app.state.update(snapshot.clone());
        paint(&mut app, 80, 24);
        app.scroll = 30;
        app.reveal = false;
        app.receive(Update::Snapshot(Ok(snapshot.clone())));
        paint(&mut app, 80, 24);
        assert_eq!(app.scroll, 30);
        snapshot.sessions.remove(0);
        app.receive(Update::Snapshot(Ok(snapshot)));
        assert!(app.state.selected().is_none());
        app.activate();
        assert!(!app.opening);
    }

    #[test]
    fn late_body_match_is_selected_and_replaced_activity_is_discarded() {
        let mut app = app();
        app.editor.insert("bodyneedle");
        app.query_changed();
        assert!(app.state.selected().is_none());
        let origin = app.state.snapshot.as_ref().unwrap().sessions.clone();
        app.receive(Update::Contents(
            origin.clone(),
            HashMap::from([("three".into(), ContentDocument::new("bodyneedle context"))]),
        ));
        assert_eq!(app.state.selected_key(), Some("s:three"));
        let mut stale = origin[0].clone();
        stale.native_reference = Some(("claude".into(), "id".into(), "old".into()));
        let stamp = ActivityStamp {
            at: Utc::now(),
            source: "claude_jsonl".into(),
        };
        app.receive(Update::Activity(
            vec![stale],
            HashMap::from([("one".into(), stamp)]),
        ));
        assert!(!app.stamps.contains_key("one"));
    }

    #[test]
    fn tiny_terminals_and_help_do_not_panic_or_leave_escape_ambiguous() {
        let mut app = app();
        for (w, h) in [(1, 1), (8, 6), (40, 12)] {
            app.rebuild = true;
            paint(&mut app, w, h);
        }
        press(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(app.help);
        paint(&mut app, 40, 24);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!app.help && !app.quit);
        press(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.quit);
    }

    #[test]
    fn spacer_at_viewport_top_keeps_following_group_when_a_date_arrives_above() {
        let mut app = app();
        let mut snapshot = fixture();
        let seeds = snapshot.sessions.clone();
        snapshot.sessions = (0..90)
            .map(|i| {
                let seed = &seeds[i / 30];
                Session {
                    terminal_id: format!("term-{i}"),
                    pane_id: format!("pane-{i}"),
                    focused: i == 0,
                    native_reference: (i == 0)
                        .then(|| ("claude".into(), "id".into(), "known".into())),
                    ..seed.clone()
                }
            })
            .collect();
        app.state.update(snapshot.clone());
        paint(&mut app, 80, 24);
        app.scroll = app
            .lines
            .iter()
            .position(|line| line.key.is_none())
            .unwrap();
        app.reveal = false;
        paint(&mut app, 80, 24);
        assert!(app.lines[app.scroll].key.is_none());
        let following = app.lines[app.scroll + 1].key.clone();
        let stamp = ActivityStamp {
            at: Utc::now(),
            source: "claude_jsonl".into(),
        };
        app.receive(Update::Activity(
            vec![snapshot.sessions[0].clone()],
            HashMap::from([("term-0".into(), stamp)]),
        ));
        paint(&mut app, 80, 24);
        assert!(app.lines[app.scroll].key.is_none());
        assert_eq!(app.lines[app.scroll + 1].key, following);
    }

    #[test]
    fn long_search_keeps_the_cursor_visible_without_repeated_prefix_scans() {
        let mut app = app();
        app.editor.insert(&MIXED_WIDTH_QUERY.repeat(4000));
        app.query_changed();
        paint(&mut app, 40, 24);
        assert!(app.geometry.input_start > 0);
        assert!(
            render::width(&app.editor.text[app.geometry.input_start..app.editor.cursor])
                < app.geometry.input.width as usize
        );
    }

    #[test]
    fn dropping_the_ui_cancels_background_io_even_on_early_failure() {
        let client =
            Client::new("/nonexistent/navigator-drop-test", Duration::from_millis(5)).unwrap();
        {
            let _app = App::new(client.clone(), false);
        }
        assert_eq!(client.snapshot().unwrap_err().code, "cancelled");
    }
}
