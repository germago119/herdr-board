//! Bounded, hermetic adapter over the existing authenticated `jira` CLI
//! (verified against jira-cli v1.7.0).
//!
//! herdr-board never reads or copies Jira credentials: it always shells out
//! to the user's own already-authenticated `jira` binary, exactly like
//! [`crate::agy_catalog`] shells out to `agy`. Every invocation is an argv
//! array (never a shell string), `stdin` is `/dev/null`, and both `stdout`
//! and `stderr` are captured through a bounded reader thread each, with a
//! wall-clock deadline — a hung or pathological `jira` cannot block the
//! daemon forever or exhaust memory.
//!
//! Field-verified CLI facts pinned here (do not drift without re-verifying
//! against a real `jira` binary):
//! - listing: `jira -p PROJECT issue list --jql 'JQL' --raw --paginate
//!   OFFSET:LIMIT`; `--raw` stdout is a JSON array of
//!   `{key, fields:{summary, description, status:{name}}}`. `description` is
//!   nullable and may be a plain string (Jira Server) or an Atlassian
//!   Document Format object (Jira Cloud) — [`adf_to_text`] best-effort
//!   flattens the latter; this shape is deliberately parsed defensively.
//! - **a valid empty result exits 1** with a stderr message containing
//!   `"No result found for given query in project"` — this exact case is the
//!   only nonzero exit classified as "no issues"; every other nonzero exit
//!   (auth failure, bad JQL, network error, ...) is a real error.
//! - `--paginate` accepts at most 100 issues per page; callers page by
//!   incrementing the offset until a short page ends the list.
//! - transitioning: `jira -p PROJECT issue move KEY STATE`, where `STATE` is
//!   a workflow **transition action name** (case-insensitive), not
//!   necessarily equal to the destination status name.
//! - verifying: `jira -p PROJECT issue view KEY --raw` returns the full issue
//!   JSON (`.key`, `.self`, `.fields.summary`, `.fields.description`,
//!   `.fields.status.name`).

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::JiraConfig;
use crate::{Error, Result};

/// Wall-clock budget for one `jira` invocation. Sits on request paths (card
/// move, `jira import`), so a hung CLI must not block the daemon forever.
pub const CLI_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on stdout/stderr bytes retained from one CLI run.
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// `--paginate`'s documented maximum page size.
pub const MAX_PAGE_SIZE: u32 = 100;
/// Hard cap on total issues fetched across pages for one import, so a
/// misconfigured JQL cannot page forever.
pub const MAX_ISSUES: usize = 10_000;
/// The exact substring jira-cli v1.7.0 prints on a valid empty result
/// (exit 1). Matched verbatim, never by full-message equality, so trailing
/// project-name text does not break the check.
const EMPTY_RESULT_MARKER: &str = "No result found for given query in project";

/// The three canonical column categories the local workflow maps Jira issues
/// onto. Deliberately just three — see `docs/design.md`'s Jira section for
/// why this stays unopinionated about a site's real workflow shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JiraCategory {
    Backlog,
    InProgress,
    Done,
}

impl JiraCategory {
    /// The exact local column name `jira import` creates/matches
    /// case-insensitively.
    pub fn column_name(self) -> &'static str {
        match self {
            JiraCategory::Backlog => "Backlog",
            JiraCategory::InProgress => "In Progress",
            JiraCategory::Done => "Done",
        }
    }

    /// The key this category is addressed by in `JiraConfig.transitions`.
    pub fn config_key(self) -> &'static str {
        match self {
            JiraCategory::Backlog => "backlog",
            JiraCategory::InProgress => "in_progress",
            JiraCategory::Done => "done",
        }
    }
}

/// Match a local column's own name against the three canonical categories,
/// case-insensitively and trimmed. `None` means the column is not one of the
/// three Jira-tracked columns (an ordinary local column) — the caller should
/// treat that as "nothing to sync", not an error.
pub fn category_for_column_name(name: &str) -> Option<JiraCategory> {
    match name.trim().to_lowercase().as_str() {
        "backlog" => Some(JiraCategory::Backlog),
        "in progress" => Some(JiraCategory::InProgress),
        "done" => Some(JiraCategory::Done),
        _ => None,
    }
}

/// Map a Jira status *name* onto a canonical category via `status_map`
/// (lowercased lookup). Anything absent from the map — including every
/// status the default map does not name — falls back to
/// [`JiraCategory::Backlog`], matching the default mapping behavior: Done ->
/// Done, In Progress -> In Progress, and all other statuses -> Backlog.
pub fn category_for_status(
    status_map: &HashMap<String, String>,
    status_name: &str,
) -> JiraCategory {
    let key = status_name.trim().to_lowercase();
    match status_map.get(&key).map(String::as_str) {
        Some("done") => JiraCategory::Done,
        Some("in_progress") => JiraCategory::InProgress,
        Some("backlog") => JiraCategory::Backlog,
        _ => JiraCategory::Backlog,
    }
}

/// The transition action name configured for `category`, or a
/// [`Error::JiraUnavailable`] fail-closed error when the site's `[jira]`
/// config does not declare one — an ambiguous mapping must never silently
/// let the local board and Jira drift apart.
pub fn transition_for_category(config: &JiraConfig, category: JiraCategory) -> Result<String> {
    config
        .transitions
        .get(category.config_key())
        .cloned()
        .ok_or_else(|| {
            Error::JiraUnavailable(format!(
                "no Jira transition configured for column category {:?}; set \
                 [jira.transitions] {} = \"<action name>\"",
                category,
                category.config_key()
            ))
        })
}

/// Require a non-empty `[jira].project` before invoking jira-cli. Import can
/// supply a CLI override, so its error message names that escape hatch.
pub fn require_project(config: &JiraConfig, import_override_available: bool) -> Result<()> {
    if !config.project.trim().is_empty() {
        return Ok(());
    }
    let hint = if import_override_available {
        "[jira].project must be set (or pass --jira-project)"
    } else {
        "[jira].project must be set"
    };
    Err(Error::Config(hint.to_string()))
}

/// One issue as returned by `issue list --raw` / `issue view --raw`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JiraIssue {
    pub key: String,
    pub summary: String,
    pub description: String,
    pub status_name: String,
}

/// Best-effort flatten of a Jira `description` field: `null` → empty string,
/// a plain string → itself, an Atlassian Document Format object → its text
/// nodes joined with paragraph breaks. Any other/unrecognized shape yields an
/// empty string rather than an error — a description is display-only.
pub fn adf_to_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Object(_) => {
            let mut out = String::new();
            collect_adf_text(v, &mut out);
            out.trim().to_string()
        }
        _ => String::new(),
    }
}

fn collect_adf_text(v: &Value, out: &mut String) {
    if v.get("type").and_then(Value::as_str) == Some("text") {
        if let Some(text) = v.get("text").and_then(Value::as_str) {
            out.push_str(text);
        }
    }
    if let Some(content) = v.get("content").and_then(Value::as_array) {
        for child in content {
            collect_adf_text(child, out);
        }
        // Block-level nodes (paragraph/heading/...) get a break after their
        // children so flattened text stays readable instead of one run-on.
        out.push('\n');
    }
}

/// Strip ANSI escape (CSI) sequences from CLI output before it reaches an
/// error message or the TUI. Hand-rolled (no new crate dependency): scans for
/// `ESC '[' ... final-byte` and drops it; a lone/unrecognized `ESC` is
/// dropped on its own rather than emitted raw. Sequences other than CSI
/// (e.g. OSC) are not expected from `jira`'s plain-text errors and are not
/// specially handled.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if ('@'..='~').contains(&c2) {
                    break;
                }
            }
        }
        // else: a bare/unrecognized escape byte; drop it and continue.
    }
    out
}

/// argv for `issue list --jql JQL --raw --paginate OFFSET:LIMIT`.
pub fn list_argv(config: &JiraConfig, offset: u32, limit: u32) -> Vec<String> {
    vec![
        config.executable.clone(),
        "-p".to_string(),
        config.project.clone(),
        "issue".to_string(),
        "list".to_string(),
        "--jql".to_string(),
        config.jql.clone(),
        "--raw".to_string(),
        "--paginate".to_string(),
        format!("{offset}:{limit}"),
    ]
}

/// argv for `issue move KEY STATE`.
pub fn move_argv(config: &JiraConfig, key: &str, state: &str) -> Vec<String> {
    vec![
        config.executable.clone(),
        "-p".to_string(),
        config.project.clone(),
        "issue".to_string(),
        "move".to_string(),
        key.to_string(),
        state.to_string(),
    ]
}

/// argv for `issue view KEY --raw`.
pub fn view_argv(config: &JiraConfig, key: &str) -> Vec<String> {
    vec![
        config.executable.clone(),
        "-p".to_string(),
        config.project.clone(),
        "issue".to_string(),
        "view".to_string(),
        key.to_string(),
        "--raw".to_string(),
    ]
}

/// A completed CLI run: exit code (`None` only when the process could not be
/// reaped and was killed, treated the same as a timeout by callers) plus
/// captured, UTF-8-lossy-decoded stdout/stderr.
struct CliRun {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Spawn a reader thread that drains `pipe` into a byte buffer capped at
/// [`MAX_OUTPUT_BYTES`], signaling `None` on overflow or a read error.
fn spawn_capped_reader(
    mut pipe: impl Read + Send + 'static,
) -> (std::thread::JoinHandle<()>, mpsc::Receiver<Option<Vec<u8>>>) {
    let (tx, rx) = mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0_u8; 8 * 1024];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buffer.len() + n > MAX_OUTPUT_BYTES {
                        let _ = tx.send(None);
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                }
                Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
        let _ = tx.send(Some(buffer));
    });
    (handle, rx)
}

const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Run `argv[0] argv[1..]` with `stdin` null, both `stdout`/`stderr`
/// captured, bounded by `timeout`. Never goes through a shell. Returns
/// [`Error::JiraUnavailable`] for every failure that has no meaningful exit
/// code to classify further (spawn failure, timeout, oversized output, lost
/// reader) — the caller supplies the CLI name in its own message via the
/// argv it built, so no extra context is added here beyond the cause.
fn run(argv: &[String], timeout: Duration) -> Result<CliRun> {
    let Some((program, args)) = argv.split_first() else {
        return Err(Error::JiraUnavailable("empty jira command".into()));
    };
    // A fresh executable can transiently answer `ETXTBSY` on some
    // filesystems/schedulers if the kernel has not yet released a very
    // recent writer's mapping of the same inode (observed with hermetic
    // fixture binaries under heavy parallel test load) — bounded retries
    // absorb that window without masking a real, persistent spawn failure.
    let mut attempt: u64 = 0;
    let mut child = loop {
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(26) && attempt < 5 => {
                // ETXTBSY; retry after a brief backoff.
                attempt += 1;
                std::thread::sleep(Duration::from_millis(10 * attempt));
                continue;
            }
            Err(e) => {
                return Err(Error::JiraUnavailable(format!(
                    "failed to run {program}: {e}"
                )))
            }
        }
    };
    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| Error::JiraUnavailable("no stdout pipe".into()))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| Error::JiraUnavailable("no stderr pipe".into()))?;
    let (out_handle, out_rx) = spawn_capped_reader(stdout_pipe);
    let (err_handle, err_rx) = spawn_capped_reader(stderr_pipe);
    let deadline = Instant::now() + timeout;

    let kill_and_fail = |mut child: std::process::Child,
                         out_handle: std::thread::JoinHandle<()>,
                         err_handle: std::thread::JoinHandle<()>,
                         out_rx: mpsc::Receiver<Option<Vec<u8>>>,
                         err_rx: mpsc::Receiver<Option<Vec<u8>>>,
                         reason: &str|
     -> Error {
        let _ = child.kill();
        let _ = child.wait();
        drop(out_rx);
        drop(err_rx);
        let _ = out_handle.join();
        let _ = err_handle.join();
        Error::JiraUnavailable(format!("{program}: {reason}"))
    };

    let stdout_bytes = match out_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Err(kill_and_fail(
                child,
                out_handle,
                err_handle,
                out_rx,
                err_rx,
                "stdout exceeded the output cap or failed to read",
            ))
        }
        Err(_) => {
            return Err(kill_and_fail(
                child,
                out_handle,
                err_handle,
                out_rx,
                err_rx,
                "timed out waiting for stdout",
            ))
        }
    };
    let stderr_bytes = match err_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            return Err(kill_and_fail(
                child,
                out_handle,
                err_handle,
                out_rx,
                err_rx,
                "stderr exceeded the output cap or failed to read",
            ))
        }
        Err(_) => {
            return Err(kill_and_fail(
                child,
                out_handle,
                err_handle,
                out_rx,
                err_rx,
                "timed out waiting for stderr",
            ))
        }
    };

    // Both pipes reached EOF; reap the process within the remaining budget.
    let status = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => std::thread::sleep(remaining.min(POLL_INTERVAL)),
            Err(_) => break None,
        }
    };
    let _ = out_handle.join();
    let _ = err_handle.join();
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::JiraUnavailable(format!(
                "{program}: timed out waiting for exit"
            )));
        }
    };
    Ok(CliRun {
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
    })
}

/// Whether a nonzero-exit CLI run is jira-cli's documented "valid empty
/// result" case (exit 1 + [`EMPTY_RESULT_MARKER`] in either stream) rather
/// than a real failure. Checked against both streams because the message's
/// exact stream is not pinned by the CLI's own contract.
fn is_empty_result(run: &CliRun) -> bool {
    run.exit_code == Some(1)
        && (run.stdout.contains(EMPTY_RESULT_MARKER) || run.stderr.contains(EMPTY_RESULT_MARKER))
}

/// Build an ANSI-stripped error from a failed run's streams, favoring
/// stderr (where CLIs conventionally put diagnostics) and falling back to
/// stdout when stderr is empty.
fn cli_error(program: &str, run: &CliRun) -> Error {
    let raw = if run.stderr.trim().is_empty() {
        &run.stdout
    } else {
        &run.stderr
    };
    let message = strip_ansi(raw.trim());
    let code = run
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    Error::JiraUnavailable(format!("{program} exited {code}: {message}"))
}

/// Parse one page of `issue list --raw` JSON into typed issues. Malformed
/// JSON is a real error (a CLI contract violation); a well-formed array with
/// entries missing required fields skips just those entries.
fn parse_issues(stdout: &str) -> Result<Vec<JiraIssue>> {
    let root: Value = serde_json::from_str(stdout)
        .map_err(|e| Error::JiraUnavailable(format!("jira issue list: malformed JSON: {e}")))?;
    let entries = root
        .as_array()
        .ok_or_else(|| Error::JiraUnavailable("jira issue list: expected a JSON array".into()))?;
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            continue;
        };
        let fields = entry.get("fields");
        let summary = fields
            .and_then(|f| f.get("summary"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = fields
            .and_then(|f| f.get("description"))
            .map(adf_to_text)
            .unwrap_or_default();
        let status_name = fields
            .and_then(|f| f.get("status"))
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        out.push(JiraIssue {
            key: key.to_string(),
            summary,
            description,
            status_name,
        });
    }
    Ok(out)
}

/// Fetch every issue matching `config.jql`, paginating in pages of at most
/// [`MAX_PAGE_SIZE`] until a short page ends the list (or [`MAX_ISSUES`] is
/// hit, as a hard safety cap against a runaway JQL). jira-cli's own "no
/// result" case (exit 1 + the exact empty-result marker) is treated as zero
/// issues, never an error; every other nonzero exit is
/// [`Error::JiraUnavailable`].
pub fn fetch_all_issues(config: &JiraConfig) -> Result<Vec<JiraIssue>> {
    require_project(config, false)?;
    let page_size = config.page_size.clamp(1, MAX_PAGE_SIZE);
    let mut all = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let argv = list_argv(config, offset, page_size);
        let run = run(&argv, CLI_TIMEOUT)?;
        if run.exit_code == Some(0) {
            let mut page = parse_issues(&run.stdout)?;
            let got = page.len();
            all.append(&mut page);
            if got < page_size as usize || all.len() >= MAX_ISSUES {
                break;
            }
            offset += page_size;
        } else if is_empty_result(&run) {
            break;
        } else {
            return Err(cli_error(&config.executable, &run));
        }
    }
    all.truncate(MAX_ISSUES);
    Ok(all)
}

/// Invoke `jira -p PROJECT issue move KEY STATE`. Success is exit 0; any
/// nonzero exit is [`Error::JiraUnavailable`] with an ANSI-safe message.
pub fn transition_issue(config: &JiraConfig, key: &str, state: &str) -> Result<()> {
    require_project(config, false)?;
    require_valid_key(key)?;
    let argv = move_argv(config, key, state);
    let run = run(&argv, CLI_TIMEOUT)?;
    if run.exit_code == Some(0) {
        Ok(())
    } else {
        Err(cli_error(&config.executable, &run))
    }
}

/// `jira -p PROJECT issue view KEY --raw`, requiring the resulting
/// `.fields.status.name` to map (via `status_map`) onto `expected`. Used to
/// verify a transition actually landed on the intended category, since
/// transition *action* names are not guaranteed to equal the destination
/// *status* name.
pub fn verify_status(config: &JiraConfig, key: &str, expected: JiraCategory) -> Result<()> {
    require_project(config, false)?;
    require_valid_key(key)?;
    let argv = view_argv(config, key);
    let run = run(&argv, CLI_TIMEOUT)?;
    if run.exit_code != Some(0) {
        return Err(cli_error(&config.executable, &run));
    }
    let root: Value = serde_json::from_str(&run.stdout).map_err(|e| {
        Error::JiraUnavailable(format!("jira issue view {key}: malformed JSON: {e}"))
    })?;
    let status_name = root
        .get("fields")
        .and_then(|f| f.get("status"))
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Error::JiraUnavailable(format!("jira issue view {key}: missing fields.status.name"))
        })?;
    let actual = category_for_status(&config.status_map, status_name);
    if actual == expected {
        Ok(())
    } else {
        Err(Error::JiraUnavailable(format!(
            "jira issue {key} transitioned but status is now {status_name:?} \
             (category {actual:?}), expected {expected:?}"
        )))
    }
}

/// Reject characters that have no business in a Jira issue key/project,
/// closing off any risk of argv confusion (e.g. a value starting with `-`
/// being parsed as a flag by the `jira` CLI itself). Keys are
/// `PROJECT-123`-shaped; this is deliberately generous (alnum, `-`, `_`)
/// rather than a strict regex, since project key conventions vary.
fn require_valid_key(key: &str) -> Result<()> {
    let valid = !key.is_empty()
        && !key.starts_with('-')
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "invalid Jira issue key: {key:?}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_sequences_only() {
        assert_eq!(strip_ansi("plain text"), "plain text");
        assert_eq!(strip_ansi("\x1b[31merror\x1b[0m"), "error");
        assert_eq!(strip_ansi("no\x1bclose"), "noclose");
    }

    #[test]
    fn adf_to_text_handles_null_string_and_object() {
        assert_eq!(adf_to_text(&Value::Null), "");
        assert_eq!(adf_to_text(&Value::String("hi".into())), "hi");
        let adf = serde_json::json!({
            "type": "doc",
            "content": [{
                "type": "paragraph",
                "content": [{"type": "text", "text": "hello"}, {"type": "text", "text": " world"}]
            }]
        });
        assert_eq!(adf_to_text(&adf), "hello world");
    }

    #[test]
    fn category_for_status_defaults_to_backlog() {
        let map = crate::config::JiraConfig::default().status_map;
        assert_eq!(category_for_status(&map, "Done"), JiraCategory::Done);
        assert_eq!(
            category_for_status(&map, "In Progress"),
            JiraCategory::InProgress
        );
        assert_eq!(category_for_status(&map, "To Do"), JiraCategory::Backlog);
        assert_eq!(
            category_for_status(&map, "Anything Else"),
            JiraCategory::Backlog
        );
    }

    #[test]
    fn category_for_column_name_matches_case_insensitively() {
        assert_eq!(
            category_for_column_name("backlog"),
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
    fn require_valid_key_rejects_flag_like_values() {
        assert!(require_valid_key("PROJ-123").is_ok());
        assert!(require_valid_key("--help").is_err());
        assert!(require_valid_key("").is_err());
    }

    #[test]
    fn list_argv_matches_the_field_verified_shape() {
        let config = JiraConfig {
            executable: "jira".into(),
            project: "PROJ".into(),
            jql: "assignee = currentUser() AND sprint in openSprints()".into(),
            ..JiraConfig::default()
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
    fn move_argv_and_view_argv_match_the_field_verified_shape() {
        let config = JiraConfig {
            executable: "jira".into(),
            project: "PROJ".into(),
            ..JiraConfig::default()
        };
        assert_eq!(
            move_argv(&config, "PROJ-1", "Done"),
            vec!["jira", "-p", "PROJ", "issue", "move", "PROJ-1", "Done"]
        );
        assert_eq!(
            view_argv(&config, "PROJ-1"),
            vec!["jira", "-p", "PROJ", "issue", "view", "PROJ-1", "--raw"]
        );
    }
}
