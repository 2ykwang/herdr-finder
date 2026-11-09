use herdr_finder::model::{Session, Snapshot, Tab, Workspace};
use herdr_finder::state::{FinderState, GroupLevel, RowKind};
use herdr_finder::visits::VisitHistory;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn fixture() -> Snapshot {
    let mut snapshot = Snapshot::default();
    for index in 1..=5 {
        let id = format!("w{index}");
        snapshot.workspaces.push(Workspace {
            id: id.clone(),
            label: format!("Workspace {index}"),
            focused: index == 3,
        });
        for tab in [2, 1] {
            let tab_id = format!("{id}-t{tab}");
            snapshot.tabs.push(Tab {
                id: tab_id.clone(),
                workspace_id: id.clone(),
                label: "Task".into(),
                number: tab,
                focused: index == 3 && tab == 2,
            });
            for pane in [2, 1] {
                snapshot.sessions.push(Session {
                    terminal_id: format!("{tab_id}-p{pane}"),
                    pane_id: format!("{tab_id}-p{pane}"),
                    workspace_id: id.clone(),
                    tab_id: tab_id.clone(),
                    title: "Task".into(),
                    focused: index == 3 && tab == 2 && pane == 2,
                    ..Session::default()
                });
            }
        }
    }
    snapshot
}

fn workspaces(state: &FinderState) -> Vec<&str> {
    state
        .rows()
        .iter()
        .filter(|row| row.kind == RowKind::Workspace)
        .map(|row| row.workspace.id.as_str())
        .collect()
}

fn state() -> FinderState {
    let mut state = FinderState::default();
    state.set_workspace_visits(HashMap::from([
        ("w1".into(), 100),
        ("w3".into(), 1),
        ("w4".into(), 300),
        ("closed".into(), 500),
    ]));
    state.update(fixture());
    state
}

#[test]
fn browse_puts_current_first_then_visits_then_unvisited_in_native_order() {
    let state = state();
    assert_eq!(workspaces(&state), ["w3", "w4", "w1", "w2", "w5"]);
    assert_eq!(state.selected_key(), Some("s:w3-t2-p2"));
    assert_eq!(state.snapshot.as_ref().unwrap(), &fixture());
}

#[test]
fn visit_ties_and_missing_focus_keep_native_order() {
    let mut snapshot = fixture();
    for workspace in &mut snapshot.workspaces {
        workspace.focused = false;
    }
    let mut state = FinderState::default();
    state.set_workspace_visits(HashMap::from([
        ("w5".into(), 100),
        ("w2".into(), 100),
        ("w4".into(), 100),
    ]));
    state.update(snapshot);
    assert_eq!(workspaces(&state), ["w2", "w4", "w5", "w1", "w3"]);
}

#[test]
fn workspace_priority_preserves_native_tab_and_pane_order() {
    let state = state();
    let keys: Vec<_> = state
        .rows()
        .iter()
        .take(7)
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(
        keys,
        [
            "w:w3",
            "t:w3-t2",
            "s:w3-t2-p2",
            "s:w3-t2-p1",
            "t:w3-t1",
            "s:w3-t1-p2",
            "s:w3-t1-p1"
        ]
    );
}

#[test]
fn focus_output_status_and_native_reordering_do_not_shuffle_open_browse_order() {
    let mut state = state();
    let mut snapshot = fixture();
    snapshot.workspaces.reverse();
    for workspace in &mut snapshot.workspaces {
        workspace.focused = workspace.id == "w5";
    }
    for session in &mut snapshot.sessions {
        session.focused = session.terminal_id == "w5-t1-p1";
        session.status = "done".into();
        session.revision += 50;
    }
    state.set_workspace_visits(HashMap::from([("w5".into(), 900)]));
    state.update(snapshot);
    assert_eq!(workspaces(&state), ["w3", "w4", "w1", "w2", "w5"]);
    assert_eq!(state.selected_key(), Some("s:w3-t2-p2"));
}

#[test]
fn new_workspaces_append_and_closed_workspaces_disappear() {
    let mut state = state();
    let mut snapshot = fixture();
    snapshot.workspaces.retain(|workspace| workspace.id != "w4");
    snapshot.tabs.retain(|tab| tab.workspace_id != "w4");
    snapshot
        .sessions
        .retain(|session| session.workspace_id != "w4");
    snapshot.workspaces.insert(
        0,
        Workspace {
            id: "new".into(),
            label: "New workspace".into(),
            focused: false,
        },
    );
    state.update(snapshot);
    assert_eq!(workspaces(&state), ["w3", "w1", "w2", "w5", "new"]);
}

#[test]
fn search_ties_remain_native_and_clearing_restores_frozen_browse_order() {
    let mut state = state();
    state.set_query("Task".into());
    assert_eq!(workspaces(&state), ["w1", "w2", "w3", "w4", "w5"]);
    state.set_query(String::new());
    assert_eq!(workspaces(&state), ["w3", "w4", "w1", "w2", "w5"]);
}

#[test]
fn group_navigation_follows_browse_order() {
    let mut state = state();
    assert!(state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected().unwrap().workspace.id, "w4");
    assert!(state.move_target(1, GroupLevel::Workspace));
    assert_eq!(state.selected().unwrap().workspace.id, "w1");
    assert!(state.move_target(-1, GroupLevel::Workspace));
    assert_eq!(state.selected().unwrap().workspace.id, "w4");
}

#[test]
fn reopening_uses_the_new_current_workspace_and_updated_visits() {
    let mut snapshot = fixture();
    for workspace in &mut snapshot.workspaces {
        workspace.focused = workspace.id == "w5";
    }
    let mut reopened = FinderState::default();
    reopened.set_workspace_visits(HashMap::from([("w2".into(), 900), ("w3".into(), 800)]));
    reopened.update(snapshot);
    assert_eq!(workspaces(&reopened), ["w5", "w2", "w3", "w1", "w4"]);
}

#[test]
fn visit_hook_records_plugin_events_without_contacting_the_server() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("absent-server.sock");
    let history = VisitHistory::new(directory.path(), Path::new(&socket));
    for (kind, present) in [("workspace_focused", true), ("workspace_closed", false)] {
        let event = serde_json::json!({
            "event": kind,
            "data": {"type": kind, "workspace_id": "w1"},
        });
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-finder"))
            .arg("record-visit")
            .env("HERDR_SOCKET_PATH", &socket)
            .env("HERDR_PLUGIN_STATE_DIR", directory.path())
            .env("HERDR_PLUGIN_EVENT_JSON", event.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert_eq!(history.recent().unwrap().contains_key("w1"), present);
    }
}
