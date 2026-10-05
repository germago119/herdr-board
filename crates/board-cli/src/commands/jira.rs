use anyhow::Result;
use board_core::client::BoardClient;
use board_core::protocol::JiraImportParams;

use crate::args::JiraCmd;
use crate::context::Ctx;
use crate::render::emit_line;

pub(crate) fn cmd_jira(sub: JiraCmd, ctx: &mut Ctx) -> Result<()> {
    let json = ctx.json();
    match sub {
        JiraCmd::Import {
            jira_project,
            jql,
            board_name,
        } => {
            // Apply the CLI's global --project/--board scope selectors the
            // same way every other command does, so `jira.import`'s
            // create-or-select-a-board-by-name runs inside the right
            // project. The resolved board id itself is not the target board
            // (jira.import names its own board by `--board-name`), so it is
            // discarded — only the selection side effect matters here.
            ctx.board_id()?;
            let params = JiraImportParams {
                project: jira_project,
                jql,
                board: board_name,
            };
            let result = ctx.client()?.jira_import(&params)?;
            let text = format!(
                "Imported {} Jira issue(s) from project {} into board #{} {:?}: \
                 {} created, {} updated, {} unchanged",
                result.fetched,
                result.project,
                result.board_id,
                result.board_name,
                result.created,
                result.updated,
                result.unchanged,
            );
            emit_line(&result, json, text)
        }
    }
}
