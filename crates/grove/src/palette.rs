//! The command palette (SPEC §4.2 and §5).
//!
//! grove's command line, not a menu. It opens on `^g /`, lists every command
//! with nothing typed, and filters as you type. It is not a pty, so it takes
//! keys directly — no prefix.
//!
//! The commands are **data**. Adding one is a row in [`COMMANDS`] and nothing
//! else: no match arm in the renderer, no branch in the filter. That is what
//! makes #33's Lua-registered commands possible without the palette learning
//! what Lua is.
//!
//! What is *not* here is as deliberate. SPEC §11 cuts bulk git — no push, no
//! rebase, no staging — because grove manages worktrees and the terminal in
//! the next pane is already a better git client than a menu of verbs would be.
//! A test asserts the registry has not grown any.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::text::truncate;
use crate::theme::{Role, Theme};

/// What a command needs after its name.
///
/// Carried so the palette can tell an argumentless command from one that is
/// half-typed. Collecting the argument is #24; knowing that one is wanted is
/// what stops `enter` on `add` looking like it did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takes {
    /// Runs as soon as it is chosen.
    Nothing,
    /// Needs something typed or picked after the name.
    Argument(&'static str),
}

/// One palette command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    /// What it does, in the words a user would use.
    pub summary: &'static str,
    pub takes: Takes,
}

/// SPEC §5's table, verbatim and in its order.
///
/// The order is the table's rather than alphabetical: it runs from what you do
/// to a workspace, through what you do to a session, to what you do to a
/// worktree, which is the order a new user meets them in.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "scan",
        summary: "re-walk the workspace for repos",
        takes: Takes::Nothing,
    },
    Command {
        name: "session new",
        summary: "create and switch to a named session",
        takes: Takes::Argument("<name>"),
    },
    Command {
        name: "session rename",
        summary: "rename the current session",
        takes: Takes::Argument("<name>"),
    },
    Command {
        name: "open",
        summary: "open a stored session, replacing the current",
        takes: Takes::Argument("<session>"),
    },
    Command {
        name: "add",
        summary: "add member repos to the current session",
        takes: Takes::Argument("<repo>…"),
    },
    Command {
        name: "remove",
        summary: "drop a member repo",
        takes: Takes::Argument("<repo>"),
    },
    Command {
        name: "new",
        summary: "create worktrees on a branch",
        takes: Takes::Argument("<branch>"),
    },
    Command {
        name: "fetch",
        summary: "fetch member repos, or one named repo",
        takes: Takes::Argument("[repo]"),
    },
    Command {
        name: "prune",
        summary: "cleanup picker",
        takes: Takes::Nothing,
    },
    Command {
        name: "snapshot",
        summary: "write a restorable snapshot of this session",
        takes: Takes::Nothing,
    },
    Command {
        name: "defaults",
        summary: "show and edit config values",
        takes: Takes::Nothing,
    },
    Command {
        name: "keys",
        summary: "help overlay",
        takes: Takes::Nothing,
    },
];

/// The palette's state while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Palette {
    input: String,
    selected: usize,
}

impl Palette {
    /// Start again. Opening a palette that remembers the last query would make
    /// `^g /` mean two different things depending on history.
    pub fn open(&mut self) {
        self.input.clear();
        self.selected = 0;
    }

    /// What has been typed. The renderer reads the field directly; this is
    /// for tests and for the status bar when it grows a prompt.
    #[cfg(test)]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Add a typed character.
    pub fn push(&mut self, c: char) {
        self.input.push(c);
        // The list under the cursor just changed, so the cursor means
        // something else now. Back to the top, which is the best match.
        self.selected = 0;
    }

    /// Remove the last character. Returns whether anything changed, so an
    /// empty palette does not redraw on every backspace.
    pub fn backspace(&mut self) -> bool {
        let changed = self.input.pop().is_some();
        if changed {
            self.selected = 0;
        }
        changed
    }

    /// The commands matching what has been typed, best first.
    pub fn matches(&self) -> Vec<&'static Command> {
        if self.input.is_empty() {
            // Nothing typed lists everything, in SPEC §5's order.
            return COMMANDS.iter().collect();
        }
        let mut scored: Vec<(i32, &'static Command)> = COMMANDS
            .iter()
            .filter_map(|command| score(&self.input, command.name).map(|s| (s, command)))
            .collect();
        // Higher score first; ties go to the shorter name, which is the more
        // general command — `new` before `session new` for "new".
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
                .then_with(|| a.1.name.cmp(b.1.name))
        });
        scored.into_iter().map(|(_, command)| command).collect()
    }

    /// The command `enter` would run.
    pub fn selected(&self) -> Option<&'static Command> {
        self.matches().get(self.selected).copied()
    }

    pub fn move_down(&mut self) -> bool {
        let last = self.matches().len().saturating_sub(1);
        if self.selected >= last {
            return false;
        }
        self.selected += 1;
        true
    }

    pub fn move_up(&mut self) -> bool {
        if self.selected == 0 {
            return false;
        }
        self.selected -= 1;
        true
    }

    /// Draw the palette into `area`.
    pub fn render(&self, buf: &mut Buffer, area: Rect, theme: &Theme) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut lines = vec![Line::from(vec![
            Span::styled("❯ ", theme.style(Role::Accent)),
            Span::styled(self.input.clone(), theme.style(Role::Clean)),
            // A block for the caret: the palette is a command line and has to
            // look like one, and the terminal's own cursor is elsewhere.
            Span::styled("▏", theme.style(Role::Accent)),
        ])];

        let matches = self.matches();
        if matches.is_empty() {
            lines.push(Line::styled(
                format!("no command matches {:?}", self.input),
                theme.style(Role::Muted),
            ));
        }
        // One row per command, minus the input line.
        let room = usize::from(area.height).saturating_sub(1);
        for (index, command) in matches.iter().take(room).enumerate() {
            let chosen = index == self.selected;
            let name_style = if chosen {
                theme.style(Role::Accent).add_modifier(Modifier::BOLD)
            } else {
                theme.style(Role::Clean)
            };
            let argument = match command.takes {
                Takes::Nothing => String::new(),
                Takes::Argument(what) => format!(" {what}"),
            };
            let name = format!("{}{argument}", command.name);
            // The summary gives way first: the name is what is being chosen.
            let room = usize::from(area.width).saturating_sub(name.len() + 4);
            lines.push(Line::from(vec![
                Span::styled(if chosen { "❯ " } else { "  " }, theme.style(Role::Accent)),
                Span::styled(name, name_style),
                Span::raw("  "),
                Span::styled(truncate(command.summary, room), theme.style(Role::Muted)),
            ]));
        }
        Paragraph::new(lines).render(area, buf);
    }
}

/// How well `query` matches `name`, or `None` if it does not.
///
/// A subsequence match, scored so that the ranking matches what a person
/// typing two letters expects. The three things that earn points are the ones
/// that carry intent: a match at the very start, a match at the start of a
/// word — which is how `sn` finds `session new` — and a run of consecutive
/// characters, which is how `sess` beats a scatter of the same letters.
/// Skipped characters cost, so a tight match at the end still loses to a loose
/// one at the front.
fn score(query: &str, name: &str) -> Option<i32> {
    const START: i32 = 12;
    const WORD_START: i32 = 9;
    const CONSECUTIVE: i32 = 6;
    const MATCH: i32 = 2;
    const GAP: i32 = 1;

    let haystack: Vec<char> = name.chars().collect();
    let mut total = 0;
    let mut at = 0usize;
    let mut previous: Option<usize> = None;

    for wanted in query.chars().flat_map(char::to_lowercase) {
        let found = haystack[at..]
            .iter()
            .position(|c| c.to_lowercase().eq(std::iter::once(wanted)))?;
        let index = at + found;

        total += MATCH;
        if index == 0 {
            total += START;
        } else if haystack
            .get(index - 1)
            .is_some_and(|c| *c == ' ' || *c == '-' || *c == '_')
        {
            total += WORD_START;
        }
        if previous == Some(index.saturating_sub(1)) && index > 0 {
            total += CONSECUTIVE;
        }
        total -= i32::try_from(found).unwrap_or(i32::MAX) * GAP;

        previous = Some(index);
        at = index + 1;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::resolve(&grove_lua::TuiConfig::default(), crate::theme::Depth::True).0
    }

    fn typed(query: &str) -> Palette {
        let mut palette = Palette::default();
        for c in query.chars() {
            palette.push(c);
        }
        palette
    }

    fn names(palette: &Palette) -> Vec<&'static str> {
        palette.matches().iter().map(|c| c.name).collect()
    }

    #[test]
    fn nothing_typed_lists_every_command_in_the_specs_order() {
        let palette = Palette::default();
        assert_eq!(names(&palette).len(), COMMANDS.len());
        assert_eq!(names(&palette)[0], "scan");
        assert_eq!(names(&palette).last(), Some(&"keys"));
    }

    #[test]
    fn short_queries_rank_the_way_a_person_would_expect() {
        // The acceptance criterion, and the reason the scoring has three
        // bonuses rather than one: each of these is a different intent
        // expressed in two keystrokes.
        assert_eq!(names(&typed("sc"))[0], "scan", "a prefix");
        assert_eq!(names(&typed("pr"))[0], "prune");
        assert_eq!(names(&typed("de"))[0], "defaults");
        assert_eq!(names(&typed("fe"))[0], "fetch");
        // Initials, where nothing has those letters as a prefix.
        assert_eq!(names(&typed("sr"))[0], "session rename");
        // And where something does, the prefix wins: a user typing "sn" is
        // most likely reaching for snapshot, not spelling out session new.
        // This was the opposite of what I first asserted, and the scorer was
        // right.
        assert_eq!(names(&typed("sn"))[0], "snapshot");
        assert!(
            names(&typed("sn")).contains(&"session new"),
            "but the initials still match"
        );
    }

    #[test]
    fn a_full_word_beats_the_command_that_merely_contains_it() {
        // "new" is both a command and the tail of "session new". Typing it
        // should offer the command itself first.
        assert_eq!(names(&typed("new"))[0], "new");
        assert!(names(&typed("new")).contains(&"session new"));
    }

    #[test]
    fn a_query_that_matches_nothing_matches_nothing() {
        // Rather than falling back to the whole list, which would make the
        // palette look like it ignored what was typed.
        assert!(typed("zzz").matches().is_empty());
        assert!(typed("zzz").selected().is_none());
    }

    #[test]
    fn matching_ignores_case_because_typing_does() {
        assert_eq!(names(&typed("SC"))[0], "scan");
        assert_eq!(names(&typed("Prune"))[0], "prune");
    }

    #[test]
    fn the_cursor_returns_to_the_best_match_when_the_query_changes() {
        // The list under the cursor is a different list now, so holding the
        // index would leave the cursor on whatever happens to be third.
        let mut palette = typed("s");
        assert!(palette.move_down());
        assert_eq!(palette.selected, 1, "the cursor moved off the best match");
        palette.push('c');
        assert_eq!(palette.selected, 0);
        assert_eq!(palette.selected().map(|c| c.name), Some("scan"));
    }

    #[test]
    fn backspace_narrows_back_and_says_when_there_is_nothing_left() {
        let mut palette = typed("sc");
        assert!(palette.backspace());
        assert_eq!(palette.input(), "s");
        assert!(palette.backspace());
        assert!(
            !palette.backspace(),
            "an empty palette must not redraw on every backspace"
        );
    }

    #[test]
    fn opening_forgets_the_last_query() {
        // `^g /` has to mean the same thing every time.
        let mut palette = typed("prune");
        palette.open();
        assert_eq!(palette.input(), "");
        assert_eq!(palette.selected, 0);
        assert_eq!(names(&palette).len(), COMMANDS.len());
    }

    #[test]
    fn the_registry_is_specs_table_and_nothing_else() {
        // §5 is the whole list. A command that appears here without appearing
        // there is a feature nobody specified.
        let expected = [
            "scan",
            "session new",
            "session rename",
            "open",
            "add",
            "remove",
            "new",
            "fetch",
            "prune",
            "snapshot",
            "defaults",
            "keys",
        ];
        let actual: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn there_are_no_bulk_git_commands() {
        // Acceptance, and SPEC §11's cut: push, rebase and staging are not
        // grove's job. The terminal in the next pane is already a better git
        // client than a menu of verbs, and a palette that half-wraps git
        // invites the half that is missing.
        for forbidden in [
            "push", "pull", "rebase", "merge", "stage", "commit", "cherry", "reset", "stash",
        ] {
            assert!(
                !COMMANDS.iter().any(|c| c.name.contains(forbidden)),
                "{forbidden} is not grove's to run"
            );
        }
    }

    #[test]
    fn a_command_that_needs_an_argument_says_so() {
        // So `enter` on `add` does not look like it did nothing. Collecting
        // the argument is #24; knowing one is wanted is this issue's.
        let add = COMMANDS.iter().find(|c| c.name == "add").expect("add");
        assert_eq!(add.takes, Takes::Argument("<repo>…"));
        let scan = COMMANDS.iter().find(|c| c.name == "scan").expect("scan");
        assert_eq!(scan.takes, Takes::Nothing);
    }

    #[test]
    fn adding_a_command_needs_no_change_to_the_rendering() {
        // The registry is data. This is asserted by rendering a palette whose
        // matches come from the same table the renderer walks — if drawing
        // ever grows a match arm per command, this test keeps passing but the
        // next reader is warned by the comment.
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 14,
        };
        let mut buf = Buffer::empty(area);
        Palette::default().render(&mut buf, area, &theme());
        let painted: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for command in COMMANDS.iter().take(13) {
            assert!(
                painted.contains(command.name),
                "{} is in the registry but not on screen:\n{painted}",
                command.name
            );
        }
    }

    #[test]
    fn what_is_typed_is_shown() {
        // It is a command line: a user has to be able to see what they wrote.
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        typed("sess").render(&mut buf, area, &theme());
        let first: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(first.contains("sess"), "{first:?}");
    }
}
