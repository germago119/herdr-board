//! `board jira ...` — Jira import subcommands. Business logic (project/board
//! resolution, column mapping, card create/update) lives daemon-side in
//! `board-daemon::ops::jira`; this module only declares clap surface.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum JiraCmd {
    /// Import the current sprint's Jira issues into a local board.
    ///
    /// Creates or selects a board (`--board-name`, else `[jira].board`, else
    /// `"Jira: <project>"`) with three manual columns (Backlog, In Progress,
    /// Done — created if missing, matched case-insensitively if already
    /// present), then creates or updates one card per issue the JQL returns.
    /// Never dispatches an agent. Requires `[jira] enabled = true` in
    /// config.toml.
    Import {
        /// Jira project key; defaults to required `[jira].project`.
        #[arg(long, value_name = "KEY")]
        jira_project: Option<String>,
        /// JQL query; defaults to `[jira].jql`.
        #[arg(long, value_name = "JQL")]
        jql: Option<String>,
        /// Local board name to create or select; defaults to `[jira].board`,
        /// else `"Jira: <project>"`.
        #[arg(long, value_name = "NAME")]
        board_name: Option<String>,
    },
}
