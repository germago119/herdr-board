//! Daemon-level Jira integration tests, hermetic against a fake `jira`
//! executable (never a real Jira instance): `jira.import` (create-or-select
//! board/columns, create-or-update cards, idempotency, no-dispatch guarantee)
//! and `card.move`'s Jira status sync (transition-then-verify before commit,
//! fail-closed on transition/verify failure, same-column reorder and
//! unlinked-card moves never contacting Jira, and the reconciliation-required
//! path when a local write unexpectedly fails after a confirmed remote
//! transition).

use super::*;
use board_core::config::JiraConfig;
use board_core::db::LifecycleFaultPoint;

/// A fake `jira` binary at `dir`/`jira-fixture` running `body` (a `/bin/sh`
/// script fragment), executable. Mirrors `board-core/tests/jira.rs`'s pattern.
#[cfg(unix)]
fn fixture_jira_bin(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = format!("#!/bin/sh\n{body}\n");
    let bin = dir.path().join("jira-fixture");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

fn jira_config(executable: std::path::PathBuf) -> JiraConfig {
    JiraConfig {
        enabled: true,
        executable: executable.to_str().unwrap().to_string(),
        project: "PROJ".into(),
        ..Default::default()
    }
}

fn columns_by_name(d: &Arc<Daemon>, board_id: i64) -> std::collections::HashMap<String, i64> {
    let snap = handle_request(d, "board.get", json!({ "board_id": board_id })).unwrap();
    snap["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["id"].as_i64().unwrap(),
            )
        })
        .collect()
}

// -- jira.import --------------------------------------------------------------

#[test]
fn jira_import_is_rejected_when_jira_is_disabled() {
    let d = test_daemon(Config::default());
    let err = handle_request(&d, "jira.import", json!({})).unwrap_err();
    assert!(matches!(err, Error::JiraUnavailable(_)));
    assert!(err.to_string().contains("disabled"));
}

#[test]
fn jira_import_requires_project_when_enabled_without_override() {
    let d = testkit::daemon()
        .jira(JiraConfig {
            enabled: true,
            ..Default::default()
        })
        .build_daemon();
    let err = handle_request(&d, "jira.import", json!({})).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
    let msg = err.to_string();
    assert!(msg.contains("[jira].project must be set"), "{msg}");
    assert!(msg.contains("--jira-project"), "{msg}");
}

#[cfg(unix)]
#[test]
fn jira_import_creates_a_board_three_columns_and_maps_cards_by_status_category() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        r#"cat <<'HBEOF'
[
  {"key": "PROJ-1", "fields": {"summary": "Fix login", "description": "d1", "status": {"name": "To Do"}}},
  {"key": "PROJ-2", "fields": {"summary": "Build export", "description": null, "status": {"name": "In Progress"}}},
  {"key": "PROJ-3", "fields": {"summary": "Ship release", "description": null, "status": {"name": "Done"}}}
]
HBEOF
"#,
    );
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();

    let result = handle_request(&d, "jira.import", json!({})).unwrap();
    assert_eq!(result["fetched"].as_u64(), Some(3));
    assert_eq!(result["created"].as_u64(), Some(3));
    assert_eq!(result["updated"].as_u64(), Some(0));
    assert_eq!(result["board_name"].as_str(), Some("Jira: PROJ"));
    let board_id = result["board_id"].as_i64().unwrap();

    let columns = columns_by_name(&d, board_id);
    assert!(columns.contains_key("Backlog"));
    assert!(columns.contains_key("In Progress"));
    assert!(columns.contains_key("Done"));

    let snap = handle_request(&d, "board.get", json!({ "board_id": board_id })).unwrap();
    let cards = snap["cards"].as_array().unwrap();
    let find = |key: &str| {
        cards
            .iter()
            .find(|c| c["external_key"].as_str() == Some(key))
            .unwrap_or_else(|| panic!("no card linked to {key}: {cards:?}"))
    };
    assert_eq!(
        find("PROJ-1")["column_id"].as_i64(),
        Some(columns["Backlog"]),
        "an unmapped status name defaults to Backlog"
    );
    assert_eq!(
        find("PROJ-2")["column_id"].as_i64(),
        Some(columns["In Progress"])
    );
    assert_eq!(find("PROJ-3")["column_id"].as_i64(), Some(columns["Done"]));
    for key in ["PROJ-1", "PROJ-2", "PROJ-3"] {
        assert_eq!(find(key)["external_source"].as_str(), Some("jira"));
    }
}

#[cfg(unix)]
#[test]
fn jira_import_is_idempotent_and_updates_moved_or_renamed_linked_cards_without_duplicating() {
    let dir = tempfile::tempdir().unwrap();
    let bin_path = dir.path().join("jira-fixture");
    let write_script = |body: &str| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&bin_path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    write_script(
        r#"cat <<'HBEOF'
[{"key": "PROJ-1", "fields": {"summary": "Fix login", "description": "d1", "status": {"name": "To Do"}}}]
HBEOF
"#,
    );
    let d = testkit::daemon()
        .jira(jira_config(bin_path.clone()))
        .build_daemon();

    let first = handle_request(&d, "jira.import", json!({})).unwrap();
    assert_eq!(first["created"].as_u64(), Some(1));
    let board_id = first["board_id"].as_i64().unwrap();

    // Re-import with no changes at all: no duplicate, no write.
    let second = handle_request(&d, "jira.import", json!({})).unwrap();
    assert_eq!(second["board_id"].as_i64(), Some(board_id));
    assert_eq!(second["created"].as_u64(), Some(0));
    assert_eq!(second["updated"].as_u64(), Some(0));
    assert_eq!(second["unchanged"].as_u64(), Some(1));

    // The same issue changes title and crosses into Done: must update the
    // existing linked card in place, never create a second one.
    write_script(
        r#"cat <<'HBEOF'
[{"key": "PROJ-1", "fields": {"summary": "Fix login (renamed)", "description": "d1", "status": {"name": "Done"}}}]
HBEOF
"#,
    );
    let third = handle_request(&d, "jira.import", json!({})).unwrap();
    assert_eq!(third["created"].as_u64(), Some(0));
    assert_eq!(third["updated"].as_u64(), Some(1));

    let columns = columns_by_name(&d, board_id);
    let snap = handle_request(&d, "board.get", json!({ "board_id": board_id })).unwrap();
    let cards = snap["cards"].as_array().unwrap();
    assert_eq!(
        cards.len(),
        1,
        "the import must never duplicate a linked card"
    );
    assert_eq!(cards[0]["title"].as_str(), Some("Fix login (renamed)"));
    assert_eq!(cards[0]["column_id"].as_i64(), Some(columns["Done"]));
}

#[cfg(unix)]
#[test]
fn jira_import_never_dispatches_even_when_landing_on_a_pre_existing_auto_column() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        r#"cat <<'HBEOF'
[{"key": "PROJ-9", "fields": {"summary": "Auto-landed", "description": null, "status": {"name": "Done"}}}]
HBEOF
"#,
    );
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let effects = Arc::new(Mutex::new(Vec::new()));
    *d.effect_log.lock().unwrap() = Some(effects.clone());

    // Pre-create the destination board with a "Done" column already wired to
    // auto-dispatch, exactly the situation the import must not exploit.
    let board_id = {
        let db = d.store.lock();
        let project_id = db.selected_project_id().unwrap().unwrap_or(BOARD_ID);
        let board = db.create_board(project_id, "Jira: PROJ").unwrap();
        db.create_column(&ColumnCreateParams {
            name: "Done".into(),
            board_id: Some(board.id),
            trigger: Some(Trigger::Auto),
            ..Default::default()
        })
        .unwrap();
        board.id
    };

    let result = handle_request(&d, "jira.import", json!({})).unwrap();
    assert_eq!(result["board_id"].as_i64(), Some(board_id));
    assert_eq!(result["created"].as_u64(), Some(1));

    let db = d.store.lock();
    let snap_cards = db.list_all_cards().unwrap();
    let imported = snap_cards
        .iter()
        .find(|c| c.external_key.as_deref() == Some("PROJ-9"))
        .unwrap();
    assert_eq!(imported.status, CardStatus::Idle, "import never enqueues");
    assert!(db.list_runs(imported.id).unwrap().is_empty());
    drop(db);
    let logged = effects.lock().unwrap();
    assert!(
        !logged.contains(&"dispatch_wake"),
        "import landing on an auto column must never wake dispatch: {logged:?}"
    );
}

// -- card.move Jira status sync -----------------------------------------------

/// A card already linked to Jira, on a Manual "Backlog"-named column.
fn linked_card(d: &Arc<Daemon>, key: &str) -> (i64, i64) {
    let db = d.store.lock();
    let backlog = db
        .create_column(&ColumnCreateParams {
            name: "Backlog".into(),
            trigger: Some(Trigger::Manual),
            ..Default::default()
        })
        .unwrap();
    let card = db
        .create_card(&CardCreateParams {
            title: "linked".into(),
            column_id: Some(backlog.id),
            external_source: Some("jira".into()),
            external_key: Some(key.into()),
            ..Default::default()
        })
        .unwrap();
    (card.id, backlog.id)
}

#[cfg(unix)]
#[test]
fn card_move_transitions_and_verifies_jira_before_committing_a_cross_column_move() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("move-argv.txt");
    let bin = fixture_jira_bin(
        &dir,
        &format!(
            r#"
case "$4" in
  move) printf '%s %s %s\n' "$2" "$5" "$6" > {marker:?} ; exit 0 ;;
  view) cat <<'HBEOF'
{{"key": "PROJ-1", "fields": {{"status": {{"name": "Done"}}}}}}
HBEOF
    ;;
esac
"#
        ),
    );
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, _backlog_id) = linked_card(&d, "PROJ-1");
    let done = handle_request(
        &d,
        "column.create",
        json!({ "name": "Done", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();

    let moved =
        handle_request(&d, "card.move", json!({ "id": card_id, "column_id": done })).unwrap();
    assert_eq!(moved["column_id"].as_i64(), Some(done));

    let recorded = std::fs::read_to_string(&marker).expect("the transition must have run");
    assert_eq!(recorded.trim(), "PROJ PROJ-1 Done");
}

#[cfg(unix)]
#[test]
fn card_move_does_not_hold_the_store_lock_during_jira_io() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    let bin = fixture_jira_bin(
        &dir,
        &format!(
            r#"
case "$4" in
  move)
    touch {started:?}
    while [ ! -e {release:?} ]; do sleep 0.01; done
    exit 0
    ;;
  view) cat <<'HBEOF'
{{"key": "PROJ-1", "fields": {{"status": {{"name": "Done"}}}}}}
HBEOF
    ;;
esac
"#
        ),
    );
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, _) = linked_card(&d, "PROJ-1");
    let done = handle_request(
        &d,
        "column.create",
        json!({ "name": "Done", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();

    let moving_daemon = d.clone();
    let move_thread = std::thread::spawn(move || {
        handle_request(
            &moving_daemon,
            "card.move",
            json!({ "id": card_id, "column_id": done }),
        )
    });
    for _ in 0..100 {
        if started.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(started.exists(), "the fake Jira transition did not start");

    let reading_daemon = d.clone();
    let (tx, rx) = mpsc::channel();
    let read_thread = std::thread::spawn(move || {
        tx.send(handle_request(
            &reading_daemon,
            "card.get",
            json!({ "id": card_id }),
        ))
        .unwrap();
    });
    let read_result = rx.recv_timeout(Duration::from_secs(1));
    std::fs::write(&release, "").unwrap();

    assert!(
        read_result
            .expect("card.get blocked on Jira subprocess I/O")
            .is_ok(),
        "card.get failed while Jira synchronization was in progress"
    );
    read_thread.join().unwrap();
    assert!(move_thread.join().unwrap().is_ok());
}

#[cfg(unix)]
#[test]
fn card_move_leaves_the_card_column_unchanged_when_the_jira_transition_fails() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "echo 'no such transition' >&2\nexit 1");
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, backlog_id) = linked_card(&d, "PROJ-1");
    let done = handle_request(
        &d,
        "column.create",
        json!({ "name": "Done", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();

    let err =
        handle_request(&d, "card.move", json!({ "id": card_id, "column_id": done })).unwrap_err();
    assert!(matches!(err, Error::JiraUnavailable(_)));
    let card = d.store.lock().require_card(card_id).unwrap();
    assert_eq!(
        card.column_id, backlog_id,
        "a failed remote transition must leave the local column untouched"
    );
}

#[cfg(unix)]
#[test]
fn card_move_leaves_the_card_column_unchanged_when_verify_status_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        r#"
case "$4" in
  move) exit 0 ;;
  view) cat <<'HBEOF'
{"key": "PROJ-1", "fields": {"status": {"name": "To Do"}}}
HBEOF
    ;;
esac
"#,
    );
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, backlog_id) = linked_card(&d, "PROJ-1");
    let done = handle_request(
        &d,
        "column.create",
        json!({ "name": "Done", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();

    let err =
        handle_request(&d, "card.move", json!({ "id": card_id, "column_id": done })).unwrap_err();
    assert!(matches!(err, Error::JiraUnavailable(_)));
    let card = d.store.lock().require_card(card_id).unwrap();
    assert_eq!(card.column_id, backlog_id);
}

#[cfg(unix)]
#[test]
fn card_move_same_column_reorder_never_contacts_jira() {
    let dir = tempfile::tempdir().unwrap();
    // Any invocation at all fails loudly, proving a same-column reorder never
    // shells out.
    let bin = fixture_jira_bin(&dir, "echo 'must never run' >&2\nexit 1");
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, backlog_id) = linked_card(&d, "PROJ-1");
    handle_request(
        &d,
        "card.create",
        json!({ "column_id": backlog_id, "title": "sibling" }),
    )
    .unwrap();

    let moved = handle_request(
        &d,
        "card.move",
        json!({ "id": card_id, "column_id": backlog_id, "position": 1 }),
    )
    .unwrap();
    assert_eq!(moved["column_id"].as_i64(), Some(backlog_id));
}

#[cfg(unix)]
#[test]
fn card_move_never_contacts_jira_for_a_card_without_an_external_link() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "echo 'must never run' >&2\nexit 1");
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let backlog = handle_request(
        &d,
        "column.create",
        json!({ "name": "Backlog", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();
    let done = handle_request(
        &d,
        "column.create",
        json!({ "name": "Done", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();
    let card = handle_request(
        &d,
        "card.create",
        json!({ "column_id": backlog, "title": "unlinked" }),
    )
    .unwrap();
    let card_id = card["id"].as_i64().unwrap();

    let moved =
        handle_request(&d, "card.move", json!({ "id": card_id, "column_id": done })).unwrap();
    assert_eq!(moved["column_id"].as_i64(), Some(done));
}

#[cfg(unix)]
#[test]
fn card_move_never_contacts_jira_when_the_destination_column_name_does_not_map_to_a_category() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "echo 'must never run' >&2\nexit 1");
    let d = testkit::daemon().jira(jira_config(bin)).build_daemon();
    let (card_id, _backlog_id) = linked_card(&d, "PROJ-1");
    let review = handle_request(
        &d,
        "column.create",
        json!({ "name": "Review", "trigger": "manual" }),
    )
    .unwrap()["id"]
        .as_i64()
        .unwrap();

    let moved = handle_request(
        &d,
        "card.move",
        json!({ "id": card_id, "column_id": review }),
    )
    .unwrap();
    assert_eq!(moved["column_id"].as_i64(), Some(review));
}

#[cfg(unix)]
#[test]
fn card_move_returns_reconciliation_required_when_local_persistence_fails_after_a_confirmed_remote_transition(
) {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        r#"
case "$4" in
  move) exit 0 ;;
  view) cat <<'HBEOF'
{"key": "PROJ-1", "fields": {"status": {"name": "Done"}}}
HBEOF
    ;;
esac
"#,
    );
    let db_path = dir.path().join("reconcile.db");
    let (db, fault) = testkit::fault_db(
        &db_path,
        LifecycleFaultPoint::EnqueueAfterRunInsert,
        "injected enqueue fault",
    );
    let (card_id, _backlog_id) = {
        let auto_done = db
            .create_column(&ColumnCreateParams {
                name: "Done".into(),
                trigger: Some(Trigger::Auto),
                ..Default::default()
            })
            .unwrap();
        let card = db
            .create_card(&CardCreateParams {
                title: "linked".into(),
                external_source: Some("jira".into()),
                external_key: Some("PROJ-1".into()),
                ..Default::default()
            })
            .unwrap();
        (card.id, auto_done.id)
    };
    let done_id = db
        .list_columns(BOARD_ID)
        .unwrap()
        .into_iter()
        .find(|c| c.name == "Done")
        .unwrap()
        .id;
    let d = testkit::daemon()
        .db(db)
        .db_path(db_path.clone())
        .socket_path(dir.path().join("board.sock"))
        .jira(jira_config(bin))
        .build_daemon();
    fault.arm();

    let err = handle_request(
        &d,
        "card.move",
        json!({ "id": card_id, "column_id": done_id }),
    )
    .unwrap_err();
    assert!(matches!(err, Error::ReconciliationRequired(_)));
    let msg = err.to_string();
    assert!(msg.contains("PROJ-1"), "{msg}");
    assert!(fault.observed(), "the injected fault must have fired");
}
