//! `jira.import`: create-or-select a local board, create-or-match its three
//! canonical columns (Backlog / In Progress / Done), and create-or-update
//! cards for every issue the configured JQL returns.
//!
//! This handler deliberately calls `Db` create/update/move methods directly
//! instead of going through `ops::cards`' `card_create`/`card_move` (which
//! decide whether to enqueue a run on an auto column): an import must never
//! dispatch an agent merely because it landed a card on a trigger-configured
//! column. `d.wake_dispatch()` is never called here.
//!
//! The `jira` CLI subprocess work ([`board_core::jira::fetch_all_issues`]) is
//! deliberately done with no store lock held — a page fetch can take up to
//! [`board_core::jira::CLI_TIMEOUT`] and must not block every other daemon
//! request meanwhile. The store is locked only for the bounded, fast DB work.

use std::collections::HashMap;

use super::*;
use board_core::db::BOARD_ID;
use board_core::jira::{self, JiraCategory};

const JIRA_CATEGORIES: [JiraCategory; 3] = [
    JiraCategory::Backlog,
    JiraCategory::InProgress,
    JiraCategory::Done,
];

pub(super) fn jira_import(d: &Arc<Daemon>, p: JiraImportParams) -> Result<Value> {
    if !d.jira.enabled {
        return Err(Error::JiraUnavailable(
            "Jira is disabled; set `[jira] enabled = true` (and `project`/`jql` as needed) \
             in config.toml"
                .into(),
        ));
    }
    let mut effective = d.jira.clone();
    if let Some(project) = &p.project {
        effective.project = project.clone();
    }
    if let Some(jql) = &p.jql {
        effective.jql = jql.clone();
    }
    jira::require_project(&effective, true)?;
    let board_name = p
        .board
        .clone()
        .or_else(|| effective.board.clone())
        .unwrap_or_else(|| format!("Jira: {}", effective.project));

    // Subprocess I/O first, with no store lock held.
    let issues = jira::fetch_all_issues(&effective)?;

    let db = d.store.lock();
    let project_id = db.selected_project_id()?.unwrap_or(BOARD_ID);
    let board = match db.resolve_board(project_id, &board_name) {
        Ok(b) => b,
        Err(Error::NotFound(_)) => db.create_board(project_id, &board_name)?,
        Err(e) => return Err(e),
    };
    if board.archived_at.is_some() {
        return Err(Error::InvalidState(format!(
            "archived board must be restored first: `board board restore {}`",
            board.id
        )));
    }

    // Create-or-match (case-insensitively) the three canonical columns.
    let existing_columns = db.list_columns(board.id)?;
    let mut column_for: HashMap<JiraCategory, i64> = HashMap::new();
    for category in JIRA_CATEGORIES {
        let column_id = match existing_columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(category.column_name()))
        {
            Some(c) => c.id,
            None => {
                db.create_column(&ColumnCreateParams {
                    name: category.column_name().to_string(),
                    board_id: Some(board.id),
                    trigger: Some(Trigger::Manual),
                    ..Default::default()
                })?
                .id
            }
        };
        column_for.insert(category, column_id);
    }

    let mut created = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    for issue in &issues {
        let category = jira::category_for_status(&effective.status_map, &issue.status_name);
        // Every category was just ensured above, so this is always present.
        let column_id = column_for[&category];
        match db.find_card_by_external(board.id, "jira", &issue.key)? {
            None => {
                db.create_card(&CardCreateParams {
                    title: issue.summary.clone(),
                    description: Some(issue.description.clone()),
                    board_id: Some(board.id),
                    column_id: Some(column_id),
                    external_source: Some("jira".to_string()),
                    external_key: Some(issue.key.clone()),
                    ..Default::default()
                })?;
                created += 1;
            }
            Some(existing) => {
                let mut changed = false;
                if existing.title != issue.summary || existing.description != issue.description {
                    db.update_card(&CardUpdateParams {
                        id: existing.id,
                        title: Some(issue.summary.clone()),
                        description: Some(issue.description.clone()),
                        ..Default::default()
                    })?;
                    changed = true;
                }
                if existing.column_id != column_id {
                    db.move_card(existing.id, column_id, None)?;
                    changed = true;
                }
                if changed {
                    updated += 1;
                } else {
                    unchanged += 1;
                }
            }
        }
    }
    drop(db);

    // One coarse refresh signal for the whole batch, mirroring
    // `template_apply`'s post-column-creation emit — there is no single
    // card/column id to name when many cards may have changed.
    d.emit_changed_board(BoardChangedReason::ColumnChanged, board.id, None, None);

    Ok(json!(JiraImportResult {
        board_id: board.id,
        board_name: board.name,
        project: effective.project,
        fetched: issues.len(),
        created,
        updated,
        unchanged,
    }))
}
