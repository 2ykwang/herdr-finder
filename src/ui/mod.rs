mod help;
mod input;
mod messages;
mod render;
mod terminal_colors;
mod theme;

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
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    client::{Client, ScreenCapture},
    model::{FinderError, Session, Snapshot},
    search::{ContentDocument, highlight_spans_terms},
    state::{FinderState, GroupLevel, RowKind},
    visits::{VisitHistory, visit_time},
};
use input::Editor;
use messages::Messages;
use render::VisualLine;
use terminal_colors::ColorProbe;
use theme::Theme;

enum Update {
    Snapshot(Result<Snapshot, FinderError>, Option<HashMap<String, i64>>),
    Contents(Vec<Session>, HashMap<String, ContentDocument>),
    Focus(Result<(), FinderError>),
    Preview(Box<PreviewResponse>),
}

#[derive(Default)]
struct Geometry {
    search: Rect,
    input: Rect,
    input_start: usize,
    list: Rect,
    preview: Rect,
    details: Rect,
}

struct PreviewCapture {
    session: Session,
    document: ContentDocument,
    truncated: bool,
    read_at: Instant,
}

struct PreviewResponse {
    generation: u64,
    session: Session,
    result: Result<ScreenCapture, FinderError>,
}

struct PreviewRequest {
    generation: u64,
    client: Client,
}

fn same_preview_session(a: &Session, b: &Session) -> bool {
    a.terminal_id == b.terminal_id
        && a.pane_id == b.pane_id
        && a.workspace_id == b.workspace_id
        && a.tab_id == b.tab_id
        && a.kind == b.kind
        && a.native_reference == b.native_reference
}

fn same_preview_target(a: &Session, b: &Session) -> bool {
    same_preview_session(a, b) && a.revision == b.revision
}

struct IndexedScreen {
    pane_id: String,
    kind: String,
    reference: Option<(String, String, String)>,
    revision: u64,
    read_at: Instant,
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
    next_snapshot: Instant,
    index_due: Option<Instant>,

    indexed: HashMap<String, IndexedScreen>,

    preview_visible: bool,
    preview_target: Option<Session>,
    preview_due: Option<Instant>,
    preview_generation: u64,
    preview_busy: Option<PreviewRequest>,
    preview_capture: Option<PreviewCapture>,
    preview_error: Option<FinderError>,
    preview_scroll: usize,
    preview_manual_scroll: bool,

    sender: mpsc::Sender<Update>,
    receiver: mpsc::Receiver<Update>,
    last_click: Option<(String, u16, u16, Instant)>,
}

impl App {
    fn new(client: Client, prefetch: bool) -> Self {
        let (sender, receiver) = mpsc::channel();
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
            next_snapshot: Instant::now() + Duration::from_secs(2),
            index_due: None,

            indexed: HashMap::new(),

            preview_visible: true,
            preview_target: None,
            preview_due: None,
            preview_generation: 0,
            preview_busy: None,
            preview_capture: None,
            preview_error: None,
            preview_scroll: 0,
            preview_manual_scroll: false,

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
        let client = self.client.clone();
        let history = self
            .state
            .snapshot
            .is_none()
            .then(|| VisitHistory::from_env(client.socket_path()))
            .flatten();
        self.snapshot_busy = true;
        let sender = self.sender.clone();
        thread::spawn(move || {
            let at = visit_time();
            let snapshot = client.snapshot();
            let visits = history
                .zip(snapshot.as_ref().ok())
                .map(|(history, snapshot)| {
                    let visits = history.recent().unwrap_or_default();
                    if let Some(workspace) = snapshot
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.focused)
                    {
                        let _ = history.record(&workspace.id, at);
                    }
                    visits
                });
            let _ = sender.send(Update::Snapshot(snapshot, visits));
        });
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
        self.sync_preview();
        self.request_preview();
    }

    fn receive(&mut self, update: Update) {
        match update {
            Update::Snapshot(result, visits) => {
                self.snapshot_busy = false;
                match result {
                    Ok(snapshot) => {
                        let initial = self.state.snapshot.is_none();
                        if let Some(visits) = visits {
                            self.state.set_workspace_visits(visits);
                        }
                        self.indexed
                            .retain(|id, _| snapshot.sessions.iter().any(|s| &s.terminal_id == id));
                        self.state.update(snapshot);
                        self.error.clear();
                        self.reveal |= initial;
                        self.rebuild = true;
                        if self.state.needs_contents() {
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
            Update::Focus(result) => {
                self.opening = false;
                match result {
                    Ok(()) => self.quit = true,
                    Err(error) => self.error = error.to_string(),
                }
            }
            Update::Preview(result) => {
                let PreviewResponse {
                    generation,
                    session: origin,
                    result,
                } = *result;
                if self
                    .preview_busy
                    .as_ref()
                    .is_some_and(|busy| busy.generation == generation)
                {
                    self.preview_busy = None;
                }
                if generation == self.preview_generation
                    && self.preview_visible
                    && self
                        .state
                        .selected()
                        .and_then(|row| row.session.as_ref())
                        .is_some_and(|current| same_preview_target(current, &origin))
                {
                    self.preview_due = Some(Instant::now() + Duration::from_secs(2));
                    match result {
                        Ok(capture) => {
                            let document = ContentDocument::new(capture.text);
                            if capture.revision >= origin.revision {
                                let mut captured_session = origin.clone();
                                captured_session.revision = capture.revision;
                                self.rebuild |= self.state.set_contents(
                                    HashMap::from([(origin.terminal_id.clone(), document.clone())]),
                                    std::slice::from_ref(&captured_session),
                                );
                                self.indexed.insert(
                                    origin.terminal_id.clone(),
                                    IndexedScreen {
                                        pane_id: origin.pane_id.clone(),
                                        kind: origin.kind.clone(),
                                        reference: origin.native_reference.clone(),
                                        revision: capture.revision,
                                        read_at: Instant::now(),
                                    },
                                );
                            }
                            self.preview_capture = Some(PreviewCapture {
                                session: origin,
                                document,
                                truncated: capture.truncated,
                                read_at: Instant::now(),
                            });
                            self.preview_error = None;
                        }
                        Err(error) => self.preview_error = Some(error),
                    }
                }
            }
        }
        self.sync_preview();
        self.dirty = true;
    }

    fn request_contents(&mut self) {
        if self.index_busy || self.opening || !self.state.needs_contents() {
            return;
        }
        let client = self.client.clone();
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
                        || cached.revision < s.revision
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

    fn invalidate_preview(&mut self, clear_capture: bool) {
        self.preview_generation = self.preview_generation.wrapping_add(1);
        if let Some(request) = &self.preview_busy {
            request.client.cancel();
        }
        self.preview_due = None;
        self.preview_error = None;
        if clear_capture {
            self.preview_capture = None;
            self.preview_scroll = 0;
            self.preview_manual_scroll = false;
        }
    }

    fn sync_preview(&mut self) {
        if !self.preview_visible {
            if self.preview_target.is_some() {
                self.invalidate_preview(true);
                self.preview_target = None;
            }
            return;
        }
        let selected = self.state.selected().and_then(|row| row.session.clone());
        let changed = match (&self.preview_target, &selected) {
            (Some(previous), Some(current)) => !same_preview_target(previous, current),
            (None, None) => false,
            _ => true,
        };
        if changed {
            let same_session = self
                .preview_target
                .as_ref()
                .zip(selected.as_ref())
                .is_some_and(|(previous, current)| same_preview_session(previous, current));
            let previous_scroll = self.preview_scroll;
            let manual_scroll = self.preview_manual_scroll;
            self.invalidate_preview(true);
            if same_session {
                self.preview_scroll = previous_scroll;
                self.preview_manual_scroll = manual_scroll;
            }
            self.preview_target = selected;
            self.preview_due = self
                .preview_target
                .as_ref()
                .map(|_| Instant::now() + Duration::from_millis(90));
            self.dirty = true;
        }
    }

    fn request_preview(&mut self) {
        if self.preview_busy.is_some()
            || self.opening
            || !self.preview_visible
            || !self.preview_due.is_some_and(|due| due <= Instant::now())
        {
            return;
        }
        let Some(session) = self.preview_target.clone() else {
            return;
        };
        let client = self.client.independent_requests();
        self.preview_generation = self.preview_generation.wrapping_add(1);
        let generation = self.preview_generation;
        self.preview_due = None;
        self.preview_busy = Some(PreviewRequest {
            generation,
            client: client.clone(),
        });
        let sender = self.sender.clone();
        thread::spawn(move || {
            let result = client.read_screen(&session);
            let _ = sender.send(Update::Preview(Box::new(PreviewResponse {
                generation,
                session,
                result,
            })));
        });
        self.dirty = true;
    }

    fn scroll_preview(&mut self, delta: isize) {
        let Some(capture) = &self.preview_capture else {
            return;
        };
        let limit = capture.document.lines.len().saturating_sub(1);
        self.preview_scroll = self.preview_scroll.saturating_add_signed(delta).min(limit);
        self.preview_manual_scroll = true;
        self.dirty = true;
    }

    fn query_changed(&mut self) {
        self.state.set_query(self.editor.text.clone());
        self.preview_manual_scroll = false;
        self.sync_preview();
        self.index_due = self
            .state
            .needs_contents()
            .then(|| Instant::now() + Duration::from_millis(100));
        self.rebuild = true;
        self.reveal = true;
        self.dirty = true;
    }

    fn move_selection(&mut self, delta: isize) {
        self.state.move_selection(delta);
        self.sync_preview();
        self.reveal = true;
        self.dirty = true;
    }

    fn move_group(&mut self, delta: isize, level: GroupLevel) {
        self.state.move_target(delta, level);
        self.sync_preview();
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
        let client = self.client.clone();
        self.opening = true;
        self.dirty = true;
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
            KeyCode::Tab if !self.opening => {
                self.invalidate_preview(true);
                self.preview_visible = !self.preview_visible;
                self.preview_target = None;
                self.sync_preview();
                self.rebuild = true;
                self.dirty = true;
                return;
            }
            KeyCode::Up if alt && self.preview_visible => {
                self.scroll_preview(-3);
                return;
            }
            KeyCode::Down if alt && self.preview_visible => {
                self.scroll_preview(3);
                return;
            }
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
                self.invalidate_preview(false);
                self.sync_preview();
                self.preview_due = self.preview_target.as_ref().map(|_| Instant::now());
                self.dirty = true;
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
            KeyCode::Enter if self.search_focus => {
                self.search_focus = false;
                self.dirty = true;
            }
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
        self.sync_preview();
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
            if self.geometry.preview.contains(point.into()) {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_preview(-3),
                    MouseEventKind::ScrollDown => self.scroll_preview(3),
                    _ => {}
                }
            }
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
        self.sync_preview();
        self.dirty = true;
    }

    fn notice(&self) -> String {
        if self.opening {
            self.messages.get("opening").to_owned()
        } else if !self.error.is_empty() {
            self.error.clone()
        } else if let Some(error) = &self.state.query_error {
            error.clone()
        } else if self.state.selection_lost {
            self.messages.get("lost").to_owned()
        } else if self.state.needs_contents() && self.index_busy {
            self.messages.get("indexing").to_owned()
        } else if self.state.needs_contents() && self.content_error {
            self.messages.get("partial").to_owned()
        } else {
            String::new()
        }
    }

    fn draw(&mut self, frame: &mut Frame<'_>) {
        self.sync_preview();
        let area = frame.area();
        frame.render_widget(Block::default().style(self.theme.normal()), area);
        let notice = self.notice();
        let filter_height = u16::from(self.state.status.is_some());
        let notice_height = u16::from(!notice.is_empty());
        let available = area
            .height
            .saturating_sub(4 + filter_height + notice_height);
        let side_preview = self.preview_visible && area.width >= 105 && available >= 8;
        let bottom_preview =
            self.preview_visible && !side_preview && area.width >= 50 && available >= 16;
        let has_preview = side_preview || bottom_preview;
        let details_height = if has_preview {
            0
        } else if area.height >= 18 {
            4
        } else if area.height >= 12 {
            3
        } else if area.height >= 8 {
            2
        } else {
            0
        };
        let regions = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(filter_height),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(details_height),
            Constraint::Length(notice_height),
            Constraint::Length(1),
        ])
        .split(area);
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
        self.geometry.preview = Rect::default();
        if side_preview {
            let panes =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(regions[3]);
            self.draw_list(frame, panes[0]);
            self.draw_preview(frame, panes[1]);
        } else if bottom_preview {
            let panes = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(regions[3]);
            self.draw_list(frame, panes[0]);
            self.draw_preview(frame, panes[1]);
        } else {
            self.draw_list(frame, regions[3]);
        }
        self.geometry.details = padded(regions[5]);
        self.draw_selection_details(frame, self.geometry.details);
        frame.render_widget(
            Paragraph::new(notice).style(self.theme.accent()),
            padded(regions[6]),
        );
        let footer = if self.search_focus {
            "Enter browse  Esc browse  ↑↓ select  Tab preview  F1 help"
        } else if self.preview_visible && !has_preview {
            "Tab preview  Enlarge terminal for preview  / search  F1 help"
        } else if has_preview {
            "Tab preview  Alt+↑↓ scroll  / search  Enter open  F1 help"
        } else {
            "Tab preview  / search  Enter open  Esc back/close  F1 help"
        };
        frame.render_widget(
            Paragraph::new(footer).style(self.theme.muted()),
            padded(regions[7]),
        );
        if self.help {
            self.draw_help(frame, area);
        }
    }

    fn draw_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.geometry.preview = area;
        let title = self.state.selected().map_or_else(String::new, |row| {
            let label = if let Some(session) = &row.session {
                format!("{} / {}", row.workspace.label, session.title)
            } else if row.kind == RowKind::Tab {
                format!("{} / {}", row.workspace.label, row.tab_label)
            } else {
                row.workspace.label.clone()
            };
            format!(
                " {} ",
                render::middle(&label, area.width.saturating_sub(4) as usize)
            )
        });
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(self.theme.separator())
            .title(ratatui::text::Line::styled(title, self.theme.workspace()));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        let Some(row) = self.state.selected() else {
            return;
        };
        if row.session.is_none() {
            return;
        }
        let Some(target) = &self.preview_target else {
            return;
        };
        let capture = self
            .preview_capture
            .as_ref()
            .filter(|capture| same_preview_target(&capture.session, target));
        let refreshing = self
            .preview_busy
            .as_ref()
            .is_some_and(|request| request.generation == self.preview_generation);
        let status = if let Some(capture) = capture {
            format!(
                "{} read {}s ago{}",
                if capture.truncated {
                    "Screen excerpt,"
                } else {
                    "Screen,"
                },
                capture.read_at.elapsed().as_secs(),
                if refreshing { ", refreshing" } else { "" }
            )
        } else if self.preview_error.is_some() {
            "Screen unavailable. Ctrl+R retries.".to_owned()
        } else if !row.snippets.is_empty() {
            "Search excerpt, loading selected screen...".to_owned()
        } else {
            "Loading selected screen...".to_owned()
        };
        let details_height = if inner.height >= 10 {
            4
        } else if inner.height >= 7 {
            3
        } else {
            2
        };
        let details = render::selection_lines(
            row,
            inner.width as usize,
            details_height as usize + 1,
            self.theme,
        )
        .into_iter()
        .skip(1)
        .collect::<Vec<_>>();
        let parts = Layout::vertical([
            Constraint::Length(details.len() as u16),
            Constraint::Length(1),
            Constraint::Length(u16::from(self.preview_error.is_some())),
            Constraint::Min(0),
        ])
        .split(inner);
        frame.render_widget(Paragraph::new(details), parts[0]);
        frame.render_widget(
            Paragraph::new(render::clip(&status, parts[1].width as usize))
                .style(self.theme.muted()),
            parts[1],
        );
        if let Some(error) = &self.preview_error {
            let prefix = if capture.is_some() {
                "Refresh failed"
            } else {
                "Read failed"
            };
            let message = format!("{prefix}: {}", error.message);
            frame.render_widget(
                Paragraph::new(render::clip(&message, parts[2].width as usize))
                    .style(self.theme.accent()),
                parts[2],
            );
        }
        let Some(capture) = capture else {
            let lines = row
                .snippets
                .iter()
                .take(parts[3].height as usize)
                .map(|line| {
                    render::preview_line(
                        line,
                        &row.query_terms,
                        parts[3].width as usize,
                        self.theme,
                    )
                })
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(lines), parts[3]);
            return;
        };
        let document = &capture.document;
        if document.text.is_empty() {
            frame.render_widget(
                Paragraph::new("The visible screen is empty.").style(self.theme.muted()),
                parts[3],
            );
            return;
        }
        let terms = row.query_terms.as_slice();
        if !self.preview_manual_scroll && parts[3].height > 0 {
            let tail = document
                .lines
                .iter()
                .rposition(|line| !line.trim().is_empty())
                .map_or(0, |line| {
                    line.saturating_add(1)
                        .saturating_sub(parts[3].height as usize)
                });
            self.preview_scroll = document
                .lines
                .iter()
                .position(|line| !highlight_spans_terms(line, terms).is_empty())
                .map_or(tail, |line| {
                    line.saturating_sub(parts[3].height as usize / 2)
                });
        }
        self.preview_scroll = self.preview_scroll.min(
            document
                .lines
                .len()
                .saturating_sub(parts[3].height as usize),
        );
        let lines = document
            .lines
            .iter()
            .skip(self.preview_scroll)
            .take(parts[3].height as usize)
            .map(|line| render::preview_line(line, terms, parts[3].width as usize, self.theme))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), parts[3]);
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

    fn draw_selection_details(&self, frame: &mut Frame<'_>, area: Rect) {
        if let Some(row) = self.state.selected() {
            frame.render_widget(
                Paragraph::new(render::selection_lines(
                    row,
                    area.width as usize,
                    area.height as usize,
                    self.theme,
                )),
                area,
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
        if let Some(request) = &self.preview_busy {
            request.client.cancel();
        }
        self.client.cancel();
    }
}

pub fn run(client: Client) -> Result<(), Box<dyn Error>> {
    run_app(App::new(client, true))
}

fn run_app(mut app: App) -> Result<(), Box<dyn Error>> {
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
        app.preview_visible = false;
        app
    }

    fn paint(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn preview_title_follows_native_context_and_group_selection_keeps_a_blank_frame() {
        let mut app = app();
        app.preview_visible = true;
        let terminal = paint(&mut app, 120, 32);
        let preview = app.geometry.preview;
        assert!(screen(&terminal).contains("App / Fix payment routing"));
        assert!(!screen(&terminal).contains("Selected session"));
        for (key, title) in [("w:a", "App"), ("t:ta", "App / API")] {
            assert!(app.state.select(key));
            let terminal = paint(&mut app, 120, 32);
            assert_eq!(app.geometry.preview, preview);
            let text = screen(&terminal);
            assert!(
                text.lines()
                    .nth(preview.y as usize)
                    .unwrap()
                    .contains(title)
            );
            assert!(!text.contains("Enter or Space"));
            for y in preview.y + 1..preview.bottom() - 1 {
                for x in preview.x + 1..preview.right() - 1 {
                    assert_eq!(terminal.backend().buffer()[(x, y)].symbol(), " ");
                }
            }
        }
    }

    #[test]
    fn fuzzy_title_matches_keep_their_color_on_the_selected_row() {
        let mut app = app();
        app.theme = Theme::new(true);
        app.editor.insert("pymnt");
        app.query_changed();
        paint(&mut app, 120, 32);
        let line = app
            .lines
            .iter()
            .find(|line| line.key.as_deref() == Some("s:one"))
            .unwrap();
        let highlighted: String = line
            .text
            .spans
            .iter()
            .filter(|span| {
                span.style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::UNDERLINED)
            })
            .map(|span| {
                assert_eq!(span.style.fg, app.theme.highlight().fg);
                span.content.as_ref()
            })
            .collect();
        assert_eq!(highlighted, "pymnt");
        assert_ne!(app.theme.highlight().fg, app.theme.normal().fg);
    }

    #[test]
    fn enter_finishes_search_without_opening_and_a_second_press_opens_the_selection() {
        let mut app = app();
        press(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
        app.editor.insert("payment");
        app.query_changed();
        let selected = app.state.selected_key().map(str::to_owned);
        assert!(screen(&paint(&mut app, 120, 32)).contains("Enter browse"));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.search_focus && !app.opening && !app.quit);
        assert_eq!(app.state.query, "payment");
        assert_eq!(app.state.selected_key(), selected.as_deref());
        let mut repeat = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        repeat.kind = event::KeyEventKind::Repeat;
        app.key(repeat);
        assert!(!app.opening && !app.quit);
        assert!(screen(&paint(&mut app, 120, 32)).contains("Enter open"));
        press(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.opening && !app.quit);
    }

    #[test]
    fn default_view_keeps_grouped_rows_and_enables_preview() {
        let mut app = App::new(
            Client::new("/nonexistent/finder-default", Duration::from_millis(5)).unwrap(),
            false,
        );
        assert!(app.preview_visible);
        app.state.update(fixture());
        paint(&mut app, 120, 32);
        assert!(!app.geometry.preview.is_empty());
        assert!(app.geometry.details.is_empty());
        assert!(
            app.state
                .rows()
                .iter()
                .any(|row| row.kind == RowKind::Workspace)
        );
        assert!(app.state.rows().iter().any(|row| row.kind == RowKind::Tab));
        for key in ["s:one", "s:two", "s:three"] {
            assert_eq!(
                app.lines
                    .iter()
                    .filter(|line| line.key.as_deref() == Some(key))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn metadata_only_and_invalid_queries_do_not_index_all_screens() {
        let mut app = app();
        for query in [
            "agent:codex",
            "status:working",
            "path:shared",
            "screen:\"unfinished",
        ] {
            app.state.set_query(query.into());
            assert!(!app.state.needs_contents());
            app.request_contents();
            assert!(!app.index_busy);
        }
        app.state.set_query("payment".into());
        assert!(app.state.needs_contents());
    }

    #[test]
    fn fuzzy_search_preserves_workspace_and_tab_headers_with_compact_sessions() {
        let mut app = app();
        app.editor.insert("pymnt");
        app.query_changed();
        paint(&mut app, 80, 32);
        assert!(
            app.state
                .rows()
                .iter()
                .any(|row| row.kind == RowKind::Workspace && row.workspace.label == "App")
        );
        for key in ["t:ta", "t:tb", "s:one", "s:two"] {
            assert_eq!(
                app.lines
                    .iter()
                    .filter(|line| line.key.as_deref() == Some(key))
                    .count(),
                1
            );
        }
        assert!(
            app.lines
                .iter()
                .all(|line| line.key.as_deref() != Some("s:three"))
        );
    }

    #[test]
    fn preview_reflows_right_below_and_hidden_on_small_terminals() {
        let mut app = preview_app();
        paint(&mut app, 120, 32);
        assert!(app.geometry.preview.x > app.geometry.list.x);
        assert_eq!(app.geometry.preview.y, app.geometry.list.y);
        assert!(app.geometry.details.is_empty());
        paint(&mut app, 80, 40);
        assert!(app.geometry.preview.y > app.geometry.list.y);
        assert_eq!(app.geometry.preview.x, app.geometry.list.x);
        paint(&mut app, 30, 12);
        assert!(app.geometry.preview.is_empty());
        assert!(!app.geometry.details.is_empty());
        for (width, height) in [(0, 0), (1, 1), (8, 6), (40, 12)] {
            paint(&mut app, width, height);
        }
    }

    fn capture(text: &str, revision: u64, truncated: bool) -> ScreenCapture {
        ScreenCapture {
            text: text.into(),
            revision,
            truncated,
        }
    }

    fn preview_response(
        generation: u64,
        session: Session,
        result: Result<ScreenCapture, FinderError>,
    ) -> Update {
        Update::Preview(Box::new(PreviewResponse {
            generation,
            session,
            result,
        }))
    }

    fn preview_app() -> App {
        let mut app = app();
        app.preview_visible = true;
        app.sync_preview();
        app
    }

    fn mark_preview_busy(app: &mut App) -> Client {
        let client = app.client.independent_requests();
        app.preview_busy = Some(PreviewRequest {
            generation: app.preview_generation,
            client: client.clone(),
        });
        app.preview_due = None;
        client
    }

    #[test]
    fn preview_drops_responses_for_old_selection_address_and_revision() {
        let mut app = preview_app();
        let origin = app.preview_target.clone().unwrap();
        let generation = app.preview_generation;
        mark_preview_busy(&mut app);
        app.state.select("s:two");
        app.sync_preview();
        app.receive(preview_response(
            generation,
            origin,
            Ok(capture("old secret screen", 17, false)),
        ));
        assert!(app.preview_busy.is_none());
        assert!(app.preview_capture.is_none());
        let current = app.preview_target.clone().unwrap();
        let generation = app.preview_generation;
        let mut wrong_address = current.clone();
        wrong_address.pane_id = "moved".into();
        app.receive(preview_response(
            generation,
            wrong_address,
            Ok(capture("wrong address", 17, false)),
        ));
        let mut wrong_revision = current.clone();
        wrong_revision.revision += 1;
        app.receive(preview_response(
            generation,
            wrong_revision,
            Ok(capture("wrong revision", 17, false)),
        ));
        assert!(app.preview_capture.is_none());
        app.receive(preview_response(
            generation,
            current,
            Ok(capture("current selected screen", 17, false)),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("current selected screen"));
        assert!(!text.contains("rev 17"));
        assert!(!text.contains("old secret screen"));
        assert!(!text.contains("wrong address") && !text.contains("wrong revision"));
    }

    #[test]
    fn current_preview_updates_search_evidence_and_vanished_match_becomes_inert() {
        let mut app = preview_app();
        let origin = app.state.snapshot.as_ref().unwrap().sessions.clone();
        app.state.set_query("screen:needle".into());
        app.state.set_contents(
            HashMap::from([("one".into(), ContentDocument::new("old needle evidence"))]),
            &origin,
        );
        app.sync_preview();
        let selected = app.preview_target.clone().unwrap();
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture("new needle evidence", 2, false)),
        ));
        assert_eq!(
            app.state.content(&selected).unwrap().text,
            "new needle evidence"
        );
        assert!(
            app.state
                .selected()
                .unwrap()
                .snippets
                .iter()
                .any(|line| line.contains("new needle evidence"))
        );
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture("screen no longer matches", 3, false)),
        ));
        assert!(app.state.selected().is_none() && app.state.selection_lost);
        assert!(app.preview_target.is_none());
        app.activate();
        assert!(!app.opening);
    }

    #[test]
    fn preview_generation_rejects_a_to_b_to_a_without_cancelling_main_client() {
        use std::sync::atomic::Ordering;
        let mut app = preview_app();
        let origin = app.preview_target.clone().unwrap();
        let old_generation = app.preview_generation;
        let independent = mark_preview_busy(&mut app);
        app.state.select("s:two");
        app.sync_preview();
        app.state.select("s:one");
        app.sync_preview();
        assert!(app.preview_generation > old_generation);
        assert!(independent.cancellation().load(Ordering::Acquire));
        assert!(!app.client.cancellation().load(Ordering::Acquire));
        app.receive(preview_response(
            old_generation,
            origin.clone(),
            Ok(capture("obsolete A response", 1, false)),
        ));
        assert!(app.preview_capture.is_none());
        app.receive(preview_response(
            app.preview_generation,
            origin,
            Ok(capture("new A response", 2, false)),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("new A response"));
        assert!(!text.contains("obsolete A response"));
    }

    #[test]
    fn hidden_and_reopened_preview_rejects_previous_generation() {
        let mut app = preview_app();
        let origin = app.preview_target.clone().unwrap();
        let generation = app.preview_generation;
        mark_preview_busy(&mut app);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        app.receive(preview_response(
            generation,
            origin.clone(),
            Ok(capture("hidden response", 1, false)),
        ));
        assert!(app.preview_capture.is_none());
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.preview_generation > generation);
        app.receive(preview_response(
            generation,
            origin.clone(),
            Ok(capture("reopened stale response", 1, false)),
        ));
        assert!(app.preview_capture.is_none());
        app.receive(preview_response(
            app.preview_generation,
            origin,
            Ok(capture("reopened current response", 2, false)),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("reopened current response"));
        assert!(!text.contains("hidden response") && !text.contains("reopened stale response"));
    }

    #[test]
    fn preview_debounces_selection_and_distinguishes_loading_unavailable_and_empty() {
        let mut app = preview_app();
        app.request_preview();
        assert!(app.preview_busy.is_none());
        assert!(screen(&paint(&mut app, 120, 32)).contains("Loading selected screen"));
        let selected = app.preview_target.clone().unwrap();
        let generation = app.preview_generation;
        app.receive(preview_response(
            generation,
            selected.clone(),
            Err(FinderError::new("timeout", "Read timed out")),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("Screen unavailable") && text.contains("Read timed out"));
        app.receive(preview_response(
            generation,
            selected,
            Ok(capture("", 1, false)),
        ));
        assert!(screen(&paint(&mut app, 120, 32)).contains("visible screen is empty"));
    }

    #[test]
    fn failed_refresh_keeps_capture_age_and_labels_screen_excerpt() {
        let mut app = preview_app();
        let selected = app.preview_target.clone().unwrap();
        let generation = app.preview_generation;
        app.receive(preview_response(
            generation,
            selected.clone(),
            Ok(capture("kept screen evidence", 42, true)),
        ));
        let captured_at = Instant::now() - Duration::from_secs(9);
        app.preview_capture.as_mut().unwrap().read_at = captured_at;
        app.invalidate_preview(false);
        assert!(app.preview_generation > generation);
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Err(FinderError::new("timeout", "Retry timed out")),
        ));
        assert_eq!(app.preview_capture.as_ref().unwrap().read_at, captured_at);
        let text = screen(&paint(&mut app, 140, 32));
        assert!(text.contains("Screen excerpt, read") && text.contains("s ago"));
        assert!(text.contains("Refresh failed: Retry timed out"));
        assert!(text.contains("kept screen evidence"));
    }

    #[test]
    fn preview_shortcuts_scroll_and_toggle_without_editing_the_query() {
        let mut app = preview_app();
        app.editor.insert("api");
        app.query_changed();
        let selected = app.state.selected().unwrap().session.clone().unwrap();
        let contents = (0..80)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture(&contents, 0, false)),
        ));
        paint(&mut app, 120, 32);
        let initial_scroll = app.preview_scroll;
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(app.preview_scroll, initial_scroll - 3);
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        paint(&mut app, 120, 32);
        assert!(app.geometry.preview.is_empty());
        assert!(!app.geometry.details.is_empty());
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        paint(&mut app, 120, 32);
        assert!(!app.geometry.preview.is_empty());
        assert_eq!(app.state.query, "api");
        assert_eq!(app.editor.text, "api");
    }

    #[test]
    fn preview_follows_latest_output_until_the_user_scrolls() {
        let mut app = preview_app();
        let selected = app.preview_target.clone().unwrap();
        app.scroll_preview(3);
        assert!(!app.preview_manual_scroll);
        let contents = |count: usize, marker: &str| {
            (0..count)
                .map(|index| {
                    if index + 1 == count {
                        marker.to_owned()
                    } else {
                        format!("output line {index}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture(&contents(80, "latest initial output"), 0, false)),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("latest initial output"));
        let initial_scroll = app.preview_scroll;
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture(&contents(90, "latest refreshed output"), 0, false)),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains("latest refreshed output"));
        assert!(app.preview_scroll > initial_scroll);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        let manual_scroll = app.preview_scroll;
        press(&mut app, KeyCode::Char('r'), KeyModifiers::CONTROL);
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture(
                &contents(100, "new output after manual scroll"),
                0,
                false,
            )),
        ));
        let text = screen(&paint(&mut app, 120, 32));
        assert_eq!(app.preview_scroll, manual_scroll);
        assert!(!text.contains("new output after manual scroll"));
        app.editor.insert("title:payment");
        app.query_changed();
        let text = screen(&paint(&mut app, 120, 32));
        assert!(!app.preview_manual_scroll);
        assert!(text.contains("new output after manual scroll"));
        let area = app.geometry.preview;
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: area.x + 1,
            row: area.y + 1,
            modifiers: KeyModifiers::NONE,
        });
        let wheel_scroll = app.preview_scroll;
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture(
                &contents(110, "new output after wheel scroll"),
                0,
                false,
            )),
        ));
        paint(&mut app, 120, 32);
        assert!(app.preview_manual_scroll);
        assert_eq!(app.preview_scroll, wheel_scroll);
    }

    #[test]
    fn preview_centers_literal_evidence_once_and_preserves_manual_scroll_on_refresh() {
        let mut app = preview_app();
        let selected = app.preview_target.clone().unwrap();
        let text = (0..80)
            .map(|index| {
                if index == 50 {
                    "needle evidence".to_owned()
                } else {
                    format!("line {index}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        app.state.set_contents(
            HashMap::from([(selected.terminal_id.clone(), ContentDocument::new(&text))]),
            std::slice::from_ref(&selected),
        );
        app.editor.insert("needle");
        app.query_changed();
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture(&text, 7, false)),
        ));
        assert!(screen(&paint(&mut app, 120, 32)).contains("needle evidence"));
        assert!(app.preview_scroll > 0);
        press(&mut app, KeyCode::Up, KeyModifiers::ALT);
        let manual = app.preview_scroll;
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture(&text, 8, false)),
        ));
        paint(&mut app, 120, 32);
        assert_eq!(app.preview_scroll, manual);
        let mut snapshot = app.state.snapshot.clone().unwrap();
        snapshot
            .sessions
            .iter_mut()
            .find(|session| session.terminal_id == "one")
            .unwrap()
            .revision += 1;
        app.state.update(snapshot);
        app.sync_preview();
        let revised = app.preview_target.clone().unwrap();
        app.receive(preview_response(
            app.preview_generation,
            revised,
            Ok(capture(&text, 9, false)),
        ));
        paint(&mut app, 120, 32);
        assert_eq!(app.preview_scroll, manual);
    }

    #[test]
    fn refreshing_preview_does_not_enqueue_a_second_read_for_the_same_capture() {
        let mut app = preview_app();
        let selected = app.preview_target.clone().unwrap();
        app.receive(preview_response(
            app.preview_generation,
            selected.clone(),
            Ok(capture("cached screen", 1, false)),
        ));
        mark_preview_busy(&mut app);
        app.sync_preview();
        assert!(app.preview_due.is_none());
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture("fresh screen", 2, false)),
        ));
        app.sync_preview();
        assert!(app.preview_busy.is_none());
        assert!(app.preview_due.is_some_and(|due| due > Instant::now()));
    }

    #[test]
    fn invalid_query_is_visible_and_does_not_start_screen_indexing() {
        let mut app = app();
        app.editor.insert("screen:\"unfinished");
        app.query_changed();
        assert!(app.state.query_error.is_some());
        assert!(app.state.rows().is_empty());
        let notice = app.notice();
        assert!(!notice.is_empty());
        let text = screen(&paint(&mut app, 120, 32));
        assert!(text.contains(&notice));
        app.request_contents();
        assert!(!app.index_busy);
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
    fn narrow_popup_keeps_selected_details_inside_padded_cells() {
        let mut app = app();
        let mut snapshot = fixture();
        snapshot.sessions[0].title = format!("{}界", "x".repeat(30));
        snapshot.sessions[0].cwd = "/project/payment".into();
        app.state.update(snapshot);
        let terminal = paint(&mut app, 35, 17);
        let buffer = terminal.backend().buffer();
        assert!(screen(&terminal).contains("/project/payment"));
        for y in app.geometry.details.y..app.geometry.details.bottom() {
            for x in 2..33 {
                assert!(x as usize + render::width(buffer[(x, y)].symbol()) <= 33);
            }
            for x in [0, 1, 33, 34] {
                assert_eq!(buffer[(x, y)].symbol(), " ");
            }
        }
    }

    #[test]
    fn only_selected_session_shows_path_reason_and_screen_evidence() {
        let mut app = preview_app();
        let mut snapshot = fixture();
        snapshot.sessions[0].cwd = "/selected/payment".into();
        snapshot.sessions[2].cwd = "/unselected/logs".into();
        app.state.update(snapshot.clone());
        app.state.set_query("screen:needle".into());
        app.state.set_contents(
            HashMap::from([
                (
                    "one".into(),
                    ContentDocument::new("selected needle evidence"),
                ),
                (
                    "three".into(),
                    ContentDocument::new("unselected needle evidence"),
                ),
            ]),
            &snapshot.sessions,
        );
        app.state.select("s:one");
        app.sync_preview();
        let selected = app.state.selected().unwrap().session.clone().unwrap();
        app.receive(preview_response(
            app.preview_generation,
            selected,
            Ok(capture("selected needle evidence", 0, false)),
        ));
        let terminal = paint(&mut app, 120, 40);
        let text = screen(&terminal);
        assert!(text.contains("/selected/payment"));
        assert!(text.contains("selected needle evidence"));
        assert!(!text.contains("/unselected/logs") && !text.contains("unselected needle evidence"));
        assert!(
            app.lines
                .iter()
                .all(|line| !line.text.to_string().contains("evidence")
                    && !line.text.to_string().contains("/selected/payment"))
        );
        assert!(terminal.backend().buffer().content.iter().any(|cell| {
            cell.modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        }));
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        let text = screen(&paint(&mut app, 80, 24));
        assert!(text.contains("/selected/payment") && text.contains("needle"));
        assert!(app.geometry.preview.is_empty() && !app.geometry.details.is_empty());
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
    fn session_details_omit_missing_activity_metadata() {
        let mut app = app();
        let text = screen(&paint(&mut app, 80, 24));
        assert!(text.contains("/shared"));
        assert!(!text.contains("Date unlinked") && !text.contains("Date unavailable"));
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
    fn tree_rows_keep_counts_and_badges_without_session_metadata_lines() {
        let mut app = app();
        app.theme = Theme::new(true);
        let mut snapshot = fixture();
        snapshot.sessions[0].activity_at =
            Some(Utc.with_ymd_and_hms(2026, 11, 8, 7, 43, 0).unwrap());
        app.state.update(snapshot);
        paint(&mut app, 100, 40);
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
        for key in ["s:one", "s:two", "s:three"] {
            assert_eq!(
                app.lines
                    .iter()
                    .filter(|line| line.key.as_deref() == Some(key))
                    .count(),
                1
            );
        }
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
        assert!(app.lines.iter().all(|line| {
            let text = line.text.to_string();
            !text.contains("2026-11-08")
                && !text.contains("/shared")
                && !text.contains("builder")
                && !text.contains("reviewer")
        }));
    }

    #[test]
    fn terminal_resize_keeps_rows_compact_and_selected_literal_evidence_visible() {
        let mut app = app();
        app.state.set_query("screen:needle".into());
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
            let terminal = paint(&mut app, width, 24);
            assert!(
                app.lines
                    .iter()
                    .all(|line| line.text.width() <= app.geometry.list.width as usize)
            );
            assert_eq!(
                app.lines
                    .iter()
                    .filter(|line| line.key.as_deref() == Some("s:one"))
                    .count(),
                1
            );
            assert!(terminal.backend().buffer().content.iter().any(|cell| {
                cell.modifier
                    .contains(Modifier::BOLD | Modifier::UNDERLINED)
            }));
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
        app.receive(Update::Snapshot(Ok(snapshot.clone()), None));
        paint(&mut app, 80, 24);
        assert_eq!(app.scroll, 30);
        snapshot.sessions.remove(0);
        app.receive(Update::Snapshot(Ok(snapshot), None));
        assert!(app.state.selected().is_none());
        app.activate();
        assert!(!app.opening);
    }

    #[test]
    fn late_body_match_is_selected_and_replaced_terminal_content_is_discarded() {
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
        stale.kind = "claude".into();
        stale.native_reference = Some(("claude".into(), "id".into(), "old".into()));
        app.receive(Update::Contents(
            vec![stale],
            HashMap::from([(
                "one".into(),
                ContentDocument::new("bodyneedle replaced terminal"),
            )]),
        ));
        assert!(app.state.rows().iter().all(|row| row.key != "s:one"));
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
    fn spacer_at_viewport_top_keeps_following_group_during_metadata_refresh() {
        let mut app = app();
        let mut snapshot = fixture();
        let seeds = snapshot.sessions.clone();
        snapshot.sessions = (0..90)
            .map(|index| Session {
                terminal_id: format!("term-{index}"),
                pane_id: format!("pane-{index}"),
                focused: index == 0,
                ..seeds[index / 30].clone()
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
        let following = app.lines[app.scroll + 1].key.clone();
        snapshot.sessions[0].activity_at = Some(Utc::now());
        app.receive(Update::Snapshot(Ok(snapshot), None));
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
