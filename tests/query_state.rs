use herdr_finder::{
    model::{Session, Snapshot, Tab, Workspace},
    query::Query,
    search::ContentDocument,
    state::{FinderState, GroupLevel, Row, RowKind},
};
use std::collections::HashMap;

#[test]
fn metadata_highlights_follow_fuzzy_matches_without_fuzzing_screen_evidence() {
    use herdr_finder::search::{highlight_spans_terms, metadata_highlight_spans_terms};
    let terms = vec!["pymnt".to_owned()];
    assert_eq!(
        metadata_highlight_spans_terms("Payment", &terms, &terms),
        [(0, 1), (2, 4), (5, 7)]
    );
    assert!(highlight_spans_terms("Payment", &terms).is_empty());
    let miss = vec!["pyzzz".to_owned()];
    assert!(metadata_highlight_spans_terms("Payment", &miss, &miss).is_empty());
}

#[test]
fn fuzzy_highlights_preserve_original_unicode_boundaries() {
    use herdr_finder::search::metadata_highlight_spans_terms;
    for (text, term, expected) in [
        ("Straße", "sts", vec![(0, 2), (4, 6)]),
        ("ｐａｙｍｅｎｔ", "pymnt", vec![(0, 3), (6, 12), (15, 21)]),
        ("한글 검색", "한검색", vec![(0, 3), (7, 13)]),
    ] {
        let terms = vec![term.to_owned()];
        let ranges = metadata_highlight_spans_terms(text, &terms, &terms);
        assert_eq!(ranges, expected, "{text}");
        for (start, end) in ranges {
            assert!(text.is_char_boundary(start) && text.is_char_boundary(end));
        }
    }
}

#[test]
fn quoted_excluded_and_scoped_terms_are_not_fuzzy_highlights() {
    let query =
        Query::parse("pymnt \"plain phrase\" !excluded agent:claude title:retry screen:error")
            .unwrap();
    assert_eq!(query.fuzzy_terms(), ["pymnt"]);
}

fn session(id: &str, title: &str, status: &str) -> Session {
    Session {
        terminal_id: id.into(),
        pane_id: format!("pane-{id}"),
        workspace_id: "workspace".into(),
        tab_id: "tab".into(),
        title: title.into(),
        status: status.into(),
        kind: "codex".into(),
        display_kind: "Codex".into(),
        cwd: "/projects/service".into(),
        revision: 1,
        ..Session::default()
    }
}

fn snapshot(sessions: Vec<Session>) -> Snapshot {
    Snapshot {
        workspaces: vec![Workspace {
            id: "workspace".into(),
            label: "Commerce".into(),
            ..Workspace::default()
        }],
        tabs: vec![Tab {
            id: "tab".into(),
            workspace_id: "workspace".into(),
            label: "Backend".into(),
            number: 1,
            ..Tab::default()
        }],
        sessions,
    }
}

fn state(snapshot: Snapshot) -> FinderState {
    let mut state = FinderState::default();
    state.update(snapshot);
    state
}

fn ids(state: &FinderState) -> Vec<&str> {
    state
        .rows()
        .iter()
        .filter_map(|row| {
            row.session
                .as_ref()
                .map(|session| session.terminal_id.as_str())
        })
        .collect()
}

fn session_rows(state: &FinderState) -> Vec<&Row> {
    state
        .rows()
        .iter()
        .filter(|row| row.session.is_some())
        .collect()
}

fn set_screen(state: &mut FinderState, origin: &[Session], id: &str, text: &str) {
    state.set_contents(
        HashMap::from([(id.into(), ContentDocument::new(text))]),
        origin,
    );
}

fn grouped_snapshot() -> Snapshot {
    let workspaces = [("first", "First"), ("second", "Second"), ("third", "Third")]
        .into_iter()
        .map(|(id, label)| Workspace {
            id: id.into(),
            label: label.into(),
            ..Workspace::default()
        })
        .collect();
    let tabs = [
        ("first-a", "first", "Alpha", 1),
        ("first-b", "first", "Beta", 2),
        ("second-a", "second", "Gamma", 1),
        ("second-b", "second", "Delta", 2),
        ("third-a", "third", "Epsilon", 1),
        ("empty", "third", "Empty tab", 2),
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
    let sessions = [
        ("fuzzy", "first", "first-a", "p-a-y-m-e-n-t"),
        ("prefix", "first", "first-a", "Payment review"),
        ("literal", "first", "first-b", "Repair payment"),
        ("second-prefix", "second", "second-a", "Payment review"),
        ("exact", "second", "second-b", "Payment"),
        ("third-prefix", "third", "third-a", "Payment review"),
    ]
    .into_iter()
    .map(|(id, workspace, tab, title)| Session {
        workspace_id: workspace.into(),
        tab_id: tab.into(),
        ..session(id, title, "working")
    })
    .collect();
    Snapshot {
        workspaces,
        tabs,
        sessions,
    }
}

#[test]
fn search_orders_descendants_and_ancestors_by_best_score_with_native_ties() {
    let mut state = state(grouped_snapshot());
    state.set_query("payment".into());
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        [
            "w:second",
            "t:second-b",
            "s:exact",
            "t:second-a",
            "s:second-prefix",
            "w:first",
            "t:first-a",
            "s:prefix",
            "s:fuzzy",
            "t:first-b",
            "s:literal",
            "w:third",
            "t:third-a",
            "s:third-prefix",
        ]
    );
    assert_eq!(state.selected_key(), Some("s:exact"));
    assert_eq!(state.rows()[0].score, state.rows()[2].score);
    assert_eq!(state.rows()[5].score, state.rows()[7].score);
    assert!(state.move_target(1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:second-prefix"));
    assert!(state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected_key(), Some("s:prefix"));
}

#[test]
fn clearing_ranked_search_restores_native_browse_order_and_empty_groups() {
    let snapshot = grouped_snapshot();
    let expected = [
        "w:first",
        "t:first-a",
        "s:fuzzy",
        "s:prefix",
        "t:first-b",
        "s:literal",
        "w:second",
        "t:second-a",
        "s:second-prefix",
        "t:second-b",
        "s:exact",
        "w:third",
        "t:third-a",
        "s:third-prefix",
        "t:empty",
    ];
    let mut state = state(snapshot);
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    state.set_query("payment".into());
    state.set_query(String::new());
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        expected
    );
    state.select("t:empty");
    assert!(state.move_target(-1, GroupLevel::Tab));
    assert_eq!(state.selected_key(), Some("s:third-prefix"));
}

#[test]
fn tied_descendants_keep_native_order_and_empty_groups_do_not_outrank_sessions() {
    let mut input = grouped_snapshot();
    input.sessions.insert(
        2,
        Session {
            terminal_id: "prefix-tie".into(),
            pane_id: "pane-prefix-tie".into(),
            ..input.sessions[1].clone()
        },
    );
    input.tabs[5].label = "Payment".into();
    let mut state = state(input);
    state.set_query("payment".into());
    assert_eq!(
        ids(&state),
        [
            "exact",
            "second-prefix",
            "prefix",
            "prefix-tie",
            "fuzzy",
            "literal",
            "third-prefix"
        ]
    );
    assert_eq!(state.selected_key(), Some("s:exact"));
    let empty = &state.rows()[state.row_index("t:empty").unwrap()];
    assert_eq!(empty.score, 0);
    assert_eq!(empty.count, 0);
    assert!(!state.select("t:empty"));
}

#[test]
fn parsed_fields_and_status_filter_keep_the_native_hierarchy() {
    let mut input = grouped_snapshot();
    input.sessions[1].status = "blocked".into();
    input.sessions[3].status = "blocked".into();
    let mut state = state(input);
    state.set_status(Some("blocked".into()));
    assert!(!state.needs_contents());
    state.set_query("ws:second status:blocked title:payment".into());
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.key.as_str())
            .collect::<Vec<_>>(),
        ["w:second", "t:second-a", "s:second-prefix"]
    );
    assert!(!state.needs_contents());
    state.set_query("status:working".into());
    assert!(state.rows().is_empty());
    state.set_status(None);
    assert_eq!(ids(&state), ["fuzzy", "literal", "exact", "third-prefix"]);
    assert!(
        state
            .rows()
            .iter()
            .any(|row| row.kind == RowKind::Workspace)
    );
    assert!(state.rows().iter().any(|row| row.kind == RowKind::Tab));
}

#[test]
fn pure_negative_screen_query_requires_evidence_and_keeps_group_navigation() {
    let input = snapshot(vec![
        session("one", "Review", "working"),
        session("two", "Build", "working"),
    ]);
    let mut state = state(input.clone());
    state.set_query("!screen:needle".into());
    assert!(state.is_filtering() && state.needs_contents());
    assert!(state.rows().is_empty());
    set_screen(&mut state, &input.sessions, "one", "Clean screen");
    set_screen(&mut state, &input.sessions, "two", "needle");
    assert_eq!(ids(&state), ["one"]);
    assert_eq!(
        state.rows().iter().map(|row| row.kind).collect::<Vec<_>>(),
        [RowKind::Workspace, RowKind::Tab, RowKind::Session,]
    );
    assert!(state.selected().unwrap().query_terms.is_empty());
    assert!(!state.select("t:tab"));
    assert!(!state.toggle_group(None));
    assert!(!state.move_target(1, GroupLevel::Tab));
}

#[test]
fn screen_scanning_is_required_only_for_valid_bare_or_screen_terms() {
    let mut state = FinderState::default();
    for (query, expected) in [
        ("needle", true),
        ("!needle", true),
        ("\"agent:codex\"", true),
        ("screen:needle", true),
        ("!screen:needle", true),
        ("https://example.test", true),
        ("title:needle screen:failure", true),
        (
            "agent:codex status:working ws:commerce path:/projects title:review",
            false,
        ),
        ("!agent:claude !path:/private !status:unknown", false),
        ("screen:", false),
        ("screen:\"unclosed", false),
        ("", false),
        ("   ", false),
    ] {
        state.set_query(query.into());
        assert_eq!(state.needs_contents(), expected, "{query}");
        if let Ok(query) = Query::parse(query) {
            assert_eq!(query.needs_screens(), expected);
        }
    }
}

#[test]
fn ranked_results_prefer_metadata_exact_prefix_literal_then_screen_then_fuzzy() {
    let snapshot = snapshot(vec![
        session("screen", "Connection probe", "working"),
        session("fuzzy", "p-a-y-m-e-n-t", "working"),
        session("literal", "Repair payment flow", "working"),
        session("prefix", "Payment retry policy", "working"),
        session("exact", "Payment", "working"),
    ]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "screen",
        "Payment request failed",
    );
    state.set_query("payment".into());
    assert_eq!(
        ids(&state),
        ["exact", "prefix", "literal", "screen", "fuzzy"]
    );
    assert_eq!(state.selected_key(), Some("s:exact"));
    assert_eq!(
        state
            .rows()
            .iter()
            .map(|row| row.kind)
            .take(2)
            .collect::<Vec<_>>(),
        [RowKind::Workspace, RowKind::Tab]
    );
    assert_eq!(session_rows(&state)[4].match_reason, "Title (fuzzy)");
    assert_eq!(session_rows(&state)[3].match_reason, "Screen (literal)");
    assert!(session_rows(&state)[3].body_hit);
    assert!(
        session_rows(&state)
            .windows(2)
            .all(|rows| rows[0].score > rows[1].score)
    );
}

#[test]
fn shared_paths_do_not_create_fuzzy_noise_or_hide_literal_screen_evidence() {
    let mut sessions = vec![
        session("noise-one", "Inspect cache", "working"),
        session("noise-two", "Inspect database", "working"),
        session("evidence", "Inspect connection", "working"),
        session("title", "Error review", "working"),
    ];
    for session in &mut sessions {
        session.cwd = "/Users/me/projects/herdr-finder".into();
    }
    let snapshot = snapshot(sessions);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "evidence",
        "database error timeout",
    );
    set_screen(
        &mut state,
        &snapshot.sessions,
        "title",
        "error investigation started",
    );
    state.set_query("error".into());
    assert_eq!(ids(&state), ["title", "evidence"]);
    assert_eq!(
        session_rows(&state)[0].match_reason,
        "Title (prefix), Screen (literal)"
    );
    assert_eq!(session_rows(&state)[1].match_reason, "Screen (literal)");
    assert_eq!(session_rows(&state)[1].score, 800);
    for row in &session_rows(&state)[..2] {
        assert!(row.body_hit);
        assert!(row.snippets.iter().any(|line| line.contains("error")));
        assert!(row.snippet_spans.iter().flatten().next().is_some());
    }
}

#[test]
fn paths_agents_and_statuses_require_literal_matches_even_for_bare_tokens() {
    let mut input = session("one", "Inspect cache", "working");
    input.cwd = "/Users/me/projects/herdr-finder".into();
    let snapshot = snapshot(vec![input]);
    let mut state = state(snapshot.clone());
    for query in ["error", "cdx", "wrk", "path:err", "agent:cdx", "status:wrk"] {
        state.set_query(query.into());
        assert!(state.rows().is_empty(), "Unexpected match for {query}");
    }
    for query in [
        "herdr",
        "codex",
        "working",
        "path:herdr",
        "agent:codex",
        "status:work",
    ] {
        state.set_query(query.into());
        assert_eq!(ids(&state), ["one"], "Missing literal match for {query}");
    }
    set_screen(
        &mut state,
        &snapshot.sessions,
        "one",
        "database error timeout",
    );
    state.set_query("error".into());
    assert_eq!(ids(&state), ["one"]);
    assert_eq!(session_rows(&state)[0].match_reason, "Screen (literal)");
}

#[test]
fn titles_names_tabs_and_workspaces_still_support_bare_fuzzy_tokens() {
    let mut input = session("one", "Payment retry", "working");
    input.name = "cache-warmup".into();
    let mut state = state(snapshot(vec![input]));
    for (query, reason) in [
        ("pymnt", "Title (fuzzy)"),
        ("cwrm", "Name (fuzzy)"),
        ("bknd", "Tab (fuzzy)"),
        ("cmmrc", "Workspace (fuzzy)"),
    ] {
        state.set_query(query.into());
        assert_eq!(ids(&state), ["one"]);
        assert_eq!(session_rows(&state)[0].match_reason, reason);
    }
}

#[test]
fn late_stronger_evidence_reranks_automatic_selection_but_never_manual_selection() {
    let mut sessions = vec![
        session("noise", "Inspect cache", "working"),
        session("second", "Inspect database", "working"),
        session("evidence", "Inspect connection", "working"),
    ];
    for session in &mut sessions {
        session.name = "e-r-r-o-r".into();
    }
    let mut snapshot = snapshot(sessions);
    let mut state = state(snapshot.clone());
    state.set_query("error".into());
    assert_eq!(state.selected_key(), Some("s:noise"));
    set_screen(
        &mut state,
        &snapshot.sessions,
        "evidence",
        "database error timeout",
    );
    assert_eq!(state.selected_key(), Some("s:evidence"));
    snapshot.sessions[0].title = "Error investigation".into();
    state.update(snapshot.clone());
    assert_eq!(state.selected_key(), Some("s:noise"));
    assert!(state.select("s:evidence"));
    snapshot.sessions[1].title = "Error".into();
    state.update(snapshot.clone());
    assert_eq!(ids(&state)[0], "second");
    assert_eq!(state.selected_key(), Some("s:evidence"));
    set_screen(&mut state, &snapshot.sessions, "second", "error report");
    assert_eq!(state.selected_key(), Some("s:evidence"));
    state.set_query("Error".into());
    assert_eq!(state.selected_key(), Some("s:second"));
}

#[test]
fn explicitly_selecting_the_current_row_freezes_it_against_late_ranking_changes() {
    let mut sessions = vec![
        session("noise", "Inspect cache", "working"),
        session("evidence", "Inspect connection", "working"),
    ];
    for session in &mut sessions {
        session.name = "e-r-r-o-r".into();
    }
    let snapshot = snapshot(sessions);
    let mut state = state(snapshot.clone());
    state.set_query("error".into());
    assert!(state.select("s:noise"));
    set_screen(&mut state, &snapshot.sessions, "evidence", "error report");
    assert_eq!(ids(&state)[0], "evidence");
    assert_eq!(state.selected_key(), Some("s:noise"));
}

#[test]
fn explicit_queries_choose_top_while_background_updates_keep_manual_selection() {
    let mut auth = session("auth", "Repair authentication", "working");
    auth.focused = true;
    let mut snapshot = snapshot(vec![auth, session("payment", "Payment retry", "working")]);
    let mut state = state(snapshot.clone());
    assert_eq!(state.selected_key(), Some("s:auth"));
    set_screen(
        &mut state,
        &snapshot.sessions,
        "auth",
        "Depends on payment service",
    );
    state.set_query("payment".into());
    assert_eq!(ids(&state), ["payment", "auth"]);
    assert_eq!(state.selected_key(), Some("s:payment"));
    state.select("s:auth");
    set_screen(
        &mut state,
        &snapshot.sessions,
        "auth",
        "Depends on payment service, updated",
    );
    assert_eq!(state.selected_key(), Some("s:auth"));
    snapshot.sessions[0].title = "Payment".into();
    state.update(snapshot);
    assert_eq!(state.selected_key(), Some("s:auth"));
    state.select("s:payment");
    state.set_query("Payment".into());
    assert_eq!(state.selected_key(), Some("s:auth"));
}

#[test]
fn fuzzy_metadata_does_not_create_fuzzy_screen_matches() {
    let snapshot = snapshot(vec![
        session("metadata", "Payment retry policy", "working"),
        session("screen", "Connection probe", "working"),
    ]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "screen",
        "Payment retry policy",
    );
    state.set_query("pymnt".into());
    assert_eq!(ids(&state), ["metadata"]);
    assert!(!session_rows(&state)[0].body_hit);
    assert!(session_rows(&state)[0].snippets.is_empty());
    state.set_query("screen:pymnt".into());
    assert!(state.rows().is_empty());
    state.set_query("screen:payment".into());
    assert_eq!(ids(&state), ["screen"]);
}

#[test]
fn metadata_and_literal_screen_terms_can_combine_with_highlighted_evidence() {
    let snapshot = snapshot(vec![session("one", "Payment retry policy", "working")]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "one",
        "Connecting to database\nconnect ECONNREFUSED 127.0.0.1:5432\nRetrying...",
    );
    state.set_query("pymnt ECONNREFUSED".into());
    assert_eq!(ids(&state), ["one"]);
    let row = &session_rows(&state)[0];
    assert_eq!(row.match_reason, "Title (fuzzy), Screen (literal)");
    assert!(row.body_hit);
    assert!(
        row.snippets
            .iter()
            .any(|line| line.contains("ECONNREFUSED"))
    );
    for (line, spans) in row.snippets.iter().zip(&row.snippet_spans) {
        for &(start, end) in spans {
            assert_eq!(&line[start..end], "ECONNREFUSED");
        }
    }
}

#[test]
fn field_filters_compose_without_matching_other_fields() {
    let mut first = session("first", "Payment decision", "blocked");
    first.kind = "claude".into();
    first.display_kind = "Claude".into();
    let second = session("second", "Claude payment decision", "blocked");
    let snapshot = snapshot(vec![first, second]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "first",
        "connection refused",
    );
    state.set_query(
        "agent:CLAUDE status:blocked ws:commerce path:/service title:decision screen:\"connection refused\"".into(),
    );
    assert_eq!(ids(&state), ["first"]);
    assert!(
        session_rows(&state)[0]
            .match_reason
            .contains("Screen (literal)")
    );
    state.set_query("agent:claude title:pymnt".into());
    assert!(state.rows().is_empty());
}

#[test]
fn literal_phrases_and_exclusions_preserve_interior_spaces_and_punctuation() {
    let snapshot = snapshot(vec![
        session("phrase", "Inspect connection", "working"),
        session("single", "Inspect socket", "working"),
    ]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "phrase",
        "error:timeout\nconnection  refused",
    );
    set_screen(
        &mut state,
        &snapshot.sessions,
        "single",
        "error timeout\nconnection refused",
    );
    state.set_query("screen:\"connection  refused\"".into());
    assert_eq!(ids(&state), ["phrase"]);
    assert_eq!(session_rows(&state)[0].query_terms, ["connection  refused"]);
    state.set_query("error:timeout".into());
    assert_eq!(ids(&state), ["phrase"]);
    state.set_query("connection !screen:\"connection  refused\"".into());
    assert_eq!(ids(&state), ["single"]);
    assert_eq!(session_rows(&state)[0].query_terms, ["connection"]);
}

#[test]
fn unread_screens_do_not_satisfy_screen_exclusions() {
    let snapshot = snapshot(vec![session("one", "Review", "working")]);
    let mut state = state(snapshot.clone());
    state.set_query("!needle".into());
    assert!(state.rows().is_empty());
    assert!(state.is_filtering());
    set_screen(&mut state, &snapshot.sessions, "one", "Nothing to exclude");
    assert_eq!(ids(&state), ["one"]);
    assert!(session_rows(&state)[0].query_terms.is_empty());
    state.set_query("!agent:claude".into());
    assert_eq!(ids(&state), ["one"]);
}

#[test]
fn quoted_operator_characters_are_literal_text() {
    let snapshot = snapshot(vec![session("one", "Review", "working")]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "one",
        "!important agent:claude",
    );
    state.set_query("\"!important\" \"agent:claude\"".into());
    assert_eq!(ids(&state), ["one"]);
    assert_eq!(
        session_rows(&state)[0].query_terms,
        ["!important", "agent:claude"]
    );
    state.set_query("!\"agent:claude\"".into());
    assert!(state.rows().is_empty());
}

#[test]
fn unicode_casefold_and_compatibility_keep_screen_highlights_on_original_boundaries() {
    let snapshot = snapshot(vec![session("one", "한글 검토", "working")]);
    let mut state = state(snapshot.clone());
    set_screen(
        &mut state,
        &snapshot.sessions,
        "one",
        "Straße ｐａｙｍｅｎｔ cafe\u{301}",
    );
    state.set_query("title:한글 screen:STRASSE screen:payment screen:café".into());
    assert_eq!(ids(&state), ["one"]);
    let row = &session_rows(&state)[0];
    let highlighted: Vec<_> = row
        .snippets
        .iter()
        .zip(&row.snippet_spans)
        .flat_map(|(line, spans)| spans.iter().map(move |&(start, end)| &line[start..end]))
        .collect();
    assert_eq!(highlighted, ["Straße", "ｐａｙｍｅｎｔ", "cafe\u{301}"]);
}

#[test]
fn malformed_queries_fail_visibly_before_any_snapshot_and_recover_on_edit() {
    let mut state = FinderState::default();
    state.set_query("screen:\"connection".into());
    assert!(state.query_error.as_deref().unwrap().contains("quote"));
    state.update(snapshot(vec![session("one", "Review", "working")]));
    assert!(state.rows().is_empty());
    assert!(state.selected().is_none());
    state.set_query("title:".into());
    assert!(state.query_error.is_some());
    state.set_query("title:review".into());
    assert!(state.query_error.is_none());
    assert_eq!(ids(&state), ["one"]);
    assert_eq!(state.selected_key(), Some("s:one"));
    assert!(Query::parse("\"\"").is_err());
    assert!(Query::parse("!").is_err());
}

#[test]
fn ranked_agent_search_preserves_provider_labels_and_known_aliases() {
    let mut omp = session("omp", "Review", "working");
    omp.kind = "omp".into();
    omp.display_kind = "omp".into();
    let mut agy = session("agy", "Review", "working");
    agy.kind = "agy".into();
    agy.display_kind = "agy".into();
    let mut claude = session("claude", "Review", "working");
    claude.kind = "claude".into();
    claude.display_kind = "claude".into();
    let mut state = state(snapshot(vec![omp, agy, claude]));
    for (query, expected) in [
        ("agent:\"oh my pi\"", "omp"),
        ("\"Oh My Pi\"", "omp"),
        ("agent:antigravity", "agy"),
        ("agent:claude-code", "claude"),
    ] {
        state.set_query(query.into());
        assert_eq!(ids(&state), [expected]);
        assert_eq!(
            session_rows(&state)[0].session.as_ref().unwrap().kind,
            expected
        );
    }
}

#[test]
fn late_screen_results_preserve_manual_selection_and_reject_replaced_identity() {
    let mut snapshot = snapshot(vec![
        session("one", "Payment", "working"),
        session("two", "Payment review", "working"),
        session("three", "Connection probe", "working"),
    ]);
    let mut state = state(snapshot.clone());
    state.set_query("payment".into());
    assert!(state.select("s:two"));
    set_screen(&mut state, &snapshot.sessions, "three", "payment failed");
    assert_eq!(state.selected_key(), Some("s:two"));
    let old = snapshot.sessions.clone();
    snapshot.sessions[2].native_reference =
        Some(("codex".into(), "id".into(), "replacement".into()));
    state.update(snapshot.clone());
    set_screen(&mut state, &old, "three", "payment stale evidence");
    assert!(!ids(&state).contains(&"three"));
    assert!(state.content(&old[2]).is_none());
    assert!(state.content(&snapshot.sessions[2]).is_none());
}

#[test]
fn stale_screen_revisions_are_hidden_and_same_text_freshening_rebuilds_results() {
    let mut snapshot = snapshot(vec![session("one", "Probe", "working")]);
    let mut state = state(snapshot.clone());
    state.set_query("screen:needle".into());
    assert!(state.selected().is_none());
    set_screen(&mut state, &snapshot.sessions, "one", "needle");
    assert_eq!(state.selected_key(), Some("s:one"));
    let old = snapshot.sessions.clone();
    snapshot.sessions[0].revision = 2;
    state.update(snapshot.clone());
    assert!(state.rows().is_empty());
    assert!(state.content(&snapshot.sessions[0]).is_none());
    assert!(state.selection_lost);
    set_screen(&mut state, &old, "one", "needle stale");
    assert!(state.rows().is_empty());
    set_screen(&mut state, &snapshot.sessions, "one", "needle stale");
    assert_eq!(ids(&state), ["one"]);
    assert!(state.content(&snapshot.sessions[0]).is_some());
    assert!(state.selected().is_none());
    state.move_selection(1);
    assert_eq!(state.selected_key(), Some("s:one"));
}

#[test]
#[ignore = "manual ranked-search performance measurement"]
fn ranked_search_benchmark() {
    use std::time::{Duration, Instant};

    fn median(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[samples.len() / 2]
    }

    for count in [500, 1000] {
        let snapshot = snapshot(
            (0..count)
                .map(|index| {
                    session(
                        &format!("session-{index}"),
                        &format!("Task {index} payment retry policy"),
                        "working",
                    )
                })
                .collect(),
        );
        let mut state = state(snapshot.clone());
        let context = (0..120)
            .map(|line| {
                format!(
                    "debug step {line}: running a bounded terminal screen with ordinary status text"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let contents = snapshot
            .sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                (
                    session.terminal_id.clone(),
                    ContentDocument::new(format!(
                        "{context}\nconnect ECONNREFUSED connection refused request {index}"
                    )),
                )
            })
            .collect();
        state.set_contents(contents, &snapshot.sessions);
        let mut metadata = Vec::new();
        let mut screen = Vec::new();
        let mut mixed = Vec::new();
        for index in 0..60 {
            for (query, samples) in [
                (format!("pymnt {index}"), &mut metadata),
                (
                    format!("screen:\"connection refused\" screen:{index}"),
                    &mut screen,
                ),
                (format!("pymnt screen:ECONNREFUSED {index}"), &mut mixed),
            ] {
                let started = Instant::now();
                state.set_query(query);
                samples.push(started.elapsed());
                std::hint::black_box(state.rows());
            }
        }
        eprintln!(
            "{count} sessions, 120 screen lines each, 60 samples/query class: metadata_p50={:?}, screen_p50={:?}, mixed_p50={:?}",
            median(&mut metadata),
            median(&mut screen),
            median(&mut mixed),
        );
    }
}
