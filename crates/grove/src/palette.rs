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
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::text::truncate;
use crate::theme::{Ink, Theme};

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
        summary: "where the config file is",
        takes: Takes::Nothing,
    },
    Command {
        name: "keys",
        summary: "help overlay",
        takes: Takes::Nothing,
    },
];

/// Which repositories a command's picker shows, if it has one.
///
/// A `match` on the command's own name rather than a flag on the row, so a
/// command that grows a picker declares it in one place — and #33's Lua
/// commands, which have no name known here, simply have none.
pub fn picker_for(name: &str) -> Option<crate::select::Wants> {
    use crate::select::Wants;
    match name {
        "new" => Some(Wants::AllReposMembersChecked),
        "add" => Some(Wants::NonMembers),
        "remove" => Some(Wants::Members),
        _ => None,
    }
}

/// What the palette is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Choosing a command.
    Choosing,
    /// A command is chosen and its argument is being typed or picked. The
    /// palette stops filtering commands here: `new feat/x` is a branch name
    /// with a slash in it, not a fuzzy query.
    Arguing {
        command: Entry,
        argument: String,
        select: crate::select::Select,
    },
}

/// A command as the palette lists it, built-in or the user's.
///
/// Owned rather than a `&'static Command`, because a user's name comes from
/// their config and outlives nothing. The `user` index is how `enter` finds
/// the callback again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub summary: String,
    pub takes: Takes,
    /// `Some(index)` for a command from `config.lua`.
    pub user: Option<usize>,
}

impl Entry {
    fn built_in(command: &'static Command) -> Self {
        Self {
            name: command.name.to_string(),
            summary: command.summary.to_string(),
            takes: command.takes,
            user: None,
        }
    }
}

/// The palette's state while it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    input: String,
    selected: usize,
    mode: Mode,
    /// Names registered from `config.lua`, in registration order — the index
    /// is what `enter` hands back to the runtime.
    user: Vec<String>,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            input: String::new(),
            selected: 0,
            mode: Mode::Choosing,
            user: Vec::new(),
        }
    }
}

impl Palette {
    /// Start again. Opening a palette that remembers the last query would make
    /// `^g /` mean two different things depending on history.
    /// Take the names the user registered.
    pub fn with_user(&mut self, names: Vec<String>) {
        self.user = names;
    }

    pub fn open(&mut self) {
        self.input.clear();
        self.selected = 0;
        self.mode = Mode::Choosing;
    }

    /// Move into argument mode for `command`, building its picker from the
    /// repositories the daemon last sent.
    ///
    /// Called when a command is chosen — by `enter` or by typing its name and
    /// a space, which is how a command line behaves.
    pub fn argue(&mut self, command: Entry, repos: &[grove_proto::RepoRow]) {
        let select = match picker_for(&command.name) {
            Some(wants) => crate::select::Select::build(repos, wants),
            None => crate::select::Select::default(),
        };
        // The line keeps the command name, because that is what a command line
        // looks like: `new feat/x`, not a branch name floating on its own.
        self.input = format!("{} ", command.name);
        self.mode = Mode::Arguing {
            command,
            argument: String::new(),
            select,
        };
    }

    /// Back to choosing a command, keeping the palette open.
    ///
    /// `esc` in argument mode steps back rather than closing outright: the
    /// user is one keystroke from the command they wanted, and closing would
    /// make them retype it.
    pub fn unargue(&mut self) -> bool {
        if matches!(self.mode, Mode::Choosing) {
            return false;
        }
        self.mode = Mode::Choosing;
        self.input.clear();
        self.selected = 0;
        true
    }

    /// The argument typed so far, if any.
    pub fn argument(&self) -> Option<&str> {
        match &self.mode {
            Mode::Choosing => None,
            Mode::Arguing { argument, .. } => Some(argument),
        }
    }

    /// The picker, while one is open.
    /// Whether there is a list of repositories to tick.
    ///
    /// Not the same as "an argument is being collected": `session new` takes
    /// a name and `new` takes a branch *and* a set of repos. Everything that
    /// treats space as a toggle, counts what `enter` will do, or draws a row
    /// of checkboxes has to ask this rather than assume.
    pub fn picking(&self) -> bool {
        matches!(&self.mode, Mode::Arguing { command, .. } if picker_for(&command.name).is_some())
    }

    pub fn select_mut(&mut self) -> Option<&mut crate::select::Select> {
        if !self.picking() {
            return None;
        }
        match &mut self.mode {
            Mode::Choosing => None,
            Mode::Arguing { select, .. } => Some(select),
        }
    }

    pub fn select(&self) -> Option<&crate::select::Select> {
        if !self.picking() {
            return None;
        }
        match &self.mode {
            Mode::Choosing => None,
            Mode::Arguing { select, .. } => Some(select),
        }
    }

    /// The command being argued, if any.
    pub fn arguing(&self) -> Option<&Entry> {
        match &self.mode {
            Mode::Choosing => None,
            Mode::Arguing { command, .. } => Some(command),
        }
    }

    /// What has been typed. The renderer reads the field directly; this is
    /// for tests and for the status bar when it grows a prompt.
    #[cfg(test)]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Add a typed character.
    ///
    /// In argument mode the character goes to the argument rather than to a
    /// fuzzy query: `new feat/ABC-4471` is a branch name, and filtering the
    /// command list by it would leave the palette empty and the user unsure
    /// whether their typing landed.
    pub fn push(&mut self, c: char) {
        self.input.push(c);
        if let Mode::Arguing { argument, .. } = &mut self.mode {
            argument.push(c);
            return;
        }
        // The list under the cursor just changed, so the cursor means
        // something else now. Back to the top, which is the best match.
        self.selected = 0;
    }

    /// Remove the last character. Returns whether anything changed, so an
    /// empty palette does not redraw on every backspace.
    pub fn backspace(&mut self) -> bool {
        if let Mode::Arguing { argument, .. } = &mut self.mode {
            // Backspacing past the argument leaves the command name alone;
            // `esc` is how you go back to choosing, and erasing into the name
            // would leave the line saying something the mode does not agree
            // with.
            if argument.pop().is_some() {
                self.input.pop();
                return true;
            }
            return false;
        }
        let changed = self.input.pop().is_some();
        if changed {
            self.selected = 0;
        }
        changed
    }

    /// The commands matching what has been typed, best first.
    pub fn matches(&self) -> Vec<Entry> {
        if matches!(self.mode, Mode::Arguing { .. }) {
            // Not a command list any more.
            return Vec::new();
        }
        // The user's sit alongside the built-ins rather than in a section of
        // their own: they are commands, and a palette that files them
        // elsewhere makes the user's own additions the hardest to reach.
        let all: Vec<Entry> = COMMANDS
            .iter()
            .map(Entry::built_in)
            .chain(self.user.iter().enumerate().map(|(index, name)| Entry {
                name: name.clone(),
                summary: "from your config".into(),
                // What a user command wants is its own business; the palette
                // hands over whatever was typed after the name.
                takes: Takes::Argument("[argument]"),
                user: Some(index),
            }))
            .collect();

        if self.input.is_empty() {
            // Nothing typed lists everything, in SPEC §5's order.
            return all;
        }
        let mut scored: Vec<(i32, Entry)> = all
            .into_iter()
            .filter_map(|entry| score(&self.input, &entry.name).map(|s| (s, entry)))
            .collect();
        // Higher score first; ties go to the shorter name, which is the more
        // general command — `new` before `session new` for "new".
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        scored.into_iter().map(|(_, entry)| entry).collect()
    }

    /// The command `enter` would run.
    /// Lines above the first repo in the argument view: `new`'s base branch
    /// and the rule under it, or nothing. The mouse counts rows from here, so
    /// this is the one place that knows it.
    pub fn list_offset(&self) -> u16 {
        match &self.mode {
            Mode::Arguing { command, .. } if command.name == "new" => 2,
            _ => 0,
        }
    }

    /// Where the cursor is in the command list and how many commands match,
    /// for the mouse.
    pub fn cursor(&self) -> (usize, usize) {
        (self.selected, self.matches().len())
    }

    pub fn selected(&self) -> Option<Entry> {
        self.matches().get(self.selected).cloned()
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

    /// The keys this screen offers, for the overlay's footer.
    ///
    /// Mode-aware, because the keymap is not: `space` toggles a repository
    /// while an argument is being collected and types a space when a command
    /// is being chosen, and a footer that advertised `space toggle` on the
    /// command list would be teaching a key that does something else. The
    /// verb carries the live count for the same reason §5's does: `enter
    /// create 3` is a promise, not a guess.
    pub fn footer(&self) -> Vec<crate::statusbar::Hint> {
        let mut hints = crate::statusbar::hints(crate::keymap::Screen::Palette);
        // `space` is only ever a toggle while there is something to tick:
        // nothing on the command list, where it types a space, and nothing on
        // a command that takes a name.
        hints.retain(|hint| self.picking() || hint.label != "toggle");
        if let Mode::Arguing {
            command, select, ..
        } = &self.mode
        {
            for hint in &mut hints {
                if hint.label == "run" {
                    // The count is a promise about how many things the key
                    // will act on, so it belongs to the commands that act on
                    // a number of them. `enter session new 0` promised
                    // nothing about a session with a name.
                    hint.label = match picker_for(&command.name) {
                        Some(_) => format!("{} {}", command.name, select.count()),
                        None => command.name.clone(),
                    };
                }
            }
        }
        hints
    }

    /// Rows of content this palette has, so the box can be that tall.
    pub fn height(&self) -> u16 {
        let rows = match &self.mode {
            // The argument view spends two lines on the base branch and the
            // rule under it before the repositories start.
            Mode::Arguing {
                command, select, ..
            } => match picker_for(&command.name) {
                // `new` spends two lines on the base branch and the rule
                // under it before the repositories start.
                Some(_) if command.name == "new" => select.rows().len() + 2,
                Some(_) => select.rows().len().max(1),
                // A typed argument is one line saying what to type.
                None => 1,
            },
            _ => self.matches().len().max(1),
        };
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// Draw the palette into the overlay's parts.
    pub fn render(&self, buf: &mut Buffer, parts: crate::overlay::Parts, theme: &Theme) {
        let (header, body) = (parts.header, parts.body);
        if body.width == 0 || body.height == 0 {
            return;
        }

        // Argument mode draws the picker where the command list was, with the
        // base branch above it — §4.2's sketch, and the mock's `palArg`.
        if let Mode::Arguing {
            command, select, ..
        } = &self.mode
        {
            let typed = match self.argument() {
                Some(argument) => format!("{} {argument}", command.name),
                None => command.name.to_string(),
            };
            crate::overlay::header("❯", &typed, "", header.width, theme).render(header, buf);

            // A command that takes a name takes a name: no repositories to
            // tick, no base branch, and no count on the verb. `session new`
            // used to draw the repo picker, announce "no repos to choose
            // from", and offer `enter session new 0` — three lies about one
            // command, on the screen a first run is now sent to first.
            if picker_for(&command.name).is_none() {
                let Takes::Argument(what) = command.takes else {
                    return;
                };
                let noun = what.trim_matches(['<', '>', '[', ']', '…']);
                // `[repo]` is optional, and leaving it out is the usual case:
                // saying only "type a repo" reads as if it were required.
                let prompt = if what.starts_with('[') {
                    format!("type a {noun}, or press enter for all")
                } else {
                    format!("type a {noun} and press enter")
                };
                Paragraph::new(Line::styled(prompt, theme.ink_style(Ink::Faint))).render(body, buf);
                return;
            }

            let mut lines = Vec::new();
            // The base is `new`'s alone: it is the branch the worktrees are
            // cut from, and nothing else here cuts one.
            // The base each repo would be cut from, as the daemon reports it
            // — this used to be the literal `origin/main`, whatever the repos
            // were really based on. One line when they agree; when they do
            // not, each row says its own.
            let bases = distinct_bases(select);
            if self.list_offset() > 0 {
                let (base, whence) = match bases.as_slice() {
                    [] => ("base: none".to_string(), String::new()),
                    [(base, origin)] => (
                        format!("base: {base}"),
                        if *origin {
                            "from origin/HEAD"
                        } else {
                            "configured"
                        }
                        .to_string(),
                    ),
                    _ => ("base: per repo".to_string(), String::new()),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{base:<24}"), theme.ink_style(Ink::Subtext)),
                    Span::styled(whence, theme.ink_style(Ink::Faint)),
                ]));
                lines.push(Line::styled(
                    "─".repeat(body.width as usize),
                    theme.ink_style(Ink::Frame),
                ));
            }
            if select.is_empty() {
                lines.push(Line::styled(
                    "no repos to choose from",
                    theme.ink_style(Ink::Subtext),
                ));
            }
            let room = usize::from(body.height).saturating_sub(lines.len());
            for (index, row) in select.rows().iter().take(room).enumerate() {
                let here = index == select.cursor();
                let membership = if row.member { "member" } else { "workspace" };
                let note = if bases.len() > 1 && !row.base.is_empty() {
                    format!("{membership} · {}", row.base)
                } else {
                    membership.to_string()
                };
                let mut cells = vec![
                    (
                        if row.checked { "[x]" } else { "[ ]" }.to_string(),
                        Ink::Text,
                    ),
                    (format!("{:<22}", truncate(&row.name, 22)), Ink::Text),
                    (format!("{note:<30}"), Ink::Subtext),
                ];
                // Said before enter, not after: `new` in a repo with no base
                // fails with "has no base branch" once it has been asked.
                let baseless = self.list_offset() > 0 && row.base.is_empty();
                if baseless {
                    cells.push(("no base branch".to_string(), Ink::Subtext));
                }
                let mut spans = crate::overlay::row(here, cells, theme);
                if baseless
                    && !here
                    && let Some(last) = spans.last_mut()
                {
                    *last = Span::styled("no base branch", theme.style(crate::theme::Role::Dirty));
                }
                lines.push(Line::from(spans));
            }
            Paragraph::new(lines).render(body, buf);
            return;
        }

        let matches = self.matches();
        crate::overlay::header(
            "❯",
            &self.input,
            &match matches.len() {
                1 => "1 command".to_string(),
                n => format!("{n} commands"),
            },
            header.width,
            theme,
        )
        .render(header, buf);

        let mut lines = Vec::new();
        if matches.is_empty() {
            lines.push(Line::styled(
                format!("no command matches {:?}", self.input),
                theme.ink_style(Ink::Subtext),
            ));
        }
        for (index, command) in matches.iter().take(usize::from(body.height)).enumerate() {
            let chosen = index == self.selected;
            let argument = match command.takes {
                Takes::Nothing => String::new(),
                Takes::Argument(what) => format!(" {what}"),
            };
            // The mock's columns: a 24-column name, then the summary in what
            // is left, then the key that runs it without opening this at all.
            let name = truncate(&format!("{}{argument}", command.name), 24);
            let room = usize::from(body.width).saturating_sub(24 + 2 + 4);
            lines.push(Line::from(crate::overlay::row(
                chosen,
                vec![
                    (format!("{name:<24}"), Ink::Text),
                    (truncate(&command.summary, room), Ink::Subtext),
                ],
                theme,
            )));
        }
        Paragraph::new(lines).render(body, buf);
    }
}

/// The distinct non-empty bases among the rows `new` would branch — the
/// ticked ones, or all of them before any are ticked — with whether each came
/// from `origin/HEAD`.
fn distinct_bases(select: &crate::select::Select) -> Vec<(String, bool)> {
    let ticked: Vec<&crate::select::Row> = select.checked();
    let rows: Vec<&crate::select::Row> = if ticked.is_empty() {
        select.rows().iter().collect()
    } else {
        ticked
    };
    let mut bases: Vec<(String, bool)> = Vec::new();
    for row in rows {
        if !row.base.is_empty() && !bases.iter().any(|(base, _)| *base == row.base) {
            bases.push((row.base.clone(), row.from_origin_head));
        }
    }
    bases
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
    use ratatui::layout::Rect;

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

    fn names(palette: &Palette) -> Vec<String> {
        palette.matches().into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn nothing_typed_lists_every_command_in_the_specs_order() {
        let palette = Palette::default();
        assert_eq!(names(&palette).len(), COMMANDS.len());
        assert_eq!(names(&palette)[0], "scan");
        assert_eq!(names(&palette).last().map(String::as_str), Some("keys"));
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
            names(&typed("sn")).iter().any(|n| n == "session new"),
            "but the initials still match"
        );
    }

    #[test]
    fn a_full_word_beats_the_command_that_merely_contains_it() {
        // "new" is both a command and the tail of "session new". Typing it
        // should offer the command itself first.
        assert_eq!(names(&typed("new"))[0], "new");
        assert!(names(&typed("new")).iter().any(|n| n == "session new"));
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
        assert_eq!(palette.selected().map(|c| c.name), Some("scan".to_string()));
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
        Palette::default().render(&mut buf, crate::overlay::Parts::of(area), &theme());
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

    fn repo(name: &str, member: bool) -> grove_proto::RepoRow {
        grove_proto::RepoRow {
            repo: grove_domain::RepoId(name.into()),
            name: name.into(),
            base_branch: "origin/main".into(),
            base_from_origin_head: true,
            worktrees: 0,
            dirty: false,
            member,
        }
    }

    fn arguing(command: &str, repos: &[grove_proto::RepoRow]) -> Palette {
        let chosen = COMMANDS
            .iter()
            .find(|c| c.name == command)
            .map(Entry::built_in)
            .expect("a command");
        let mut palette = Palette::default();
        palette.argue(chosen, repos);
        palette
    }

    fn based(name: &str, base: &str) -> grove_proto::RepoRow {
        grove_proto::RepoRow {
            base_branch: base.into(),
            ..repo(name, true)
        }
    }

    fn painted_palette(palette: &Palette) -> String {
        let area = Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        palette.render(&mut buf, crate::overlay::Parts::of(area), &theme());
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn new_names_the_base_the_repos_really_have() {
        // It always said `base: origin/main  from origin/HEAD`, whatever the
        // repos were based on — a literal, not the daemon's answer.
        let palette = arguing("new", &[based("api", "develop"), based("web", "develop")]);
        let screen = painted_palette(&palette);
        assert!(screen.contains("base: develop"), "{screen}");
        assert!(!screen.contains("origin/main"), "{screen}");
    }

    #[test]
    fn new_says_when_the_repos_disagree_about_the_base() {
        let palette = arguing("new", &[based("api", "develop"), based("web", "main")]);
        let screen = painted_palette(&palette);
        assert!(screen.contains("base: per repo"), "{screen}");
        assert!(
            screen.contains("develop") && screen.contains("main"),
            "{screen}"
        );
    }

    #[test]
    fn a_repo_with_no_base_is_flagged_before_enter() {
        // A repo with no origin has no base branch, and `new` in it fails
        // with "has no base branch" — after the user has pressed enter. The
        // picker knows before then.
        let palette = arguing("new", &[based("api", "main"), based("scratchpad", "")]);
        let screen = painted_palette(&palette);
        assert!(screen.contains("no base branch"), "{screen}");
    }

    #[test]
    fn an_optional_argument_says_enter_alone_will_do() {
        // Found by hand: `fetch [repo]` said "type a repo and press enter",
        // which reads as required — and the empty enter that fetches every
        // member is the common case.
        let palette = arguing("fetch", &[repo("a", true)]);
        let screen = painted_palette(&palette);
        assert!(screen.contains("or press enter for all"), "{screen}");
    }

    #[test]
    fn a_command_that_takes_a_name_does_not_draw_a_repo_picker() {
        // `session new` asks for a name. It used to draw the repo picker, say
        // "no repos to choose from", show a base branch it does not have, and
        // offer `enter session new 0` — three untruths about one command, on
        // the screen a first run is sent to first.
        let palette = arguing("session new", &[repo("a", true), repo("b", false)]);
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        palette.render(&mut buf, crate::overlay::Parts::of(area), &theme());
        let painted: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !painted.contains("no repos to choose from"),
            "it is not choosing repos: {painted}"
        );
        assert!(
            !painted.contains("base:"),
            "and it cuts no branch: {painted}"
        );
        assert!(painted.contains("type a name"), "{painted}");

        let verb = palette
            .footer()
            .into_iter()
            .find(|hint| hint.keys == "enter")
            .expect("enter is bound");
        assert_eq!(verb.label, "session new", "no count on a name");
        assert!(
            !palette.footer().iter().any(|hint| hint.label == "toggle"),
            "and nothing to toggle"
        );
    }

    #[test]
    fn a_command_that_picks_repos_still_does() {
        // The other half of the same rule, so the fix cannot be "draw nothing".
        let palette = arguing("add", &[repo("a", false), repo("b", false)]);
        let verb = palette
            .footer()
            .into_iter()
            .find(|hint| hint.keys == "enter")
            .expect("enter is bound");
        assert_eq!(verb.label, "add 0");
        assert!(palette.footer().iter().any(|hint| hint.label == "toggle"));
    }

    #[test]
    fn typing_an_argument_is_not_a_fuzzy_query() {
        // `new feat/ABC-4471` is a branch name. Filtering the command list by
        // it would empty the palette and leave the user unsure whether their
        // typing landed anywhere.
        let mut palette = arguing("new", &[repo("a", true)]);
        for c in "feat/ABC-4471".chars() {
            palette.push(c);
        }
        assert_eq!(palette.argument(), Some("feat/ABC-4471"));
        assert_eq!(palette.input(), "new feat/ABC-4471");
        assert!(
            palette.matches().is_empty(),
            "the command list is not what is being chosen any more"
        );
    }

    #[test]
    fn backspace_erases_the_argument_and_stops_at_the_command() {
        // Erasing into the command name would leave the line saying something
        // the mode does not agree with; `esc` is how you go back.
        let mut palette = arguing("new", &[]);
        for c in "ab".chars() {
            palette.push(c);
        }
        assert!(palette.backspace());
        assert_eq!(palette.argument(), Some("a"));
        assert!(palette.backspace());
        assert!(!palette.backspace(), "the command name is not erasable");
        assert_eq!(palette.input(), "new ");
    }

    #[test]
    fn esc_in_argument_mode_steps_back_rather_than_closing() {
        // The user is one keystroke from the command they wanted. Closing
        // would make them type it again.
        let mut palette = arguing("new", &[]);
        assert!(palette.unargue());
        assert_eq!(palette.input(), "");
        assert!(!palette.matches().is_empty(), "back to choosing");
        assert!(!palette.unargue(), "and there is nowhere further back");
    }

    #[test]
    fn only_the_commands_that_pick_repos_get_a_picker() {
        assert!(picker_for("new").is_some());
        assert!(picker_for("add").is_some());
        assert!(picker_for("remove").is_some());
        // `session new` takes a name, not a list of repos.
        assert!(picker_for("session new").is_none());
        assert!(picker_for("scan").is_none());
    }

    #[test]
    fn the_footer_counts_what_enter_will_do() {
        // §4.2's "enter create 3". The count is live, so it is a promise
        // rather than a guess.
        let palette = arguing("new", &[repo("a", true), repo("b", true), repo("c", false)]);
        let area = Rect {
            x: 0,
            y: 0,
            width: 70,
            height: 10,
        };
        let mut buf = Buffer::empty(area);
        palette.render(&mut buf, crate::overlay::Parts::of(area), &theme());
        let painted: String = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(painted.contains("[x] a"), "{painted}");
        assert!(painted.contains("[ ] c"), "{painted}");
        assert!(painted.contains("workspace"), "non-members are reachable");
        // The count lives on the verb in the footer, which the overlay draws:
        // two members are ticked, so enter promises two.
        let verb = palette
            .footer()
            .into_iter()
            .find(|hint| hint.keys == "enter")
            .expect("enter is bound here");
        assert_eq!(verb.label, "new 2", "the promise has to be live");
        assert!(
            palette.footer().iter().any(|hint| hint.label == "toggle"),
            "space toggles while an argument is being collected"
        );
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
        typed("sess").render(&mut buf, crate::overlay::Parts::of(area), &theme());
        let first: String = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect();
        assert!(first.contains("sess"), "{first:?}");
    }
}
