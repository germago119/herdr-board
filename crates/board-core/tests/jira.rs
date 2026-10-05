//! Hermetic tests for the `jira` CLI adapter (`board_core::jira`) against a
//! fake `jira` executable — no real Jira instance is ever contacted, and none
//! of these tests read or require credentials.
//!
//! Mirrors `crates/board-core/tests/agy_catalog.rs`'s fixture-binary pattern:
//! a tiny `#!/bin/sh` script stands in for jira-cli v1.7.0 and prints/exits
//! exactly what the field-verified contract in `jira.rs`'s module doc says it
//! does.

use std::fs;

use board_core::config::JiraConfig;
use board_core::jira::{
    category_for_column_name, category_for_status, fetch_all_issues, list_argv, move_argv,
    transition_issue, verify_status, view_argv, JiraCategory, JiraIssue,
};
use board_core::Error;

/// A fake `jira` binary at `dir`/`name` running `body` (a `/bin/sh` script
/// fragment), executable.
#[cfg(unix)]
fn fixture_jira_bin(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = format!("#!/bin/sh\n{body}\n");
    let bin = dir.path().join(name);
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

fn config_with(executable: std::path::PathBuf) -> JiraConfig {
    JiraConfig {
        executable: executable.to_str().unwrap().to_string(),
        project: "PROJ".into(),
        ..Default::default()
    }
}

// -- argv shapes (field-verified against jira-cli v1.7.0) -------------------

#[test]
fn list_argv_matches_the_exact_verified_command() {
    let config = JiraConfig {
        project: "PROJ".into(),
        jql: "assignee = currentUser() AND sprint in openSprints()".into(),
        ..Default::default()
    };
    assert_eq!(
        list_argv(&config, 0, 100),
        vec![
            "jira",
            "-p",
            "PROJ",
            "issue",
            "list",
            "--jql",
            "assignee = currentUser() AND sprint in openSprints()",
            "--raw",
            "--paginate",
            "0:100",
        ]
    );
}

#[test]
fn move_argv_matches_the_exact_verified_command() {
    let config = JiraConfig {
        project: "PROJECT".into(),
        ..Default::default()
    };
    assert_eq!(
        move_argv(&config, "PROJECT-1", "Done"),
        vec![
            "jira",
            "-p",
            "PROJECT",
            "issue",
            "move",
            "PROJECT-1",
            "Done"
        ]
    );
}

#[test]
fn view_argv_matches_the_exact_verified_command() {
    let config = JiraConfig {
        project: "PROJECT".into(),
        ..Default::default()
    };
    assert_eq!(
        view_argv(&config, "PROJECT-1"),
        vec![
            "jira",
            "-p",
            "PROJECT",
            "issue",
            "view",
            "PROJECT-1",
            "--raw"
        ]
    );
}

// -- category mapping ---------------------------------------------------------

#[test]
fn column_name_matching_is_case_insensitive_and_trimmed() {
    assert_eq!(
        category_for_column_name("  backlog "),
        Some(JiraCategory::Backlog)
    );
    assert_eq!(
        category_for_column_name("In Progress"),
        Some(JiraCategory::InProgress)
    );
    assert_eq!(category_for_column_name("DONE"), Some(JiraCategory::Done));
    assert_eq!(category_for_column_name("Todo"), None);
}

#[test]
fn status_mapping_defaults_unmapped_statuses_to_backlog() {
    let map = JiraConfig::default().status_map;
    assert_eq!(category_for_status(&map, "Done"), JiraCategory::Done);
    assert_eq!(
        category_for_status(&map, "In Progress"),
        JiraCategory::InProgress
    );
    assert_eq!(category_for_status(&map, "To Do"), JiraCategory::Backlog);
    assert_eq!(
        category_for_status(&map, "Some Custom Status"),
        JiraCategory::Backlog
    );
}

// -- fetch_all_issues ---------------------------------------------------------

const ONE_PAGE_JSON: &str = r#"[
  {"key": "PROJ-1", "fields": {"summary": "Fix login", "description": "plain text", "status": {"name": "In Progress"}}},
  {"key": "PROJ-2", "fields": {"summary": "Add export", "description": null, "status": {"name": "Done"}}},
  {"key": "PROJ-3", "fields": {"summary": "Investigate crash", "description": {"type": "doc", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Repro steps"}]}]}, "status": {"name": "To Do"}}}
]"#;

#[cfg(unix)]
#[test]
fn fetch_all_issues_parses_a_single_short_page() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        &format!("cat <<'HBEOF'\n{ONE_PAGE_JSON}\nHBEOF\n"),
    );
    let config = config_with(bin);
    let issues = fetch_all_issues(&config).expect("a short page must parse");
    assert_eq!(issues.len(), 3);
    assert_eq!(
        issues[0],
        JiraIssue {
            key: "PROJ-1".into(),
            summary: "Fix login".into(),
            description: "plain text".into(),
            status_name: "In Progress".into(),
        }
    );
    assert_eq!(
        issues[1].description, "",
        "a null description flattens to empty"
    );
    assert!(
        issues[2].description.contains("Repro steps"),
        "an ADF description flattens to its text nodes: {:?}",
        issues[2].description
    );
}

#[cfg(unix)]
#[test]
fn fetch_all_issues_pages_until_a_short_page() {
    // A fixture that returns a full page (matching MAX_PAGE_SIZE would be
    // slow to fabricate; instead this drives page_size down to 2 via config
    // so a 2-then-1 sequence exercises the loop) keyed off $2 (the
    // `OFFSET:LIMIT` argument), then a short page on the second call.
    let dir = tempfile::tempdir().unwrap();
    let script = r#"
offset_limit="$9"
case "$offset_limit" in
  0:2)
    cat <<'HBEOF'
[
  {"key": "P-1", "fields": {"summary": "one", "description": null, "status": {"name": "Backlog"}}},
  {"key": "P-2", "fields": {"summary": "two", "description": null, "status": {"name": "Backlog"}}}
]
HBEOF
    ;;
  2:2)
    cat <<'HBEOF'
[
  {"key": "P-3", "fields": {"summary": "three", "description": null, "status": {"name": "Backlog"}}}
]
HBEOF
    ;;
  *)
    echo "unexpected paginate arg: $offset_limit" >&2
    exit 9
    ;;
esac
"#;
    let bin = fixture_jira_bin(&dir, "jira-fixture", script);
    let config = JiraConfig {
        page_size: 2,
        ..config_with(bin)
    };
    let issues = fetch_all_issues(&config).expect("pagination must follow short-page logic");
    let keys: Vec<&str> = issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["P-1", "P-2", "P-3"]);
}

#[cfg(unix)]
#[test]
fn fetch_all_issues_classifies_the_documented_empty_result_as_zero_issues() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        "echo 'No result found for given query in project PROJ' >&2\nexit 1",
    );
    let config = config_with(bin);
    let issues = fetch_all_issues(&config).expect("the documented empty case is not an error");
    assert!(issues.is_empty());
}

#[cfg(unix)]
#[test]
fn fetch_all_issues_treats_every_other_nonzero_exit_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        "echo 'authentication failed' >&2\nexit 2",
    );
    let config = config_with(bin);
    let err = fetch_all_issues(&config).expect_err("a non-empty-result nonzero exit is an error");
    assert!(matches!(err, Error::JiraUnavailable(_)));
    assert!(
        format!("{err}").contains("authentication failed"),
        "the error must carry the CLI's own message: {err}"
    );
}

#[cfg(unix)]
#[test]
fn fetch_all_issues_requires_a_configured_project() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "jira-fixture", "exit 9");
    let config = JiraConfig {
        project: String::new(),
        ..config_with(bin)
    };
    let err = fetch_all_issues(&config).expect_err("missing project must fail before invoking CLI");
    assert!(matches!(err, Error::Config(_)));
    assert!(format!("{err}").contains("[jira].project must be set"));
}

#[cfg(unix)]
#[test]
fn transition_and_view_require_a_configured_project() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "jira-fixture", "exit 9");
    let config = JiraConfig {
        project: String::new(),
        ..config_with(bin)
    };

    let move_err = transition_issue(&config, "PROJ-1", "Done").unwrap_err();
    assert!(matches!(move_err, Error::Config(_)));
    assert!(format!("{move_err}").contains("[jira].project must be set"));

    let view_err = verify_status(&config, "PROJ-1", JiraCategory::Done).unwrap_err();
    assert!(matches!(view_err, Error::Config(_)));
    assert!(format!("{view_err}").contains("[jira].project must be set"));
}

#[cfg(unix)]
#[test]
fn fetch_all_issues_strips_ansi_from_error_messages() {
    let dir = tempfile::tempdir().unwrap();
    // A red "denied" (SGR 31) followed by a reset (SGR 0) — must come through
    // as plain text in the error.
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        "printf '\\033[31mdenied\\033[0m\\n' >&2\nexit 5",
    );
    let config = config_with(bin);
    let err = fetch_all_issues(&config).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("denied"));
    assert!(
        !msg.contains('\u{1b}'),
        "ANSI escape must be stripped: {msg:?}"
    );
}

// -- transition_issue / verify_status -----------------------------------------

#[cfg(unix)]
#[test]
fn transition_issue_succeeds_on_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(&dir, "jira-fixture", "exit 0");
    let config = config_with(bin);
    transition_issue(&config, "PROJ-1", "Done").expect("exit 0 must succeed");
}

#[cfg(unix)]
#[test]
fn transition_issue_fails_on_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        "echo 'no such transition: Done' >&2\nexit 1",
    );
    let config = config_with(bin);
    let err = transition_issue(&config, "PROJ-1", "Done").unwrap_err();
    assert!(matches!(err, Error::JiraUnavailable(_)));
}

#[cfg(unix)]
#[test]
fn transition_issue_rejects_an_invalid_key_without_invoking_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    // If this script ever ran, the test would still fail closed (exit 9 is
    // not exit 0), proving the rejection is argv-level, not a lucky exit.
    let bin = fixture_jira_bin(&dir, "jira-fixture", "exit 9");
    let config = config_with(bin);
    let err = transition_issue(&config, "; rm -rf", "Done").unwrap_err();
    assert!(matches!(err, Error::BadRequest(_)));
}

#[cfg(unix)]
#[test]
fn verify_status_succeeds_when_the_viewed_status_maps_to_the_expected_category() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        r#"cat <<'HBEOF'
{"key": "PROJ-1", "fields": {"status": {"name": "Done"}}}
HBEOF
"#,
    );
    let config = config_with(bin);
    verify_status(&config, "PROJ-1", JiraCategory::Done).expect("Done maps to Done");
}

#[cfg(unix)]
#[test]
fn verify_status_fails_when_the_viewed_status_maps_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let bin = fixture_jira_bin(
        &dir,
        "jira-fixture",
        r#"cat <<'HBEOF'
{"key": "PROJ-1", "fields": {"status": {"name": "To Do"}}}
HBEOF
"#,
    );
    let config = config_with(bin);
    let err = verify_status(&config, "PROJ-1", JiraCategory::Done).unwrap_err();
    assert!(matches!(err, Error::JiraUnavailable(_)));
}
