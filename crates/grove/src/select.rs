//! The repo multi-select the palette turns into (SPEC §4.2).
//!
//! `new feat/x` stops being a text field and becomes a list of repositories
//! with the session's members already checked. Non-members are in the same
//! list rather than behind another screen, because "this task also touches
//! api-gateway" is a thing you discover while typing the branch name, not
//! before — and checking one is what makes it a member.
//!
//! The same list serves `add` and `remove`. They differ only in which repos
//! are worth showing and what is checked to begin with, which is a filter and
//! a default rather than three screens.

use grove_domain::RepoId;
use grove_proto::RepoRow;

/// One repository as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub repo: RepoId,
    pub name: String,
    /// Already a member of the open session.
    pub member: bool,
    pub checked: bool,
    /// The branch `new` would cut from, as the daemon reports it; empty for
    /// a repo that has none (no `origin/HEAD`, no configured base).
    pub base: String,
    /// Whether that came from `origin/HEAD` rather than configuration.
    pub from_origin_head: bool,
}

/// Which repositories a command wants to see, and which start checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wants {
    /// Every repository, members checked. `new` creates worktrees in the ones
    /// you pick, and picking a non-member adds it to the session.
    AllReposMembersChecked,
    /// Only repositories the session does not hold — there is nothing to add
    /// about one it already has.
    NonMembers,
    /// Only members. Removing what is not there is not an operation.
    Members,
}

/// A list of repositories with checkboxes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Select {
    rows: Vec<Row>,
    cursor: usize,
}

impl Select {
    /// Build the list a command wants from what the daemon last sent.
    pub fn build(repos: &[RepoRow], wants: Wants) -> Self {
        let rows = repos
            .iter()
            .filter(|row| match wants {
                Wants::AllReposMembersChecked => true,
                Wants::NonMembers => !row.member,
                Wants::Members => row.member,
            })
            .map(|row| Row {
                repo: row.repo.clone(),
                name: row.name.clone(),
                member: row.member,
                base: row.base_branch.clone(),
                from_origin_head: row.base_from_origin_head,
                checked: match wants {
                    // The common case is "the repos I am already working in",
                    // so that is what is checked when the picker opens.
                    Wants::AllReposMembersChecked => row.member,
                    // Nothing is preselected for add or remove: both change
                    // membership, and a default that does it by accident is
                    // worse than one keystroke.
                    Wants::NonMembers | Wants::Members => false,
                },
            })
            .collect();
        Self { rows, cursor: 0 }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The repositories with a check against them, in list order.
    pub fn checked(&self) -> Vec<&Row> {
        self.rows.iter().filter(|row| row.checked).collect()
    }

    /// How many are checked, for the footer's live count.
    pub fn count(&self) -> usize {
        self.rows.iter().filter(|row| row.checked).count()
    }

    /// Flip the row under the cursor.
    pub fn toggle(&mut self) -> bool {
        match self.rows.get_mut(self.cursor) {
            Some(row) => {
                row.checked = !row.checked;
                true
            }
            None => false,
        }
    }

    pub fn move_down(&mut self) -> bool {
        let last = self.rows.len().saturating_sub(1);
        if self.rows.is_empty() || self.cursor >= last {
            return false;
        }
        self.cursor += 1;
        true
    }

    pub fn move_up(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str, member: bool) -> RepoRow {
        RepoRow {
            repo: RepoId(name.into()),
            name: name.into(),
            base_branch: "origin/main".into(),
            base_from_origin_head: true,
            worktrees: 0,
            dirty: false,
            member,
        }
    }

    fn workspace() -> Vec<RepoRow> {
        vec![
            repo("billing-service", true),
            repo("web-app", true),
            repo("sdk-js", true),
            repo("api-gateway", false),
            repo("design-system", false),
        ]
    }

    #[test]
    fn new_shows_every_repo_with_the_members_checked() {
        // §4.2's sketch: members checked, non-members present and reachable.
        // "This task also touches api-gateway" is something you discover while
        // typing the branch name, not before.
        let select = Select::build(&workspace(), Wants::AllReposMembersChecked);
        assert_eq!(select.rows().len(), 5);
        assert_eq!(select.count(), 3);
        let names: Vec<&str> = select
            .checked()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(names, ["billing-service", "web-app", "sdk-js"]);
    }

    #[test]
    fn add_offers_only_what_is_not_already_a_member() {
        // There is nothing to add about a repo the session already holds.
        let select = Select::build(&workspace(), Wants::NonMembers);
        let names: Vec<&str> = select.rows().iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["api-gateway", "design-system"]);
    }

    #[test]
    fn remove_offers_only_members() {
        let select = Select::build(&workspace(), Wants::Members);
        assert_eq!(select.rows().len(), 3);
        assert!(select.rows().iter().all(|row| row.member));
    }

    #[test]
    fn nothing_is_preselected_for_a_command_that_changes_membership() {
        // `add` and `remove` both change what the session holds. A default
        // that does that by accident costs more than one keystroke saves.
        assert_eq!(Select::build(&workspace(), Wants::NonMembers).count(), 0);
        assert_eq!(Select::build(&workspace(), Wants::Members).count(), 0);
    }

    #[test]
    fn space_toggles_the_row_under_the_cursor() {
        let mut select = Select::build(&workspace(), Wants::AllReposMembersChecked);
        assert_eq!(select.count(), 3);
        assert!(
            select.toggle(),
            "the first row is a member, so this unchecks"
        );
        assert_eq!(select.count(), 2);
        assert!(select.toggle());
        assert_eq!(select.count(), 3);
    }

    #[test]
    fn checking_a_non_member_is_how_it_becomes_one() {
        let mut select = Select::build(&workspace(), Wants::AllReposMembersChecked);
        for _ in 0..3 {
            assert!(select.move_down());
        }
        assert!(select.toggle());
        let checked = select.checked();
        let newcomer = checked
            .iter()
            .find(|row| row.name == "api-gateway")
            .expect("checked");
        assert!(
            !newcomer.member,
            "it is not a member yet — that is what confirming does"
        );
        assert_eq!(select.count(), 4);
    }

    #[test]
    fn the_cursor_stops_at_the_ends() {
        let mut select = Select::build(&workspace(), Wants::AllReposMembersChecked);
        assert!(!select.move_up());
        for _ in 0..4 {
            assert!(select.move_down());
        }
        assert!(!select.move_down());
        assert_eq!(select.cursor(), 4);
    }

    #[test]
    fn an_empty_list_toggles_nothing_rather_than_panicking() {
        let mut select = Select::build(&[], Wants::AllReposMembersChecked);
        assert!(select.is_empty());
        assert!(!select.toggle());
        assert!(!select.move_down());
        assert_eq!(select.count(), 0);
    }
}
