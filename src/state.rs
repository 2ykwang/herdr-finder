//! Cached native hierarchy, filtering and terminal-identity selection.

use crate::model::{Session, Snapshot, Workspace};
use crate::provider;
use crate::search::{ContentDocument, SearchMatch, find_matches_terms, query_terms};
use chrono::{DateTime, Utc};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RowKind {
    #[default]
    Workspace,
    Tab,
    Session,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupLevel {
    #[default]
    Workspace,
    Tab,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub workspace: Workspace,
    pub session: Option<Session>,
    pub kind: RowKind,
    pub tab_id: String,
    pub tab_label: String,
    pub tab_number: usize,
    pub count: usize,
    pub total: usize,
    pub collapsed: bool,
    pub query_terms: Vec<String>,
    pub snippets: Vec<String>,
    pub snippet_spans: Vec<Vec<(usize, usize)>>,
    pub activity_at: Option<DateTime<Utc>>,
    pub separator_before: bool,
    pub body_hit: bool,
}

#[derive(Clone, Debug)]
struct TabNode {
    id: String,
    label: String,
    number: usize,
    sessions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    kind: String,
    reference: Option<(String, String, String)>,
}

impl From<&Session> for Identity {
    fn from(session: &Session) -> Self {
        Self {
            kind: session.kind.clone(),
            reference: session.native_reference.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct CachedContent {
    identity: Identity,
    revision: u64,
    document: ContentDocument,
}

#[derive(Clone, Debug)]
struct Metadata {
    values: Vec<String>,
    document: OnceCell<ContentDocument>,
}

#[derive(Default)]
struct TargetNavigation {
    targets: Vec<usize>,
    previous: Vec<Option<usize>>,
    next: Vec<Option<usize>>,
}

impl TargetNavigation {
    fn from_rows(rows: &[Row], level: GroupLevel, querying: bool) -> Self {
        let mut targets = Vec::new();
        let mut anchors = Vec::new();
        let mut owners = vec![None; rows.len()];
        let mut add_group = |start: usize, end: usize| {
            let group = &rows[start..end];
            let pane = group.iter().position(|row| row.kind == RowKind::Session);
            if querying && pane.is_none() {
                return;
            }
            let landing = pane
                .or_else(|| group.iter().position(|row| row.kind == RowKind::Tab))
                .unwrap_or(0);
            let index = targets.len();
            targets.push(start + landing);
            anchors.push(start);
            owners[start..end].fill(Some(index));
        };
        let workspaces: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| (row.kind == RowKind::Workspace).then_some(index))
            .chain(std::iter::once(rows.len()))
            .collect();
        for workspace in workspaces.windows(2) {
            let (start, end) = (workspace[0], workspace[1]);
            if level == GroupLevel::Workspace {
                add_group(start, end);
                continue;
            }
            let tabs: Vec<usize> = (start + 1..end)
                .filter(|&index| rows[index].kind == RowKind::Tab)
                .chain(std::iter::once(end))
                .collect();
            if tabs.len() == 1 {
                // A folded or empty workspace remains reachable without exposing its children.
                add_group(start, end);
            } else {
                for tab in tabs.windows(2) {
                    add_group(tab[0], tab[1]);
                }
            }
        }
        let mut previous = Vec::with_capacity(rows.len());
        let mut next = Vec::with_capacity(rows.len());
        let mut following = 0;
        for (row, owner) in owners.into_iter().enumerate() {
            while following < anchors.len() && anchors[following] < row {
                following += 1;
            }
            if let Some(owner) = owner {
                previous.push(owner.checked_sub(1));
                next.push((owner + 1 < targets.len()).then_some(owner + 1));
            } else {
                // Expanded workspace headers sit between tab targets, rather than owning one.
                previous.push(following.checked_sub(1));
                next.push((following < targets.len()).then_some(following));
            }
        }
        Self {
            targets,
            previous,
            next,
        }
    }

    fn destination(&self, selected: Option<usize>, delta: isize) -> Option<usize> {
        if delta == 0 || self.targets.is_empty() {
            return None;
        }
        let first = match selected {
            Some(row) if delta > 0 => self.next[row]?,
            Some(row) => self.previous[row]?,
            None if delta > 0 => 0,
            None => self.targets.len() - 1,
        };
        let remaining = delta.unsigned_abs() - 1;
        let index = if delta > 0 {
            first.saturating_add(remaining).min(self.targets.len() - 1)
        } else {
            first.saturating_sub(remaining)
        };
        Some(self.targets[index])
    }
}

#[derive(Default)]
pub struct FinderState {
    pub snapshot: Option<Snapshot>,
    pub query: String,
    pub status: Option<String>,
    pub collapsed: HashSet<String>,
    pub collapsed_tabs: HashSet<String>,
    pub selection_lost: bool,
    selected_key: Option<String>,
    terms: Vec<String>,
    rows: Vec<Row>,
    indices: HashMap<String, usize>,
    selectable_rows: Vec<usize>,
    selectable_positions: HashMap<String, usize>,
    workspace_targets: TargetNavigation,
    tab_targets: TargetNavigation,
    sessions: HashMap<String, Session>,
    tabs: HashMap<String, Vec<TabNode>>,
    metadata: HashMap<String, Metadata>,
    workspace_search: HashMap<String, ContentDocument>,
    contents: HashMap<String, CachedContent>,
}

impl FinderState {
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn is_filtering(&self) -> bool {
        !self.terms.is_empty()
    }

    pub fn selected_key(&self) -> Option<&str> {
        self.selected_key.as_deref()
    }

    pub fn row_index(&self, key: &str) -> Option<usize> {
        self.indices.get(key).copied()
    }

    pub fn selected(&self) -> Option<&Row> {
        self.selected_key()
            .and_then(|key| self.row_index(key))
            .and_then(|index| self.rows.get(index))
    }

    fn index_snapshot(&mut self, snapshot: &Snapshot) {
        self.sessions = snapshot
            .sessions
            .iter()
            .map(|session| (session.terminal_id.clone(), session.clone()))
            .collect();
        let mut members: HashMap<String, Vec<String>> = HashMap::new();
        let mut fallback_order = Vec::new();
        for session in &snapshot.sessions {
            if !members.contains_key(&session.tab_id) {
                fallback_order.push(session.tab_id.clone());
            }
            members
                .entry(session.tab_id.clone())
                .or_default()
                .push(session.terminal_id.clone());
        }
        let mut tabs: HashMap<String, Vec<TabNode>> = HashMap::new();
        if !snapshot.tabs.is_empty() {
            for tab in &snapshot.tabs {
                let bucket = tabs.entry(tab.workspace_id.clone()).or_default();
                let number = if tab.number > 0 {
                    tab.number
                } else {
                    bucket.len() + 1
                };
                bucket.push(TabNode {
                    id: tab.id.clone(),
                    label: if tab.label.is_empty() {
                        format!("Tab {number}")
                    } else {
                        tab.label.clone()
                    },
                    number,
                    sessions: members.remove(&tab.id).unwrap_or_default(),
                });
            }
        } else {
            // Synthetic snapshots without Tab models keep the first native membership order.
            for tab_id in fallback_order {
                let session_ids = members.remove(&tab_id).unwrap_or_default();
                let Some(session) = session_ids.first().and_then(|id| self.sessions.get(id)) else {
                    continue;
                };
                let bucket = tabs.entry(session.workspace_id.clone()).or_default();
                let number = if session.tab_number > 0 {
                    session.tab_number
                } else {
                    bucket.len() + 1
                };
                bucket.push(TabNode {
                    id: tab_id,
                    label: if session.tab_label.is_empty() {
                        format!("Tab {number}")
                    } else {
                        session.tab_label.clone()
                    },
                    number,
                    sessions: session_ids,
                });
            }
        }
        self.tabs = tabs;
        let mut metadata = HashMap::new();
        self.workspace_search.clear();
        for workspace in &snapshot.workspaces {
            self.workspace_search
                .insert(workspace.id.clone(), ContentDocument::new(&workspace.label));
            for tab in self.tabs.get(&workspace.id).into_iter().flatten() {
                for terminal_id in &tab.sessions {
                    let Some(session) = self.sessions.get(terminal_id) else {
                        continue;
                    };
                    let mut values = vec![
                        workspace.label.clone(),
                        tab.label.clone(),
                        session.title.clone(),
                        session.tab_label.clone(),
                        session.name.clone(),
                        session.kind.clone(),
                        session.display_kind.clone(),
                        session.cwd.clone(),
                    ];
                    // Friendly badge names remain searchable without changing native identity.
                    if let Some(provider) = provider::lookup(&session.kind) {
                        values.push(provider.label.to_owned());
                    }
                    values.extend(session.search_titles.clone());
                    let previous = self.metadata.remove(terminal_id);
                    let document = previous
                        .filter(|previous| previous.values == values)
                        .map(|previous| previous.document)
                        .unwrap_or_default();
                    metadata.insert(terminal_id.clone(), Metadata { values, document });
                }
            }
        }
        self.metadata = metadata;
        self.contents.retain(|id, content| {
            self.sessions
                .get(id)
                .is_some_and(|session| content.identity == Identity::from(session))
        });
    }

    fn metadata_document(&self, terminal_id: &str) -> Option<&ContentDocument> {
        self.metadata.get(terminal_id).map(|metadata| {
            metadata
                .document
                .get_or_init(|| ContentDocument::new(metadata.values.join(" ")))
        })
    }

    fn rebuild(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        // Filtering exposes descendants temporarily without changing the saved fold choices.
        let querying = !self.terms.is_empty();
        let empty = ContentDocument::default();
        let mut rows = Vec::new();
        for workspace in &snapshot.workspaces {
            let mut children = Vec::new();
            let mut count = 0;
            let mut total = 0;
            let mut visible_tabs = 0;
            for tab in self.tabs.get(&workspace.id).into_iter().flatten() {
                let mut matched: Vec<(&Session, Option<SearchMatch>)> = Vec::new();
                total += tab.sessions.len();
                for terminal_id in &tab.sessions {
                    let Some(session) = self.sessions.get(terminal_id) else {
                        continue;
                    };
                    if self
                        .status
                        .as_ref()
                        .is_some_and(|status| status != &session.status)
                    {
                        continue;
                    }
                    let found = if querying {
                        let Some(metadata) = self.metadata_document(terminal_id) else {
                            continue;
                        };
                        let body = self
                            .contents
                            .get(terminal_id)
                            .map_or(&empty, |value| &value.document);
                        let Some(found) = find_matches_terms(&self.terms, metadata, body) else {
                            continue;
                        };
                        Some(found)
                    } else {
                        None
                    };
                    matched.push((session, found));
                }
                let empty_match = tab.sessions.is_empty()
                    && self.status.is_none()
                    && (!querying
                        || find_matches_terms(
                            &self.terms,
                            &ContentDocument::new(format!("{} {}", workspace.label, tab.label)),
                            &empty,
                        )
                        .is_some());
                if matched.is_empty() && !empty_match {
                    continue;
                }
                count += matched.len();
                let collapsed = self.collapsed_tabs.contains(&tab.id) && !querying;
                children.push(Row {
                    key: format!("t:{}", tab.id),
                    workspace: workspace.clone(),
                    kind: RowKind::Tab,
                    tab_id: tab.id.clone(),
                    tab_label: tab.label.clone(),
                    tab_number: tab.number,
                    count: matched.len(),
                    total: tab.sessions.len(),
                    collapsed,
                    query_terms: self.terms.clone(),
                    separator_before: visible_tabs > 0,
                    ..Row::default()
                });
                visible_tabs += 1;
                if !collapsed {
                    let matched_count = matched.len();
                    for (session, found) in matched {
                        let found = found.unwrap_or_default();
                        children.push(Row {
                            key: format!("s:{}", session.terminal_id),
                            workspace: workspace.clone(),
                            session: Some(session.clone()),
                            kind: RowKind::Session,
                            tab_id: tab.id.clone(),
                            tab_label: tab.label.clone(),
                            tab_number: tab.number,
                            count: matched_count,
                            total: tab.sessions.len(),
                            query_terms: self.terms.clone(),
                            snippets: found.excerpt,
                            snippet_spans: found.highlights,
                            body_hit: found.body_hit,
                            activity_at: session.activity_at,
                            ..Row::default()
                        });
                    }
                }
            }
            let workspace_match = self.status.is_none()
                && (!querying
                    || self
                        .workspace_search
                        .get(&workspace.id)
                        .is_some_and(|metadata| {
                            find_matches_terms(&self.terms, metadata, &empty).is_some()
                        }));
            if children.is_empty() && !workspace_match {
                continue;
            }
            let collapsed = self.collapsed.contains(&workspace.id) && !querying;
            rows.push(Row {
                key: format!("w:{}", workspace.id),
                workspace: workspace.clone(),
                kind: RowKind::Workspace,
                count,
                total,
                collapsed,
                query_terms: self.terms.clone(),
                separator_before: !rows.is_empty(),
                ..Row::default()
            });
            if !collapsed {
                rows.extend(children);
            }
        }
        self.indices = rows
            .iter()
            .enumerate()
            .map(|(i, row)| (row.key.clone(), i))
            .collect();
        self.selectable_rows = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| (!querying || row.kind == RowKind::Session).then_some(index))
            .collect();
        self.selectable_positions = self
            .selectable_rows
            .iter()
            .enumerate()
            .map(|(position, &index)| (rows[index].key.clone(), position))
            .collect();
        self.workspace_targets =
            TargetNavigation::from_rows(&rows, GroupLevel::Workspace, querying);
        self.tab_targets = TargetNavigation::from_rows(&rows, GroupLevel::Tab, querying);
        self.rows = rows;
    }

    fn choose_first(&mut self) {
        self.selected_key = self
            .rows
            .iter()
            .find(|row| row.kind == RowKind::Session)
            .or_else(|| self.selectable_rows.first().map(|&index| &self.rows[index]))
            .map(|row| row.key.clone());
        self.selection_lost = false;
    }

    fn preserve_selection(&mut self) {
        // Refresh must not replace a vanished target with a neighboring terminal ready for Enter.
        if self.selected_key.is_some() && self.selected().is_none() {
            self.selected_key = None;
            self.selection_lost = true;
        } else if self.selected_key.is_none() && !self.selection_lost && !self.terms.is_empty() {
            self.choose_first();
        }
    }

    pub fn update(&mut self, snapshot: Snapshot) {
        if self.snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        let initial = self.snapshot.is_none();
        self.index_snapshot(&snapshot);
        let focused = snapshot
            .sessions
            .iter()
            .find(|session| session.focused)
            .map(|session| format!("s:{}", session.terminal_id));
        self.snapshot = Some(snapshot);
        self.rebuild();
        if initial {
            if let Some(key) = focused.filter(|key| self.indices.contains_key(key)) {
                self.selected_key = Some(key);
            } else {
                self.choose_first();
            }
        } else {
            self.preserve_selection();
        }
    }

    pub fn set_query(&mut self, query: String) {
        if query == self.query {
            return;
        }

        // Clearing a filter can hide the selected pane under its restored parent folds.
        let parent = self.selected().map(|row| {
            (
                format!("t:{}", row.tab_id),
                format!("w:{}", row.workspace.id),
            )
        });
        let was_lost = self.selection_lost;
        self.terms = query_terms(&query);
        self.query = query;
        self.rebuild();

        if self
            .selected_key()
            .is_some_and(|key| self.selectable_positions.contains_key(key))
        {
            return;
        }
        if was_lost {
            self.selected_key = None;
            return;
        }
        if !self.is_filtering()
            && let Some((tab, workspace)) = parent
            && (self.select(&tab) || self.select(&workspace))
        {
            return;
        }
        self.choose_first();
    }

    pub fn set_status(&mut self, status: Option<String>) {
        if status == self.status {
            return;
        }
        self.status = status;
        self.rebuild();
        if self.selected().is_none() && !self.selection_lost {
            self.choose_first();
        }
    }

    pub fn select(&mut self, key: &str) -> bool {
        if !self.selectable_positions.contains_key(key) {
            return false;
        }
        self.selected_key = Some(key.to_owned());
        self.selection_lost = false;
        true
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.selectable_rows.is_empty() || delta == 0 {
            return;
        }
        let current = self
            .selected_key()
            .and_then(|key| self.selectable_positions.get(key).copied())
            .map_or(
                if delta > 0 {
                    -1
                } else {
                    self.selectable_rows.len() as isize
                },
                |i| i as isize,
            );
        let index = current
            .saturating_add(delta)
            .clamp(0, self.selectable_rows.len() as isize - 1) as usize;
        let key = self.rows[self.selectable_rows[index]].key.clone();
        self.select(&key);
    }

    pub fn move_group(&mut self, delta: isize, level: GroupLevel) {
        self.move_target(delta, level);
    }

    /// Land on another group's first visible pane, or its folded/empty header.
    /// Tab jumps treat folded/empty workspaces as one stop and never change folds.
    pub fn move_target(&mut self, delta: isize, level: GroupLevel) -> bool {
        let navigation = match level {
            GroupLevel::Workspace => &self.workspace_targets,
            GroupLevel::Tab => &self.tab_targets,
        };
        let selected = self.selected_key().and_then(|key| self.row_index(key));
        let Some(index) = navigation.destination(selected, delta) else {
            return false;
        };
        let key = self.rows[index].key.clone();
        if self.selected_key() == Some(&key) {
            return false;
        }
        self.select(&key)
    }

    pub fn toggle_group(&mut self, level: Option<GroupLevel>) -> bool {
        if !self.terms.is_empty() {
            return false;
        }
        let Some(row) = self.selected() else {
            return false;
        };
        let level = level.unwrap_or(if row.kind == RowKind::Workspace {
            GroupLevel::Workspace
        } else {
            GroupLevel::Tab
        });
        let (identity, key) = match level {
            GroupLevel::Workspace => (row.workspace.id.clone(), format!("w:{}", row.workspace.id)),
            GroupLevel::Tab if !row.tab_id.is_empty() => {
                (row.tab_id.clone(), format!("t:{}", row.tab_id))
            }
            GroupLevel::Tab => return false,
        };
        let collapsed = match level {
            GroupLevel::Workspace => &mut self.collapsed,
            GroupLevel::Tab => &mut self.collapsed_tabs,
        };
        if !collapsed.remove(&identity) {
            collapsed.insert(identity);
        }
        self.rebuild();
        self.select(&key)
    }

    pub fn select_parent(&mut self) -> bool {
        if self.is_filtering() {
            return false;
        }
        let Some(row) = self.selected() else {
            return false;
        };
        let workspace = row.workspace.id.clone();
        let key = if row.kind == RowKind::Session {
            format!("t:{}", row.tab_id)
        } else {
            format!("w:{workspace}")
        };
        // Parent headers are already visible, including temporarily unfolded search groups.
        self.select(&key)
    }

    pub fn tree_left(&mut self) -> bool {
        let Some(row) = self.selected() else {
            return false;
        };
        if self.terms.is_empty() && row.kind != RowKind::Session && !row.collapsed {
            let level = if row.kind == RowKind::Workspace {
                GroupLevel::Workspace
            } else {
                GroupLevel::Tab
            };
            return self.toggle_group(Some(level));
        }
        let previous = self.selected_key.clone();
        self.select_parent();
        self.selected_key != previous
    }

    pub fn tree_right(&mut self) -> bool {
        let Some(row) = self.selected() else {
            return false;
        };
        if row.kind == RowKind::Session {
            return false;
        }
        if self.terms.is_empty() && row.collapsed {
            let level = if row.kind == RowKind::Workspace {
                GroupLevel::Workspace
            } else {
                GroupLevel::Tab
            };
            return self.toggle_group(Some(level));
        }
        let Some(index) = self.row_index(&row.key) else {
            return false;
        };
        let Some(child) = self.rows.get(index + 1) else {
            return false;
        };
        let direct_child = match row.kind {
            RowKind::Workspace => {
                child.kind == RowKind::Tab && child.workspace.id == row.workspace.id
            }
            RowKind::Tab => {
                child.kind == RowKind::Session
                    && child.workspace.id == row.workspace.id
                    && child.tab_id == row.tab_id
            }
            RowKind::Session => false,
        };
        if !direct_child {
            return false;
        }
        let key = child.key.clone();
        self.select(&key)
    }

    /// Bind background documents to the sessions captured before their I/O began.
    pub fn set_contents(
        &mut self,
        documents: HashMap<String, ContentDocument>,
        captured_sessions: &[Session],
    ) -> bool {
        let origins: HashMap<&str, &Session> = captured_sessions
            .iter()
            .map(|session| (session.terminal_id.as_str(), session))
            .collect();
        let mut changed = false;
        for (id, document) in documents {
            let Some(current) = self.sessions.get(&id) else {
                continue;
            };
            let Some(origin) = origins.get(id.as_str()) else {
                continue;
            };
            let identity = Identity::from(current);
            if identity != Identity::from(*origin) {
                continue;
            }
            if let Some(previous) = self.contents.get_mut(&id) {
                if previous.revision > origin.revision {
                    continue;
                }
                if previous.document.text == document.text {
                    previous.revision = origin.revision;
                    continue;
                }
            }
            self.contents.insert(
                id,
                CachedContent {
                    identity,
                    revision: origin.revision,
                    document,
                },
            );
            changed = true;
        }
        if changed && !self.terms.is_empty() {
            self.rebuild();
            self.preserve_selection();
        }
        changed
    }

    /// This is only a revision hint. Zero-revision sources additionally need a caller TTL.
    pub fn needs_content(&self, session: &Session) -> bool {
        self.contents
            .get(&session.terminal_id)
            .is_none_or(|content| {
                content.identity != Identity::from(session) || content.revision < session.revision
            })
    }

    pub fn clear_contents(&mut self) {
        if !self.contents.is_empty() {
            self.contents.clear();
            if !self.terms.is_empty() {
                self.rebuild();
                self.preserve_selection();
            }
        }
    }
}
