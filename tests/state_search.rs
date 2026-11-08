use herdr_finder::model::{Session, Snapshot, Tab, Workspace};
use herdr_finder::search::{
    ContentDocument, MAX_CONTENT_BYTES, find_matches, highlight_spans, normalize, query_terms,
};
use herdr_finder::state::{FinderState, GroupLevel, RowKind};
use std::collections::HashMap;

fn session(id: &str, workspace: &str, tab: &str, title: &str) -> Session {
    Session {
        terminal_id: id.into(),
        pane_id: format!("pane-{id}"),
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        title: title.into(),
        kind: "codex".into(),
        display_kind: "Codex".into(),
        name: format!("role-{id}"),
        status: "working".into(),
        cwd: "/synthetic/shared".into(),
        split_index: 1,
        split_count: 1,
        ..Session::default()
    }
}

fn fixture() -> Snapshot {
    let mut a = session("a", "w1", "t1", "Fix payment errors");
    a.tab_label = "Backend".into();
    a.focused = true;
    let mut b = session("b", "w1", "t2", "Review payment requests");
    b.tab_label = "Review".into();
    b.status = "blocked".into();
    let mut c = session("c", "w2", "t3", "Investigate response latency");
    c.tab_label = "Deploy".into();
    c.kind = "claude".into();
    c.display_kind = "Claude".into();
    c.name.clear();
    c.status = "idle".into();
    let mut d = session("d", "w2", "t4", "Tail logs");
    d.tab_label = "Logs".into();
    d.kind = "terminal".into();
    d.display_kind = "Terminal".into();
    d.name.clear();
    d.status = "unknown".into();
    Snapshot {
        workspaces: vec![
            Workspace {
                id: "w1".into(),
                label: "App development".into(),
                focused: true,
            },
            Workspace {
                id: "w2".into(),
                label: "Infrastructure".into(),
                focused: false,
            },
            Workspace {
                id: "w3".into(),
                label: "Empty workspace".into(),
                focused: false,
            },
        ],
        sessions: vec![a, b, c, d],
        tabs: Vec::new(),
    }
}

fn navigation_fixture() -> Snapshot {
    let mut snapshot = fixture();
    snapshot.sessions.insert(
        1,
        session("a2", "w1", "t1", "Another pane in the first tab"),
    );
    snapshot.tabs = [
        ("t1", "w1", "Backend", 1),
        ("t2", "w1", "Review", 2),
        ("t3", "w2", "Deploy", 1),
        ("t4", "w2", "Logs", 2),
    ]
    .into_iter()
    .map(|(id, workspace, label, number)| Tab {
        id: id.into(),
        workspace_id: workspace.into(),
        label: label.into(),
        number,
        ..Tab::default()
    })
    .collect();
    snapshot
}

fn document(id: &str, text: &str) -> HashMap<String, ContentDocument> {
    HashMap::from([(id.into(), ContentDocument::new(text))])
}

fn visible_terminals(state: &FinderState) -> Vec<&str> {
    state
        .rows()
        .iter()
        .filter_map(|row| row.session.as_ref())
        .map(|session| session.terminal_id.as_str())
        .collect()
}

#[test]
fn friendly_badge_names_are_searchable_without_rewriting_native_kinds() {
    let mut snapshot = fixture();
    snapshot.sessions[0].kind = "agy".into();
    snapshot.sessions[0].display_kind = "agy".into();
    snapshot.sessions[1].kind = "omp".into();
    snapshot.sessions[1].display_kind = "omp".into();

    let mut state = FinderState::default();
    state.update(snapshot);
    for (query, terminal, kind) in [("Antigravity", "a", "agy"), ("Oh My Pi", "b", "omp")] {
        state.set_query(query.into());
        assert_eq!(visible_terminals(&state), vec![terminal]);
        assert_eq!(
            state.selected().unwrap().session.as_ref().unwrap().kind,
            kind
        );
    }
}

#[test]
fn unicode_matching_and_original_utf8_byte_ranges() {
    let cases = [
        // Full case folding expands the German sharp s into two ASCII characters.
        ("Stra\u{df}e", "STRASSE", vec![(0, 7)]),
        // NFKC maps fullwidth Latin characters to ASCII without changing the original span.
        ("\u{ff21}\u{ff30}\u{ff29} endpoint", "api", vec![(0, 9)]),
        ("A\u{030a}", "\u{e5}", vec![(0, 3)]),
        // NFKC composes decomposed Hangul while highlights retain its original UTF-8 byte range.
        (
            "\u{1112}\u{1161}\u{11ab}\u{1100}\u{1173}\u{11af}",
            "\u{d55c}\u{ae00}",
            vec![(0, 18)],
        ),
        // Compatibility jamo can compose across their original grapheme boundaries.
        ("\u{3131}\u{314f}", "\u{ac00}", vec![(0, 6)]),
        // One ligature and three ASCII letters normalize to the same search term.
        ("\u{fb03} ffi", "ffi", vec![(0, 3), (4, 7)]),
        ("[bold]Task[/bold]", "task", vec![(6, 10)]),
        // Matching one component of an emoji highlights its complete joined grapheme.
        (
            "\u{1f469}\u{200d}\u{1f4bb}",
            "\u{1f469}",
            vec![(0, "\u{1f469}\u{200d}\u{1f4bb}".len())],
        ),
    ];
    for (literal, query, expected) in cases {
        let spans = highlight_spans(literal, query);
        assert_eq!(spans, expected, "{literal:?} / {query:?}");
        for (start, end) in spans {
            assert!(literal.is_char_boundary(start) && literal.is_char_boundary(end));
            assert!(!literal[start..end].is_empty());
        }
        let document = ContentDocument::new(literal);
        assert_eq!(document.normalized, normalize(&document.text));
        assert!(document.match_query(query, "").is_some());
    }
}

#[test]
fn query_normalization_deduplicates_full_casefold_terms() {
    assert_eq!(
        query_terms("\u{ff21}\u{ff30}\u{ff29} api STRASSE Stra\u{df}e"),
        vec!["api", "strasse"]
    );
    // Uppercase sigma and lowercase final sigma share the same full case fold.
    assert_eq!(normalize("\u{39f}\u{3a3}"), normalize("\u{3bf}\u{3c2}"));
}

#[test]
fn body_and_metadata_terms_match_together_with_latest_excerpt() {
    let body = ContentDocument::new("needle old\ncontext\nneedle new\nfinal context");
    let found = body
        .match_query("workspace needle", "Workspace API")
        .unwrap();
    assert!(found.body_hit);
    assert_eq!(found.excerpt, vec!["context", "needle new"]);
    assert_eq!(found.highlights, vec![vec![], vec![(0, 6)]]);
    assert!(
        body.match_query("workspace absent", "Workspace API")
            .is_none()
    );
}

#[test]
fn metadata_only_and_empty_query_do_not_invent_excerpt() {
    let found = find_matches(
        "api task",
        &ContentDocument::new("API Task"),
        &ContentDocument::new("other"),
    )
    .unwrap();
    assert!(!found.body_hit && found.excerpt.is_empty() && found.highlights.is_empty());
    assert!(
        ContentDocument::new("output")
            .match_query("", "")
            .unwrap()
            .excerpt
            .is_empty()
    );
}

#[test]
fn body_cleanup_and_long_snippets_are_bounded() {
    let text = (0..400)
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n\x1b[31m[bold]hit\x1b[0m\u{202e}";
    let document = ContentDocument::new(text);
    assert_eq!(document.lines.len(), 300);
    assert_eq!(document.lines[0], "101");
    assert_eq!(document.lines.last().unwrap(), "[bold]hit");
    assert!(!document.text.contains('\x1b'));
    // A three-byte scalar makes the byte cap exercise UTF-8 boundary preservation.
    let oversized = ContentDocument::new("\u{d55c}".repeat(MAX_CONTENT_BYTES) + " needle");
    assert!(oversized.text.len() <= MAX_CONTENT_BYTES);
    let found = oversized.match_query("needle", "").unwrap();
    assert!(found.excerpt.last().unwrap().contains("needle"));
    assert!(found.excerpt.iter().all(|line| line.chars().count() <= 242));
    for (line, spans) in found.excerpt.iter().zip(found.highlights) {
        for (start, end) in spans {
            assert!(line.is_char_boundary(start) && line.is_char_boundary(end));
        }
    }
}

#[test]
fn literal_markup_is_not_interpreted_in_snippets() {
    let found = ContentDocument::new("[red]needle[/red]")
        .match_query("needle", "")
        .unwrap();
    assert_eq!(found.excerpt, vec!["[red]needle[/red]"]);
    assert_eq!(found.highlights, vec![vec![(5, 11)]]);
}

#[test]
fn hierarchy_has_all_workspaces_and_stable_native_tabs() {
    let mut snapshot = fixture();
    snapshot.tabs = vec![
        Tab {
            id: "t2".into(),
            workspace_id: "w1".into(),
            label: "Review".into(),
            number: 2,
            ..Tab::default()
        },
        Tab {
            id: "t1".into(),
            workspace_id: "w1".into(),
            number: 1,
            ..Tab::default()
        },
        Tab {
            id: "empty".into(),
            workspace_id: "w1".into(),
            number: 3,
            ..Tab::default()
        },
        Tab {
            id: "t4".into(),
            workspace_id: "w2".into(),
            label: "Logs".into(),
            number: 2,
            ..Tab::default()
        },
        Tab {
            id: "t3".into(),
            workspace_id: "w2".into(),
            label: "Deploy".into(),
            number: 1,
            ..Tab::default()
        },
    ];
    let mut state = FinderState::default();
    state.update(snapshot);
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        vec![
            "w:w1", "t:t2", "s:b", "t:t1", "s:a", "t:empty", "w:w2", "t:t4", "s:d", "t:t3", "s:c",
            "w:w3"
        ]
    );
    assert_eq!(state.rows()[3].tab_label, "Tab 1");
    assert_eq!(state.rows()[5].tab_label, "Tab 3");
    assert!(state.rows()[6].separator_before);
    assert_eq!(state.selected_key(), Some("s:a"));
}

#[test]
fn workspace_and_tab_queries_include_children_without_cwd_grouping() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.set_query("infrastructure response CLAUDE".into());
    assert_eq!(visible_terminals(&state), vec!["c"]);
    state.set_query("App development".into());
    assert_eq!(visible_terminals(&state), vec!["a", "b"]);
    state.set_query("/synthetic/shared".into());
    assert_eq!(visible_terminals(&state).len(), 4);
    assert_eq!(
        state
            .rows()
            .iter()
            .filter(|row| row.kind == RowKind::Workspace)
            .count(),
        2
    );
}

#[test]
fn folding_levels_restore_after_search_and_search_selects_terminal() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.toggle_group(None);
    assert!(state.collapsed_tabs.contains("t1"));
    assert_eq!(state.selected().unwrap().kind, RowKind::Tab);
    assert_eq!(visible_terminals(&state), vec!["b", "c", "d"]);
    state.toggle_group(Some(GroupLevel::Workspace));
    assert!(state.collapsed.contains("w1"));
    state.set_query("payment".into());
    assert_eq!(visible_terminals(&state), vec!["a", "b"]);
    assert_eq!(state.selected().unwrap().kind, RowKind::Session);
    state.set_query(String::new());
    assert!(state.collapsed.contains("w1") && state.collapsed_tabs.contains("t1"));
    state.select("w:w1");
    state.toggle_group(None);
    assert!(!visible_terminals(&state).contains(&"a"));
    assert!(visible_terminals(&state).contains(&"b"));
}

#[test]
fn hidden_metadata_titles_remain_searchable() {
    let mut snapshot = fixture();
    snapshot.sessions[0].title = "Manual label".into();
    snapshot.sessions[0].search_titles = vec!["Investigate retries".into()];
    let mut state = FinderState::default();
    state.update(snapshot);
    state.set_query("investigate RETRIES".into());
    assert_eq!(visible_terminals(&state), vec!["a"]);
}

#[test]
fn next_tab_lands_on_the_pane_ready_to_open_and_crosses_workspaces() {
    let mut state = FinderState::default();
    state.update(navigation_fixture());
    assert!(state.select("s:a2"));
    let storage = state.rows().as_ptr();
    assert!(state.move_target(1, GroupLevel::Tab));
    let ready_for_enter = state.selected().unwrap();
    assert_eq!(ready_for_enter.kind, RowKind::Session);
    assert_eq!(ready_for_enter.session.as_ref().unwrap().terminal_id, "b");
    assert_eq!(ready_for_enter.session.as_ref().unwrap().pane_id, "pane-b");
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:c"));
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:a"));
    assert_eq!(state.rows().as_ptr(), storage);
    assert!(state.collapsed.is_empty() && state.collapsed_tabs.is_empty());
}

#[test]
fn workspace_and_compatibility_jumps_use_the_same_landing_policy() {
    let mut state = FinderState::default();
    state.update(navigation_fixture());
    assert!(state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:c"));
    state.move_group(-1, GroupLevel::Workspace);
    assert_eq!(state.selected_key(), Some("s:a"));
    state.move_group(1, GroupLevel::Tab);
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(state.select("w:w2"));
    assert!(state.toggle_group(None));
    assert!(state.select("s:b"));
    assert!(state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("w:w2"));
    assert!(state.collapsed.contains("w2"));
}

#[test]
fn filtered_navigation_only_selects_panes_and_preserves_folds() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.toggle_group(None);
    state.toggle_group(Some(GroupLevel::Workspace));
    state.set_query("payment".into());
    assert!(state.is_filtering());
    assert!(!state.select("w:w1"));
    assert!(!state.select("t:t1"));
    assert!(!state.select_parent());
    assert_eq!(state.selected_key(), Some("s:a"));
    state.move_selection(1);
    assert_eq!(state.selected_key(), Some("s:b"));
    state.move_selection(1);
    assert_eq!(state.selected_key(), Some("s:b"));
    state.move_selection(-1);
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(!state.move_target(1, GroupLevel::Workspace));
    assert!(!state.toggle_group(None));
    assert!(state.collapsed.contains("w1") && state.collapsed_tabs.contains("t1"));
    state.set_query(String::new());
    assert!(state.collapsed.contains("w1") && state.collapsed_tabs.contains("t1"));
    assert_eq!(state.selected_key(), Some("w:w1"));
    assert!(!visible_terminals(&state).contains(&"a"));
    assert!(!visible_terminals(&state).contains(&"b"));
}

#[test]
fn folded_and_empty_groups_are_stops_without_automatic_expansion() {
    let mut snapshot = navigation_fixture();
    snapshot.tabs.insert(
        2,
        Tab {
            id: "empty-tab".into(),
            workspace_id: "w1".into(),
            label: "Empty tab".into(),
            number: 3,
            ..Tab::default()
        },
    );
    let mut state = FinderState::default();
    state.update(snapshot);
    state.select("t:t2");
    state.toggle_group(None);
    state.select("w:w2");
    state.toggle_group(None);
    state.select("s:a");
    let storage = state.rows().as_ptr();
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("t:t2"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("t:empty-tab"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("w:w2"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("w:w3"));
    assert!(!state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("w:w3"));
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("w:w2"));
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("t:empty-tab"));
    assert!(state.collapsed.contains("w2") && state.collapsed_tabs.contains("t2"));
    assert_eq!(state.rows().as_ptr(), storage);
}

#[test]
fn target_edges_keep_the_exact_pane_and_never_wrap_or_move_backwards() {
    let mut snapshot = navigation_fixture();
    snapshot.workspaces.pop();
    let mut state = FinderState::default();
    state.update(snapshot);
    state.select("s:a2");
    assert!(!state.move_target(-1, GroupLevel::Tab));
    assert!(!state.move_target(isize::MIN, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:a2"));
    state.select("s:d");
    assert!(!state.move_target(1, GroupLevel::Tab));
    assert!(!state.move_target(isize::MAX, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:d"));
    assert!(state.move_target(isize::MIN, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(state.move_target(isize::MAX, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:d"));
    assert!(!state.move_target(0, GroupLevel::Tab));
}

#[test]
fn expanded_workspace_headers_enter_tabs_without_changing_folds() {
    let mut state = FinderState::default();
    state.update(navigation_fixture());
    state.select("w:w2");
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:c"));
    state.select("w:w2");
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(state.collapsed.is_empty() && state.collapsed_tabs.is_empty());
}

#[test]
fn target_order_follows_native_tabs_and_uses_first_status_visible_pane() {
    let mut snapshot = navigation_fixture();
    snapshot.tabs.swap(0, 1);
    snapshot.sessions[0].status = "blocked".into();
    snapshot.sessions[2].status = "working".into();
    let mut state = FinderState::default();
    state.update(snapshot);
    state.set_status(Some("working".into()));
    state.select("s:b");
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:a2"));
    assert!(!state.move_target(1, GroupLevel::Tab));
}

#[test]
fn filtering_with_headers_but_no_panes_keeps_selection_empty_until_a_body_hit() {
    let snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    state.set_query("empty workspace".into());
    assert!(!state.rows().is_empty());
    assert!(state.selected().is_none() && !state.selection_lost);
    assert!(!state.select("w:w3"));
    state.move_selection(1);
    assert!(!state.move_target(1, GroupLevel::Workspace));
    assert!(state.selected().is_none());
    assert!(state.set_contents(document("b", "empty workspace"), &snapshot.sessions));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(!state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:b"));
}

#[test]
fn filtered_tab_and_workspace_jumps_land_on_matching_panes_only() {
    let mut state = FinderState::default();
    state.update(navigation_fixture());
    state.set_query("shared".into());
    assert!(state.select("s:a2"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:c"));
    assert!(state.move_target(-1, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:a"));
    let storage = state.rows().as_ptr();
    for _ in 0..100 {
        state.move_selection(1);
        assert_eq!(state.selected().unwrap().kind, RowKind::Session);
        state.move_target(1, GroupLevel::Tab);
        assert_eq!(state.selected().unwrap().kind, RowKind::Session);
        state.move_target(-1, GroupLevel::Workspace);
        assert_eq!(state.selected().unwrap().kind, RowKind::Session);
        assert_eq!(state.rows().as_ptr(), storage);
    }
    state.move_selection(isize::MIN);
    assert_eq!(state.selected_key(), Some("s:a"));
    state.move_selection(isize::MAX);
    assert_eq!(state.selected_key(), Some("s:d"));
    assert_eq!(state.query, "shared");
}

#[test]
fn clearing_query_preserves_visible_identity_or_selects_its_folded_parent() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.toggle_group(None);
    state.set_query("payment".into());
    state.set_query(String::new());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(state.collapsed_tabs.contains("t1"));
    state.set_query("shared".into());
    state.select("s:c");
    state.set_query(String::new());
    assert_eq!(state.selected_key(), Some("s:c"));
    assert!(state.collapsed_tabs.contains("t1"));
}

#[test]
fn tree_right_enters_each_direct_level_and_leaf_is_a_noop() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.select("w:w1");
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(!state.tree_right());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(state.tree_left());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(state.tree_left());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(state.collapsed_tabs.contains("t1"));
    assert!(state.tree_left());
    assert_eq!(state.selected_key(), Some("w:w1"));
    assert!(state.tree_left());
    assert!(state.collapsed.contains("w1"));
    assert!(!state.tree_left());
    assert_eq!(state.selected_key(), Some("w:w1"));
}

#[test]
fn tree_right_expands_before_entering_each_collapsed_group() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.select("t:t1");
    state.toggle_group(None);
    state.select("w:w1");
    state.toggle_group(None);
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("w:w1"));
    assert!(!state.collapsed.contains("w1"));
    assert!(state.collapsed_tabs.contains("t1"));
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("t:t1"));
    assert!(!state.collapsed_tabs.contains("t1"));
    assert!(state.tree_right());
    assert_eq!(state.selected_key(), Some("s:a"));
}

#[test]
fn unused_tree_navigation_cannot_select_search_headers_or_change_folds() {
    let mut state = FinderState::default();
    state.update(fixture());
    state.toggle_group(None);
    state.toggle_group(Some(GroupLevel::Workspace));
    state.set_query("payment".into());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(!state.tree_left());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(!state.tree_left());
    assert!(!state.tree_right());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(state.collapsed.contains("w1") && state.collapsed_tabs.contains("t1"));
    state.set_query(String::new());
    assert!(state.collapsed.contains("w1") && state.collapsed_tabs.contains("t1"));
    assert!(!visible_terminals(&state).contains(&"a"));
    assert!(!visible_terminals(&state).contains(&"b"));
}

#[test]
fn tree_right_does_not_enter_another_workspace_when_group_is_empty() {
    let mut snapshot = fixture();
    snapshot.workspaces.swap(0, 2);
    let mut state = FinderState::default();
    state.update(snapshot);
    state.select("w:w3");
    assert!(!state.tree_right());
    assert_eq!(state.selected_key(), Some("w:w3"));
}

#[test]
fn tree_arrows_do_not_reselect_after_terminal_disappears() {
    let mut snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    snapshot.sessions.remove(0);
    state.update(snapshot);
    assert!(!state.tree_left());
    assert!(!state.tree_right());
    assert!(state.selected().is_none() && state.selection_lost);
}

#[test]
fn vanished_selection_cannot_open_its_neighbor_after_refresh() {
    let mut snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    snapshot.sessions.remove(0);
    state.update(snapshot.clone());
    assert!(state.selected().is_none() && state.selection_lost);
    state.update(snapshot);
    assert!(state.selected().is_none());
    state.move_selection(1);
    assert!(state.selected().is_some() && !state.selection_lost);
}

#[test]
fn selected_terminal_survives_native_address_and_workspace_moves() {
    let mut snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    snapshot.sessions[0].workspace_id = "w2".into();
    snapshot.sessions[0].tab_id = "t4".into();
    snapshot.sessions[0].pane_id = "moved-pane".into();
    state.update(snapshot);
    assert_eq!(state.selected_key(), Some("s:a"));
    assert_eq!(
        state.selected().unwrap().session.as_ref().unwrap().pane_id,
        "moved-pane"
    );
}

#[test]
fn query_before_initial_snapshot_selects_visible_match() {
    let mut state = FinderState::default();
    state.set_query("response".into());
    state.update(fixture());
    assert_eq!(state.selected_key(), Some("s:c"));
}

#[test]
fn late_body_first_match_is_selected_without_replacing_good_selection() {
    let snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    state.set_query("codex latency".into());
    assert!(state.rows().is_empty());
    state.set_contents(
        document("a", "Current latency investigation"),
        &snapshot.sessions,
    );
    assert_eq!(state.selected_key(), Some("s:a"));
    state.set_contents(document("b", "More latency evidence"), &snapshot.sessions);
    assert_eq!(state.selected_key(), Some("s:a"));
    let selected = state.selected().unwrap();
    assert!(
        selected.body_hit && !selected.snippets.is_empty() && !selected.snippet_spans.is_empty()
    );
}

#[test]
fn late_body_results_reject_disappearance_and_replaced_identity() {
    let snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    state.set_query("body-hit".into());
    let mut changed = snapshot.clone();
    changed.sessions[0].native_reference =
        Some(("codex".into(), "id".into(), "replacement".into()));
    changed.sessions.remove(1);
    state.update(changed);
    let mut contents = document("a", "body-hit");
    contents.extend(document("b", "body-hit"));
    assert!(!state.set_contents(contents, &snapshot.sessions));
    assert!(state.rows().is_empty());
}

#[test]
fn revision_is_only_freshness_and_old_results_do_not_overwrite_newer_docs() {
    let mut snapshot = fixture();
    snapshot.sessions.truncate(1);
    snapshot.sessions[0].revision = 1;
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    state.set_contents(document("a", "Old log"), &snapshot.sessions);
    let first = snapshot.sessions.clone();
    snapshot.sessions[0].revision = 2;
    snapshot.sessions[0].pane_id = "new-address".into();
    state.update(snapshot.clone());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(state.needs_content(&snapshot.sessions[0]));
    assert!(state.set_contents(document("a", "Delayed first revision"), &first));
    assert!(state.needs_content(&snapshot.sessions[0]));
    state.set_contents(document("a", "New log"), &snapshot.sessions);
    assert!(!state.set_contents(document("a", "Stale log"), &first));
    state.set_query("new log".into());
    assert_eq!(state.selected_key(), Some("s:a"));
    assert!(!state.needs_content(&snapshot.sessions[0]));
}

#[test]
fn late_body_does_not_reselect_neighbor_after_selection_was_lost() {
    let mut snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    let mut contents = document("a", "needle");
    contents.extend(document("b", "needle"));
    state.set_contents(contents, &snapshot.sessions);
    state.set_query("needle".into());
    snapshot.sessions.remove(0);
    state.update(snapshot.clone());
    state.set_contents(document("b", "needle updated"), &snapshot.sessions);
    assert!(state.selection_lost && state.selected().is_none());
    state.set_query(String::new());
    assert!(state.selection_lost && state.selected().is_none());
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:b"));
    assert!(!state.selection_lost);
}

#[test]
fn repeated_selection_and_equal_snapshot_refresh_reuse_row_storage() {
    let snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    let original = state.rows().as_ptr();
    for _ in 0..100 {
        state.move_selection(1);
        state.select_parent();
        assert_eq!(state.rows().as_ptr(), original);
        assert!(state.row_index(state.selected_key().unwrap()).is_some());
    }
    state.update(snapshot);
    assert_eq!(state.rows().as_ptr(), original);
}

#[test]
fn empty_native_tabs_have_readable_headers_and_stay_searchable() {
    let snapshot = Snapshot::from_native(&serde_json::json!({
        "workspaces": [{
            "workspace_id": "opaque-w",
            "label": "Empty project",
        }],
        "tabs": [
            {
                "tab_id": "opaque-a",
                "workspace_id": "opaque-w",
                "label": "1",
                "number": 1,
            },
            {
                "tab_id": "opaque-b",
                "workspace_id": "opaque-w",
                "label": "",
                "number": 2,
            },
        ],
        "panes": [],
        "agents": [],
    }))
    .unwrap();
    let mut state = FinderState::default();
    state.update(snapshot);
    assert_eq!(
        state
            .rows()
            .iter()
            .filter(|row| row.kind == RowKind::Tab)
            .map(|row| row.tab_label.as_str())
            .collect::<Vec<_>>(),
        vec!["Tab 1", "Tab 2"]
    );
    state.set_query("tab 2".into());
    assert_eq!(
        state.rows().iter().map(|row| row.kind).collect::<Vec<_>>(),
        vec![RowKind::Workspace, RowKind::Tab]
    );
    assert!(state.selected().is_none());
    assert!(!state.select("t:opaque-b"));
    state.move_selection(-1);
    assert!(!state.move_target(-1, GroupLevel::Tab));
    assert!(state.selected().is_none());
}

#[test]
fn status_filter_and_known_activity_are_not_inferred() {
    let mut snapshot = fixture();
    let known = "2026-11-08T03:05:00Z".parse().unwrap();
    snapshot.sessions[1].activity_at = Some(known);
    let mut state = FinderState::default();
    state.update(snapshot);
    assert!(state.selected().unwrap().activity_at.is_none());
    state.set_status(Some("blocked".into()));
    assert_eq!(visible_terminals(&state), vec!["b"]);
    assert_eq!(state.selected().unwrap().activity_at, Some(known));
}

#[test]
fn clearing_content_preserves_vanished_selection_safety() {
    let snapshot = fixture();
    let mut state = FinderState::default();
    state.update(snapshot.clone());
    state.set_contents(document("a", "only-body-match"), &snapshot.sessions);
    state.set_query("only-body-match".into());
    assert_eq!(state.selected_key(), Some("s:a"));
    state.clear_contents();
    assert!(state.selected().is_none() && state.selection_lost);
}

#[test]
#[ignore = "manual pure-state performance measurement"]
fn state_domain_benchmark() {
    use std::time::{Duration, Instant};
    fn median(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }
    for count in [100, 500] {
        let snapshot = Snapshot {
            workspaces: (0..10)
                .map(|w| Workspace {
                    id: format!("w{w}"),
                    label: format!("Project {w}"),
                    focused: w == 0,
                })
                .collect(),
            tabs: (0..10)
                .flat_map(|w| {
                    (0..5).map(move |t| Tab {
                        id: format!("t{w}-{t}"),
                        workspace_id: format!("w{w}"),
                        label: format!("Tab {t}"),
                        number: t + 1,
                        ..Tab::default()
                    })
                })
                .collect(),
            sessions: (0..count)
                .map(|i| {
                    session(
                        &format!("s{i}"),
                        &format!("w{}", i % 10),
                        &format!("t{}-{}", i % 10, i / 10 % 5),
                        &format!("Task {i} request review"),
                    )
                })
                .collect(),
        };
        let mut startups = Vec::new();
        let mut cold_queries = Vec::new();
        for _ in 0..20 {
            let input = snapshot.clone();
            let started = Instant::now();
            let mut state = FinderState::default();
            state.update(input);
            startups.push(started.elapsed());
            let started = Instant::now();
            state.set_query("Task 12".into());
            cold_queries.push(started.elapsed());
        }
        let mut state = FinderState::default();
        state.update(snapshot);
        state.set_query("Task 12".into());
        let mut warm_queries = Vec::new();
        for i in 0..200 {
            let started = Instant::now();
            state.set_query(format!("Task {}", i % count));
            warm_queries.push(started.elapsed());
        }
        state.set_query(String::new());
        let started = Instant::now();
        for _ in 0..100_000 {
            state.move_selection(1);
            std::hint::black_box(state.selected());
        }
        let movement = started.elapsed() / 100_000;
        let started = Instant::now();
        for i in 0..100_000 {
            state.move_target(if i % 2 == 0 { 1 } else { -1 }, GroupLevel::Tab);
            std::hint::black_box(state.selected());
        }
        let target_movement = started.elapsed() / 100_000;
        eprintln!(
            "{count} panes: startup_median={:?}, first_query_median={:?}, warm_query_median={:?}, move_mean={movement:?}, target_mean={target_movement:?}",
            median(&mut startups),
            median(&mut cold_queries),
            median(&mut warm_queries)
        );
    }
}
