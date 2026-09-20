//! User-registered worktree columns (SPEC §10.3, issue #34).
//!
//! `grove.column("pr", fn)` adds a column to the WORKTREES pane. This is how
//! forge data reaches grove **without grove knowing what a forge is**:
//! principle 1 — show only what git can answer — survives because the user
//! opted in and the column is visibly theirs, which is the reason built-in
//! GitHub integration could stay cut.
//!
//! Two rules keep a user's function out of the render path.
//!
//! **Values are computed when the rows change, never while drawing.** A
//! callback shelling out to `gh` takes as long as the network does, and a
//! frame that waits for it is a pane that stops responding to arrow keys.
//! The invalidation point is the arrival of a new worktree list — the same
//! moment the rows themselves change.
//!
//! **A column that fails is disabled for the session; one that times out is
//! left empty.** §10.5 asks for the error to surface once rather than on every
//! row and every frame, and the two failures mean different things: a throw is
//! deterministic, a timeout may be a slow network.

use std::collections::HashMap;
use std::time::Duration;

use grove_lua::{CallError, TuiRuntime};

/// How long one cell may take. Shorter than a keymap's: this runs once per
/// row, so the wait a user feels is this multiplied by the list.
const BUDGET: Duration = Duration::from_millis(300);

/// One user column's heading and its computed cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// Cell text by `repo\0branch`, empty where the callback had nothing to
    /// say or ran out of time.
    cells: HashMap<String, String>,
}

impl Column {
    pub fn cell(&self, repo: &str, branch: &str) -> &str {
        self.cells
            .get(&key(repo, branch))
            .map_or("", String::as_str)
    }

    /// The widest cell, for negotiating width with the built-in columns.
    pub fn width(&self) -> usize {
        self.cells
            .values()
            .map(|value| value.chars().count())
            .chain(std::iter::once(self.name.chars().count()))
            .max()
            .unwrap_or(0)
    }
}

/// Every user column, and which have disabled themselves.
#[derive(Debug, Default)]
pub struct Columns {
    columns: Vec<Column>,
    disabled: Vec<usize>,
    /// Said once, not once per row.
    notes: Vec<String>,
}

impl Columns {
    /// Recompute every live column for these worktrees.
    ///
    /// Called when the list changes, which is the defined invalidation point:
    /// the cells describe those rows and nothing else.
    pub fn refresh(&mut self, runtime: &TuiRuntime, worktrees: &[(String, String)]) {
        self.notes.clear();
        let registrations = &runtime.registrations().columns;
        self.columns = registrations
            .iter()
            .map(|registration| Column {
                name: registration.name.clone(),
                cells: HashMap::new(),
            })
            .collect();

        for (index, column) in self.columns.iter_mut().enumerate() {
            if self.disabled.contains(&index) {
                continue;
            }
            for (repo, branch) in worktrees {
                match runtime.call_column(index, repo, branch, BUDGET) {
                    Ok(value) => {
                        column.cells.insert(key(repo, branch), value);
                    }
                    Err(CallError::Timeout) => {
                        // The cell stays empty. Reported once for the column
                        // rather than once per row, and not disabled: slow is
                        // not the same as broken.
                        if !self.notes.iter().any(|note| note.contains(&column.name)) {
                            self.notes
                                .push(format!("column {:?} timed out", column.name));
                        }
                    }
                    Err(CallError::Failed(why)) => {
                        self.disabled.push(index);
                        self.notes
                            .push(format!("column {:?} disabled: {why}", column.name));
                        break;
                    }
                }
            }
        }
    }

    /// The columns worth drawing.
    pub fn live(&self) -> Vec<&Column> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.disabled.contains(index))
            .map(|(_, column)| column)
            .collect()
    }

    /// What to say about them, once.
    pub fn take_note(&mut self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.notes).join("; "))
    }
}

fn key(repo: &str, branch: &str) -> String {
    format!("{repo}\0{branch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(source: &str) -> TuiRuntime {
        TuiRuntime::load_source(source, "columns").runtime
    }

    fn rows() -> Vec<(String, String)> {
        vec![
            ("billing".to_string(), "feat/x".to_string()),
            ("web".to_string(), "fix/y".to_string()),
        ]
    }

    #[test]
    fn a_column_is_computed_once_per_row_and_read_from_the_cache() {
        // Acceptance: not called every frame. The cells are computed when the
        // rows arrive, and drawing only reads them.
        let mut columns = Columns::default();
        columns.refresh(
            &runtime(
                "local grove = require('grove')\n\
                 grove.column('pr', function(wt) return wt.branch end)\n",
            ),
            &rows(),
        );
        let live = columns.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name, "pr");
        assert_eq!(live[0].cell("billing", "feat/x"), "feat/x");
        assert_eq!(live[0].cell("web", "fix/y"), "fix/y");
        // A row the column knows nothing about is empty, not missing.
        assert_eq!(live[0].cell("other", "none"), "");
    }

    #[test]
    fn a_column_that_times_out_leaves_the_cell_empty_and_says_so_once() {
        // §10.5, and the acceptance: the UI stays responsive and the cell is
        // empty. Said once for the column rather than once per row — two rows
        // timing out is one problem, not two.
        let mut columns = Columns::default();
        let started = std::time::Instant::now();
        columns.refresh(
            &runtime(
                "local grove = require('grove')\n\
                 grove.column('slow', function() while true do end end)\n",
            ),
            &rows(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(
            columns.live().len(),
            1,
            "still offered — slow is not broken"
        );
        assert_eq!(columns.live()[0].cell("billing", "feat/x"), "");
        let note = columns.take_note().expect("a report");
        assert_eq!(note.matches("timed out").count(), 1, "{note}");
        assert!(columns.take_note().is_none(), "and only once");
    }

    #[test]
    fn a_column_that_throws_is_disabled_for_the_session() {
        let mut columns = Columns::default();
        let lua = runtime(
            "local grove = require('grove')\n\
             grove.column('bad', function() error('no gh') end)\n",
        );
        columns.refresh(&lua, &rows());
        assert!(columns.live().is_empty(), "it is gone");
        let note = columns.take_note().expect("a report");
        assert!(note.contains("no gh"), "{note}");

        // And it stays gone when the rows change again.
        columns.refresh(&lua, &rows());
        assert!(columns.live().is_empty());
    }

    #[test]
    fn a_columns_width_covers_its_heading_and_its_widest_cell() {
        // So the pane can negotiate: a column narrower than its own heading
        // would print a truncated title over full-width values.
        let mut columns = Columns::default();
        columns.refresh(
            &runtime(
                "local grove = require('grove')\n\
                 grove.column('pr', function() return 'merged' end)\n",
            ),
            &rows(),
        );
        assert_eq!(columns.live()[0].width(), "merged".len());

        let mut wide = Columns::default();
        wide.refresh(
            &runtime(
                "local grove = require('grove')\n\
                 grove.column('a-long-heading', function() return 'x' end)\n",
            ),
            &rows(),
        );
        assert_eq!(wide.live()[0].width(), "a-long-heading".len());
    }

    #[test]
    fn no_columns_registered_is_no_columns_drawn() {
        let mut columns = Columns::default();
        columns.refresh(&runtime("local grove = require('grove')\n"), &rows());
        assert!(columns.live().is_empty());
        assert!(columns.take_note().is_none());
    }
}
