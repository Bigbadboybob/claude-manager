//! Sidebar cost on a large manifest. Laptop diag 2026-11-05: 4,196
//! workspaces, 3,942 of them closed with tombstones, ~254 open with ~290
//! sessions; `phase=input` took 200–600 ms per key and
//! `drain_backend_events` 250–300 ms. The fixture mirrors that shape so the
//! cost of a key, a frame and a task-feed update can be compared with and
//! without the closed workspaces. The planning snapshot holds every task
//! (done first, so a lookup of a running task scans past them). Ignored by
//! default (it times things):
//! `cargo test -p claude-manager-tui sidebar_cost -- --ignored --nocapture`.

use super::sections::layout_tests::{bare_ws, task, test_app};
use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::time::Instant;

const OPEN: usize = 254;
const ROOTS: usize = 20;
const DONE_TASKS: usize = 4_000;

fn live_session(uid: &str, tid: &str, host: &cm_daemon::host_id::HostId) -> TerminalSession {
    let pty = crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
        .expect("test PTY");
    let mut ts = super::model::make_simple_session_with_uid(uid.into(), uid, "codex", pty, None);
    ts.task_id = Some(tid.into());
    ts.host_id = host.clone();
    ts.session.exited = false;
    ts
}

fn api_task(id: &str, status: &str, parent: Option<&str>) -> crate::api::Task {
    serde_json::from_value(serde_json::json!({
        "id": id, "created_at": "2026-10-01T00:00:00Z",
        "repo_url": "git@github.com:x/y.git", "repo_branch": "main",
        "name": id, "prompt": null, "status": status, "worker_vm": null,
        "worker_zone": null, "blocked_at": null, "session_id": null,
        "wip_branch": null, "is_cloud": false, "kind": "task",
        "parent_task_id": parent, "project": "p",
    }))
    .unwrap()
}

/// `closed` closed workspaces (each with ~0.55 tombstones, like the diag)
/// plus the same 254 open ones every time, and the full task snapshot the
/// task feed delivers (running + done).
fn big_app(closed: usize) -> (App, Vec<crate::api::Task>) {
    let mut app = test_app();
    app.sidebar_view = SidebarView::Task;
    let sessions_host = cm_daemon::host_id::HostId::new("sessions");
    let manager = cm_daemon::host_id::HostId::new("manager");
    for (i, name) in ["SEJD", "Swarm-Focused-Design", "Prob Pipeline"].into_iter().enumerate() {
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: format!("sec-{i}"),
            name: name.into(),
            color: None,
            folded: false,
        });
    }
    let mut api = Vec::new();
    for i in 0..OPEN {
        let ws_id = format!("open-{i:04}");
        let tid = format!("task-{i:04}");
        let parent = (i >= ROOTS).then(|| format!("task-{:04}", i % ROOTS));
        let mut ws = bare_ws(&ws_id);
        ws.host_id = sessions_host.clone();
        ws.worktree_path = Some(format!("/home/lucas/.cm/worktrees/wt-{i}").into());
        ws.sessions.push(live_session(&format!("ts-{i:04}-0"), &tid, &sessions_host));
        if i % 7 == 0 {
            ws.sessions.push(live_session(&format!("ts-{i:04}-1"), &tid, &sessions_host));
        }
        app.workspaces.push(ws);
        app.tasks.push(task(&tid, &ws_id, parent.as_deref()));
        if i < ROOTS && i % 3 != 2 {
            app.workspace_sections.insert(ws_id.clone(), format!("sec-{}", i % 3));
        }
        api.push(api_task(&tid, "running", parent.as_deref()));
    }
    for i in 0..closed {
        let ws_id = format!("closed-{i:05}");
        let mut ws = bare_ws(&ws_id);
        ws.name = format!("agent: detective-{i}");
        ws.is_closed = true;
        ws.host_id = manager.clone();
        ws.worktree_path = Some(format!("/home/lucas/.cm/worktrees/predictionTrading-detective-{i}").into());
        for t in 0..usize::from(i % 20 < 11) {
            ws.tombstones.push(serde_json::from_value(serde_json::json!({
                "uid": format!("ts-c{i}-{t}"), "label": "detective", "session_type": "claude",
                "task_id": format!("done-{:04}", i % DONE_TASKS), "generation": 0,
                "exited_at": 1_793_000_000.0,
            })).unwrap());
        }
        app.folded_workspaces.insert(ws_id.clone());
        if i % 2 == 0 {
            app.auto_close_workspaces.insert(ws_id.clone());
        }
        app.workspaces.push(ws);
    }
    for i in 0..DONE_TASKS {
        api.push(api_task(&format!("done-{i:04}"), "done", Some(&format!("task-{:04}", i % OPEN))));
    }
    app.reconcile_tasks(api.clone());
    let mut planning: Vec<crate::api::Task> = api.iter().filter(|t| t.status == "done").cloned().collect();
    planning.extend(api.iter().filter(|t| t.status != "done").cloned());
    app.planning.update_from_api(planning);
    assert_eq!(app.planning.loaded_tasks().count(), api.len());
    (app, api)
}

fn mean_ms(n: usize, mut f: impl FnMut()) -> f64 {
    let started = Instant::now();
    for _ in 0..n {
        f();
    }
    started.elapsed().as_secs_f64() * 1000.0 / n as f64
}

fn measure(closed: usize) -> [f64; 5] {
    let (mut app, api) = big_app(closed);
    let alt_j = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT));
    app.cursor = Cursor::Session(0, 0);
    app.handle_event(&alt_j);
    let key = mean_ms(40, || {
        app.handle_event(&alt_j);
    });
    // A key after anything that changes the tree (agent activity, a
    // session, a task) rebuilds it.
    let key_rebuild = mean_ms(20, || {
        *app.task_tree_cache.borrow_mut() = None;
        app.handle_event(&alt_j);
    });
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 70)).unwrap();
    let draw = mean_ms(20, || {
        terminal.draw(|frame| app.draw_session_list(frame, frame.area())).unwrap();
    });
    let mut full = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 50)).unwrap();
    let frame = mean_ms(10, || {
        full.draw(|frame| app.draw(frame)).unwrap();
    });
    let reconcile = mean_ms(3, || app.reconcile_tasks(api.clone()));
    [key, key_rebuild, draw, frame, reconcile]
}

/// What one tree rebuild and one frame call, timed one by one.
fn breakdown(closed: usize) {
    let (app, _) = big_app(closed);
    let members = app.continuous_members();
    let rows: [(&str, f64); 8] = [
        ("task_parent_map", mean_ms(20, || drop(app.task_parent_map()))),
        ("continuous_members", mean_ms(20, || drop(app.continuous_members()))),
        ("visible workspaces", mean_ms(20, || drop(app.task_view_visible_workspaces(&members)))),
        ("org_index", mean_ms(20, || drop(app.org_index()))),
        ("divisions_by_task", mean_ms(20, || drop(app.planning.divisions_by_task()))),
        ("section_lookup", mean_ms(20, || drop(app.section_lookup()))),
        ("all_leads", mean_ms(20, || drop(app.all_leads()))),
        ("tree rebuild", mean_ms(10, || {
            *app.task_tree_cache.borrow_mut() = None;
            drop(app.task_tree_shared());
        })),
    ];
    for (name, ms) in rows {
        eprintln!("PART {name:<22} {closed} closed: {ms:>8.2} ms");
    }
}

#[test]
#[ignore]
fn sidebar_cost_breakdown() {
    breakdown(3_942);
}

#[test]
#[ignore]
fn sidebar_cost_with_4000_closed_workspaces() {
    let base = measure(0);
    let big = measure(3_942);
    for (name, b, g) in [
        ("key (A-j)", base[0], big[0]),
        ("key after a tree change", base[1], big[1]),
        ("sidebar draw", base[2], big[2]),
        ("full frame draw", base[3], big[3]),
        ("reconcile_tasks (task feed)", base[4], big[4]),
    ] {
        eprintln!("PERF {name:<28} 0 closed: {b:>8.2} ms   3942 closed: {g:>8.2} ms");
    }
}
