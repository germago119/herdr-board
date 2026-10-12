//! `~/.config/herdr-board/config.toml` loader (override via `HERDR_BOARD_CONFIG`).
//! A missing file yields defaults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

fn default_max_concurrent() -> usize {
    3
}
fn default_idle_grace_seconds() -> u64 {
    90
}
fn default_timeout_unit_secs() -> u64 {
    60
}
fn default_local_poll_ms() -> u64 {
    2000
}
fn default_tick_ms() -> u64 {
    1000
}
fn default_jira_executable() -> String {
    "jira".to_string()
}
fn default_jira_project() -> String {
    String::new()
}
fn default_jira_jql() -> String {
    "assignee = currentUser() AND sprint in openSprints()".to_string()
}
fn default_jira_page_size() -> u32 {
    100
}
/// Jira status *name* (lowercased) → canonical column category
/// (`backlog` | `in_progress` | `done`). Any status not listed here falls
/// back to `backlog` in code, so this map only needs to list the non-Backlog
/// exceptions.
fn default_jira_status_map() -> HashMap<String, String> {
    HashMap::from([
        ("done".to_string(), "done".to_string()),
        ("in progress".to_string(), "in_progress".to_string()),
    ])
}
/// Canonical column category → the exact Jira transition *action* name to
/// invoke (`jira issue move KEY <action>`). Action names are workflow-specific
/// and case-insensitive but not guaranteed to equal the destination status
/// name (e.g. an action called "Start Progress" might land on status "In
/// Progress") — this config exists precisely so a site's real transition
/// names can be supplied without code changes.
fn default_jira_transitions() -> HashMap<String, String> {
    HashMap::from([
        ("backlog".to_string(), "Backlog".to_string()),
        ("in_progress".to_string(), "In Progress".to_string()),
        ("done".to_string(), "Done".to_string()),
    ])
}

/// The kind of process spawner used by the daemon.
///
/// This lives in `board-core` because it is part of the on-disk configuration,
/// rather than a daemon implementation detail. Keeping it typed means a bad
/// value in `config.toml` is reported instead of silently falling back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpawnerKind {
    /// Launch agents as herdr panes (the production default).
    #[default]
    Herdr,
    /// Launch agents as plain child processes.
    Local,
}

/// Settings in the root `[daemon]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub spawner: SpawnerKind,
    #[serde(default = "default_timeout_unit_secs")]
    pub timeout_unit_secs: u64,
    #[serde(default = "default_local_poll_ms")]
    pub local_poll_ms: u64,
    #[serde(default = "default_tick_ms")]
    pub tick_ms: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            spawner: SpawnerKind::default(),
            timeout_unit_secs: default_timeout_unit_secs(),
            local_poll_ms: default_local_poll_ms(),
            tick_ms: default_tick_ms(),
        }
    }
}

/// Board configuration (the top-level fields and `[harness.*]` tables).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Global cap on concurrent runs across all spaces.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    /// Seconds an agent may sit idle (no `board done`) before it is parked awaiting review.
    #[serde(default = "default_idle_grace_seconds")]
    pub idle_grace_seconds: u64,
    /// Config-defined harnesses keyed by name (`[harness.NAME]`).
    #[serde(default)]
    pub harness: HashMap<String, HarnessDef>,
    /// Pi agent dir to read the live model catalog from (`auth.json` +
    /// `models-store.json`). `None` disables live Pi model discovery → the
    /// `pi` harness reports the static free-form catalog (`models: []`). The
    /// daemon fills this in at startup; tests leave it `None` to stay hermetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi_agent_dir: Option<PathBuf>,
    /// Codex home to read the live model catalog from (`models_cache.json`).
    /// `None` disables live codex model discovery → the `codex` harness
    /// reports the static free-form catalog (`models: []`). The daemon fills
    /// this in at startup; tests leave it `None` to stay hermetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<PathBuf>,
    /// OpenCode binary to probe for the live model catalog (`opencode models
    /// --verbose`). `None` disables live opencode discovery → the `opencode`
    /// harness reports the static fallback catalog. The daemon fills this in
    /// at startup (`$OPENCODE_BIN` else `opencode`); tests leave it `None` to
    /// stay hermetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_bin: Option<String>,
    /// Antigravity binary to probe for the live model catalog (`agy
    /// --output-format json models`). `None` disables live antigravity
    /// discovery → the `antigravity` harness reports the free-form catalog
    /// (models `[]`, `model_freeform` true). The daemon fills this in at
    /// startup (`$AGY_BIN` else `agy`); tests leave it `None` to stay
    /// hermetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy_bin: Option<String>,
    /// The last-known live antigravity model catalog ([`crate::agy_catalog`]).
    /// **Daemon-filled, never user config**: the daemon probes `agy
    /// --output-format json models` and stamps the result here before
    /// serving `harness.capabilities` or validating an antigravity
    /// run/edit. `None` = catalog unavailable → the harness is free-form
    /// (stored models still run; the UIs have no models to offer). Tests set
    /// this directly to exercise catalog-up behavior hermetically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy_models: Option<Vec<crate::capability::ModelInfo>>,
}

/// A config-defined harness: an argv template plus an optional capability
/// declaration (consumed by [`crate::capability::capabilities_for`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDef {
    pub argv: Vec<String>,
    /// Known model aliases this harness accepts (advisory; model strings are
    /// treated as free-form regardless).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Reasoning-effort levels this harness accepts (parsed via
    /// [`Effort::parse_str`](crate::protocol::Effort::parse_str); applied to
    /// every listed model).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    /// Permission modes this harness accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_modes: Vec<String>,
    /// Opt-in: this harness can resume a previously recorded conversation.
    ///
    /// **Defaults to `false`** — herdr-board never assumes a user-defined
    /// harness understands resuming, because there is no universal syntax for
    /// it. Declaring `resume = true` promises that the harness re-attaches to
    /// the conversation named by the `BOARD_RESUME_SESSION_ID` environment
    /// variable, which is the only channel the rescue flow can use for a
    /// configured argv template (the run's argv is persisted fully
    /// materialized, so no placeholder is left to substitute). See
    /// [`crate::capability::ResumeSupport`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resume: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_concurrent: default_max_concurrent(),
            idle_grace_seconds: default_idle_grace_seconds(),
            harness: HashMap::new(),
            pi_agent_dir: None,
            codex_home: None,
            opencode_bin: None,
            agy_bin: None,
            agy_models: None,
        }
    }
}

impl Config {
    /// Parse a TOML document and return its board portion.
    ///
    /// The complete document is still validated, including `[daemon]`, in the
    /// same serde pass used by [`RootConfig::from_toml`]. This projection is
    /// retained for callers that only need board settings.
    pub fn from_toml(s: &str) -> Result<Config> {
        Ok(RootConfig::from_toml(s)?.board)
    }

    /// Load from `path`; a missing file returns defaults.
    pub fn load_from(path: &Path) -> Result<Config> {
        Ok(RootConfig::load_from(path)?.board)
    }

    /// Load from the resolved config path (env override then XDG).
    pub fn load() -> Result<Config> {
        Ok(RootConfig::load()?.board)
    }
}

/// The complete configuration document.
///
/// Board settings intentionally remain at the TOML document root for
/// backwards compatibility; daemon settings occupy `[daemon]`. The flatten is
/// important: it lets one serde pass validate board, harness, and daemon
/// values together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootConfig {
    #[serde(flatten)]
    pub board: Config,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub jira: JiraConfig,
}

impl RootConfig {
    /// Parse the complete configuration document from TOML.
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s).map_err(|e| Error::Config(e.to_string()))
    }

    /// Load the complete document from `path`; a missing file returns all
    /// defaults. Existing files are never treated as optional: malformed TOML
    /// and type errors are returned as [`Error::Config`].
    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml(&s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// Load from the resolved config path (env override then XDG).
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_path())
    }
}

/// Settings in the root `[jira]` table — a local-workflow adapter for the
/// existing authenticated `jira` CLI (v1.7.0 argv shapes are baked into
/// [`crate::jira`]; only project/query/naming are configurable here).
///
/// **Disabled by default** (`enabled = false`): every non-Jira behavior and
/// test is unaffected until a user opts in. When disabled, `jira.import`
/// returns [`Error::JiraUnavailable`] and `card.move` never contacts Jira
/// regardless of a card's external identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JiraConfig {
    #[serde(default)]
    pub enabled: bool,
    /// The `jira` executable name or path; never invoked through a shell.
    #[serde(default = "default_jira_executable")]
    pub executable: String,
    /// Jira project key (`-p PROJECT`), e.g. `PROJ`. No default is assumed;
    /// set this when enabling Jira, or pass `--jira-project` for import.
    #[serde(default = "default_jira_project")]
    pub project: String,
    /// `--jql` for `jira issue list`.
    #[serde(default = "default_jira_jql")]
    pub jql: String,
    /// `--paginate` page size; clamped to the CLI's documented max (100).
    #[serde(default = "default_jira_page_size")]
    pub page_size: u32,
    /// Local board name `jira import` creates or selects when the CLI does
    /// not name one explicitly. `None` = derive `"Jira: <project>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// Jira status *name* (lowercased) → canonical column category
    /// (`backlog` | `in_progress` | `done`); see [`default_jira_status_map`].
    #[serde(default = "default_jira_status_map")]
    pub status_map: HashMap<String, String>,
    /// Canonical column category → Jira transition action name; see
    /// [`default_jira_transitions`].
    #[serde(default = "default_jira_transitions")]
    pub transitions: HashMap<String, String>,
}

impl Default for JiraConfig {
    fn default() -> Self {
        JiraConfig {
            enabled: false,
            executable: default_jira_executable(),
            project: default_jira_project(),
            jql: default_jira_jql(),
            page_size: default_jira_page_size(),
            board: None,
            status_map: default_jira_status_map(),
            transitions: default_jira_transitions(),
        }
    }
}

/// Backwards-compatible name for callers that refer to the board section as a
/// board config.
pub type BoardConfig = Config;
