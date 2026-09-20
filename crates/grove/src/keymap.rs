//! Key routing.
//!
//! The rule from SPEC §3.1 is subtler than a plain tmux prefix:
//!
//! > A focused pane that is a pty takes every keystroke; `^g` is how you
//! > address grove instead. Everything that is not a pty takes keys directly.
//!
//! So arrows in the REPOS and WORKTREES lists need no prefix — there is no pty
//! to send them to. Typing in the palette needs no prefix. Only the worktree
//! terminal and the scratch shell require `^g` to get grove's attention. That
//! keeps the prefix where it earns its keep and out of the way everywhere else.
//!
//! Bindings are **data**, not a `match` arm. The status bar and the help
//! overlay both render the current screen's keys, and a hand-written list in
//! either would drift from what the code does the first time a binding moves.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// How long grove waits for the second key of a `^g` chord before deciding the
/// user changed their mind. Without a timeout a stray `^g` leaves the next
/// keystroke silently swallowed, which reads as a dropped key.
pub const PREFIX_TIMEOUT: Duration = Duration::from_millis(1500);

/// Which screen's bindings are live. Drives the status bar and help overlay.
///
/// Every variant is already routable and already renders its hints; only the
/// screens that *construct* them are unwritten, which is #23 through #30. The
/// allow is scoped to this enum rather than the module so that genuinely dead
/// code elsewhere still fails the build.
#[allow(dead_code, reason = "constructed by the screens in #23-#30")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dash,
    Palette,
    Picker,
    Diff,
    Shell,
    EndSession,
}

/// Which dash pane has focus. Focus is functional, not decorative: `↑`/`↓`
/// drive whichever list holds it. The source mock drew a focus ring but always
/// moved the worktree list, which is the bug this type exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Repos,
    Worktrees,
    Terminal,
}

impl Screen {
    /// Whether this screen is itself a pty, regardless of pane focus.
    pub fn is_pty_screen(self) -> bool {
        matches!(self, Self::Shell)
    }

    /// Whether a pty can hold focus on this screen.
    ///
    /// The dash's third pane is a terminal, and the scratch shell is one
    /// outright. Every other screen is an overlay drawn over them: it owns the
    /// keyboard while open, regardless of which dash pane had focus.
    pub fn pty_can_hold_focus(self) -> bool {
        matches!(self, Self::Dash | Self::Shell)
    }

    /// Whether unbound printable keys are content rather than noise.
    ///
    /// Only the palette: it is a command line, so "nothing matched" is the
    /// normal case. Lists and confirms have nothing to type into.
    pub fn takes_text(self) -> bool {
        matches!(self, Self::Palette)
    }
}

impl Focus {
    /// A pty takes raw keys; a list does not. This single predicate is what the
    /// whole prefix rule turns on.
    pub fn is_pty(self) -> bool {
        matches!(self, Self::Terminal)
    }

    pub fn next(self) -> Self {
        match self {
            Self::Repos => Self::Worktrees,
            Self::Worktrees => Self::Terminal,
            Self::Terminal => Self::Repos,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            Self::Repos => Self::Terminal,
            Self::Worktrees => Self::Repos,
            Self::Terminal => Self::Worktrees,
        }
    }
}

/// Something grove does in response to a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenPalette,
    OpenPicker,
    OpenDiff,
    OpenShell,
    PrefillNew,
    EndSession,
    Snapshot,
    Help,
    Quit,
    CycleFocus,
    CycleFocusBack,
    TogglePane(u8),
    MoveUp,
    MoveDown,
    /// Scroll the terminal pane's history. Prefixed, because the pty has the
    /// unprefixed arrows.
    ScrollUp,
    ScrollDown,
    /// Open a terminal for the selected worktree, for the worktrees that have
    /// none — an adopted one, or any of them after the daemon restarted.
    SpawnTerminal,
    Adopt,
    Release,
    OpenEditor,
    Confirm,
    Cancel,
    Close,
    Detach,
}

/// One binding, as both the router and the help overlay see it.
#[derive(Debug, Clone, Copy)]
pub struct Binding {
    /// Whether `^g` must precede it.
    pub prefixed: bool,
    pub key: KeyCode,
    pub modifiers: KeyModifiers,
    pub action: Action,
    /// How the status bar and help overlay label it, e.g. `"sessions"`.
    pub label: &'static str,
}

const fn b(prefixed: bool, key: KeyCode, action: Action, label: &'static str) -> Binding {
    Binding {
        prefixed,
        key,
        modifiers: KeyModifiers::NONE,
        action,
        label,
    }
}

/// Bindings available on every screen, per SPEC §3.2. All prefixed: they must
/// work while a pty has focus, which is the whole point of the prefix.
const GLOBAL: &[Binding] = &[
    b(true, KeyCode::Char('/'), Action::OpenPalette, "palette"),
    b(true, KeyCode::Char('s'), Action::OpenPicker, "sessions"),
    b(true, KeyCode::Char('d'), Action::OpenDiff, "diff"),
    b(true, KeyCode::Char('i'), Action::OpenShell, "shell"),
    b(true, KeyCode::Char('n'), Action::PrefillNew, "new"),
    b(true, KeyCode::Char('X'), Action::EndSession, "end"),
    b(true, KeyCode::Char('S'), Action::Snapshot, "snapshot"),
    b(true, KeyCode::Char('?'), Action::Help, "help"),
    b(true, KeyCode::Char('q'), Action::Quit, "quit"),
];

/// Dash bindings, per SPEC §3.3. The pane-focus and visibility keys are
/// prefixed; list movement is not, because a list is not a pty.
const DASH: &[Binding] = &[
    b(true, KeyCode::Tab, Action::CycleFocus, "pane"),
    b(true, KeyCode::BackTab, Action::CycleFocusBack, "pane back"),
    b(true, KeyCode::Char('1'), Action::TogglePane(1), "hide"),
    b(true, KeyCode::Char('2'), Action::TogglePane(2), "hide"),
    b(true, KeyCode::Char('3'), Action::TogglePane(3), "hide"),
    b(true, KeyCode::Char('a'), Action::Adopt, "adopt"),
    b(true, KeyCode::Char('r'), Action::Release, "release"),
    b(true, KeyCode::Char('o'), Action::OpenEditor, "editor"),
    b(false, KeyCode::Up, Action::MoveUp, "move"),
    b(false, KeyCode::Down, Action::MoveDown, "move"),
    // The terminal pane's own keys. Prefixed because the pty takes the
    // unprefixed ones — that is the whole point of §3.1's rule.
    b(true, KeyCode::Up, Action::ScrollUp, "scroll"),
    b(true, KeyCode::Down, Action::ScrollDown, "scroll"),
    b(true, KeyCode::Enter, Action::SpawnTerminal, "terminal"),
];

/// Overlay bindings, per SPEC §3.4. Unprefixed throughout — the picker, diff
/// and end-session screens are not ptys, so they take keys directly.
const PICKER: &[Binding] = &[
    b(false, KeyCode::Up, Action::MoveUp, "move"),
    b(false, KeyCode::Down, Action::MoveDown, "move"),
    b(false, KeyCode::Enter, Action::Confirm, "resume"),
    b(false, KeyCode::Char('d'), Action::Detach, "detach"),
    b(false, KeyCode::Char('c'), Action::Close, "close"),
    b(false, KeyCode::Char('X'), Action::EndSession, "end"),
    b(false, KeyCode::Esc, Action::Cancel, "back"),
];

const DIFF: &[Binding] = &[
    b(false, KeyCode::Up, Action::MoveUp, "file"),
    b(false, KeyCode::Down, Action::MoveDown, "file"),
    b(false, KeyCode::Esc, Action::Cancel, "close"),
];

const END: &[Binding] = &[
    b(false, KeyCode::Enter, Action::Confirm, "remove everything"),
    b(false, KeyCode::Esc, Action::Cancel, "cancel"),
];

/// The scratch shell is a pty, so it has no unprefixed bindings at all — `esc`
/// belongs to the shell running inside it, not to grove.
const SHELL: &[Binding] = &[];

const PALETTE: &[Binding] = &[
    b(false, KeyCode::Up, Action::MoveUp, "move"),
    b(false, KeyCode::Down, Action::MoveDown, "move"),
    b(false, KeyCode::Enter, Action::Confirm, "run"),
    b(false, KeyCode::Esc, Action::Cancel, "close"),
];

/// Every binding live on a screen, globals last so a screen may shadow one.
pub fn bindings(screen: Screen) -> impl Iterator<Item = &'static Binding> {
    let local = match screen {
        Screen::Dash => DASH,
        Screen::Palette => PALETTE,
        Screen::Picker => PICKER,
        Screen::Diff => DIFF,
        Screen::Shell => SHELL,
        Screen::EndSession => END,
    };
    local.iter().chain(GLOBAL.iter())
}

/// What the router decided to do with a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Routed {
    /// Grove acts.
    Act(Action),
    /// Send these bytes to the focused pty.
    ToPty(KeyEvent),
    /// Waiting for the second key of a chord.
    PrefixPending,
    /// `^g` followed by nothing bound. Deliberately not an error: a mistyped
    /// chord should cost nothing.
    Unbound,
    /// Text for the focused input. A screen that accepts typing gets its
    /// unbound characters as content rather than having them discarded: the
    /// palette is a command line, and "no binding matched" is the normal case
    /// there, not a dead end.
    Text(char),
    /// The key means nothing here, there is no pty to give it to, and no input
    /// to type it into.
    Ignored,
}

/// Routes keys according to the prefix rule.
pub struct Router {
    prefix_at: Option<Instant>,
}

impl Router {
    pub fn new() -> Self {
        Self { prefix_at: None }
    }

    /// True while a chord is half-entered, so the status bar can say so.
    pub fn prefix_pending(&self) -> bool {
        self.prefix_at.is_some()
    }

    pub fn route(&mut self, screen: Screen, focus: Focus, key: KeyEvent) -> Routed {
        self.route_at(screen, focus, key, Instant::now())
    }

    /// Time is a parameter so the timeout is testable without sleeping.
    pub fn route_at(
        &mut self,
        screen: Screen,
        focus: Focus,
        key: KeyEvent,
        now: Instant,
    ) -> Routed {
        // Expire a stale chord before interpreting anything, or a `^g` from
        // minutes ago would silently eat the next keystroke.
        if let Some(at) = self.prefix_at
            && now.duration_since(at) >= PREFIX_TIMEOUT
        {
            self.prefix_at = None;
        }

        let pending = self.prefix_at.take().is_some();

        if pending {
            // `^g ^g` sends a literal `^g` to the pty — the standard escape for
            // a prefix key, without which a program inside the terminal could
            // never receive one.
            if is_prefix(&key) {
                return if pty_holds_the_keyboard(screen, focus) {
                    Routed::ToPty(key)
                } else {
                    Routed::Ignored
                };
            }
            return match lookup(screen, key, true) {
                Some(action) => Routed::Act(action),
                // An unbound chord is a no-op, never a crash, and never passed
                // through to the pty — the user meant to address grove.
                None => Routed::Unbound,
            };
        }

        if is_prefix(&key) {
            self.prefix_at = Some(now);
            return Routed::PrefixPending;
        }

        // The rule itself: a pty takes everything not addressed to grove —
        // but only where a pty is what has focus. An overlay *is* the focus
        // while it is open, so consulting the dash's pane focus there would
        // send every keystroke to a terminal the user cannot see. That is
        // latent until #18 allows the screen to change, and silent when it
        // lands.
        // The scratch shell *is* a pty — there is no list on that screen to
        // hold focus — so it takes keys whatever `focus` happens to say. It can
        // say anything: `focus` is the dash's pane selection, and opening the
        // shell does not change it. Gating the shell on it stranded every key
        // in `Ignored`, which is the same mistake as M4 pointing the other way.
        if pty_holds_the_keyboard(screen, focus) {
            return Routed::ToPty(key);
        }

        match lookup(screen, key, false) {
            Some(action) => Routed::Act(action),
            // An unbound printable key on a text-entry screen is content.
            None => match (screen.takes_text(), key.code) {
                (true, KeyCode::Char(c))
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    Routed::Text(c)
                }
                _ => Routed::Ignored,
            },
        }
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a pty is what the keyboard is talking to.
///
/// Two callers ask this — the main rule and the `^g ^g` literal escape — and
/// they must agree. They did not: the escape asked `focus` alone, so in the
/// scratch shell, where `focus` still holds whichever dash pane was selected,
/// the one keystroke that exists to reach the program inside the pty was the
/// one keystroke that could not. One definition, so the next caller cannot
/// drift either.
fn pty_holds_the_keyboard(screen: Screen, focus: Focus) -> bool {
    // The scratch shell *is* a pty — there is no list on that screen to hold
    // focus — so it takes keys whatever `focus` happens to say. An overlay, by
    // contrast, owns the keyboard while it is open, so the dash's pane focus
    // must not be consulted there at all.
    screen.is_pty_screen() || (screen.pty_can_hold_focus() && focus.is_pty())
}
fn is_prefix(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('g') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn lookup(screen: Screen, key: KeyEvent, prefixed: bool) -> Option<Action> {
    bindings(screen)
        .find(|bind| {
            bind.prefixed == prefixed && key_matches(bind, key) && modifiers_match(bind, key)
        })
        .map(|bind| bind.action)
}

/// Compare the modifiers that distinguish a binding, ignoring SHIFT on a
/// character key.
///
/// A terminal that reports modifiers sends `SHIFT` alongside an uppercase
/// character, and the shift is already expressed by the character itself.
/// Matching exactly meant `^g X`, `^g S` and `^g ?` worked on terminals that
/// omit the flag and were silently dead on those that send it — the kind of
/// bug that looks like a broken keyboard.
/// Whether the key itself matches, allowing for terminals that spell one key
/// two ways.
fn key_matches(bind: &Binding, key: KeyEvent) -> bool {
    if bind.key == KeyCode::BackTab {
        key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
    } else {
        bind.key == key.code
    }
}

fn modifiers_match(bind: &Binding, key: KeyEvent) -> bool {
    const DISTINGUISHING: KeyModifiers = KeyModifiers::CONTROL
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::SUPER);
    let significant = key.modifiers & DISTINGUISHING;
    let declared = bind.modifiers & DISTINGUISHING;
    if matches!(key.code, KeyCode::Char(_)) {
        significant == declared
    } else if bind.key == KeyCode::BackTab {
        // Kitty-protocol terminals report shift-tab as Tab + SHIFT rather than
        // BackTab, so an exact comparison loses `^g shift-tab` on them — the
        // same family as the dead uppercase bindings.
        key.modifiers.contains(KeyModifiers::SHIFT) || key.modifiers == bind.modifiers
    } else {
        // Otherwise SHIFT is meaningful on a non-character key.
        key.modifiers == bind.modifiers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn code(k: KeyCode) -> KeyEvent {
        KeyEvent::new(k, KeyModifiers::NONE)
    }

    #[test]
    fn a_pty_takes_every_key_that_is_not_addressed_to_grove() {
        let mut r = Router::new();
        for k in [key('s'), key('d'), key('X'), key('/'), code(KeyCode::Up)] {
            assert_eq!(
                r.route(Screen::Dash, Focus::Terminal, k),
                Routed::ToPty(k),
                "a focused pty must receive {k:?}"
            );
        }
    }

    #[test]
    fn a_list_takes_arrows_without_a_prefix() {
        // There is no pty to send them to, so requiring `^g ↓` here would be
        // prefix noise for no benefit.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Dash, Focus::Worktrees, code(KeyCode::Down)),
            Routed::Act(Action::MoveDown)
        );
    }

    #[test]
    fn grove_bindings_need_the_prefix_when_a_pty_has_focus() {
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, ctrl('g')),
            Routed::PrefixPending
        );
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, key('s')),
            Routed::Act(Action::OpenPicker)
        );
    }

    #[test]
    fn a_literal_prefix_reaches_the_pty_via_the_chord() {
        // Without this a program inside the terminal could never receive `^g`.
        let mut r = Router::new();
        r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, ctrl('g')),
            Routed::ToPty(ctrl('g'))
        );
    }

    #[test]
    fn a_literal_prefix_reaches_the_scratch_shell_too() {
        // The scratch shell *is* a pty, and `focus` there is whatever the dash
        // pane selection happened to be — so the literal-prefix escape has to
        // ask the same question the main rule asks, not `focus` alone. Asking
        // `focus` sent `^g ^g` to `Ignored` and left no way to type a literal
        // `^g` into the shell.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Shell, Focus::Worktrees, ctrl('g')),
            Routed::PrefixPending
        );
        assert_eq!(
            r.route(Screen::Shell, Focus::Worktrees, ctrl('g')),
            Routed::ToPty(ctrl('g'))
        );
    }

    #[test]
    fn an_unbound_chord_is_a_no_op_not_a_crash() {
        let mut r = Router::new();
        r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, key('%')),
            Routed::Unbound
        );
        // And the router is usable afterwards rather than stuck mid-chord.
        assert!(!r.prefix_pending());
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, key('x')),
            Routed::ToPty(key('x'))
        );
    }

    #[test]
    fn an_unbound_chord_is_not_leaked_to_the_pty() {
        // The user addressed grove and mistyped. Passing the key through would
        // have the shell act on a keystroke meant for grove.
        let mut r = Router::new();
        r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
        assert_ne!(
            r.route(Screen::Dash, Focus::Terminal, key('%')),
            Routed::ToPty(key('%'))
        );
    }

    #[test]
    fn a_stale_chord_expires_rather_than_eating_the_next_key() {
        let mut r = Router::new();
        let t0 = Instant::now();
        assert_eq!(
            r.route_at(Screen::Dash, Focus::Terminal, ctrl('g'), t0),
            Routed::PrefixPending
        );
        let later = t0 + PREFIX_TIMEOUT + Duration::from_millis(1);
        // `s` is a grove binding; after the timeout it must reach the pty
        // instead, or a forgotten `^g` silently swallows a keystroke.
        assert_eq!(
            r.route_at(Screen::Dash, Focus::Terminal, key('s'), later),
            Routed::ToPty(key('s'))
        );
    }

    #[test]
    fn focus_changes_what_the_arrows_do() {
        // The source mock drew a focus ring but always moved the worktree list.
        // Focus must be functional, so routing has to depend on it.
        let mut r = Router::new();
        let down = code(KeyCode::Down);
        assert_eq!(
            r.route(Screen::Dash, Focus::Repos, down),
            Routed::Act(Action::MoveDown)
        );
        assert_eq!(
            r.route(Screen::Dash, Focus::Worktrees, down),
            Routed::Act(Action::MoveDown)
        );
        // The same key on the pty pane is not a grove action at all.
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, down),
            Routed::ToPty(down)
        );
    }

    #[test]
    fn focus_cycles_both_ways_through_every_pane() {
        let mut f = Focus::Repos;
        for _ in 0..3 {
            f = f.next();
        }
        assert_eq!(f, Focus::Repos, "three steps must return to the start");
        assert_eq!(Focus::Repos.previous(), Focus::Terminal);
        assert_eq!(Focus::Repos.next().previous(), Focus::Repos);
    }

    #[test]
    fn an_uppercase_binding_works_whether_or_not_the_terminal_reports_shift() {
        // Terminals disagree about sending SHIFT with an uppercase character.
        // Exact matching made `^g X` work on some and silently die on others.
        let mut r = Router::new();
        for mods in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
            assert_eq!(
                r.route(
                    Screen::Dash,
                    Focus::Terminal,
                    KeyEvent::new(KeyCode::Char('X'), mods)
                ),
                Routed::Act(Action::EndSession),
                "^g X must work with modifiers {mods:?}"
            );
        }
    }

    #[test]
    fn shift_still_distinguishes_non_character_keys() {
        // shift-tab is not tab; the SHIFT relaxation must not reach those.
        let mut r = Router::new();
        r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, code(KeyCode::BackTab)),
            Routed::Act(Action::CycleFocusBack)
        );
    }

    #[test]
    fn the_scratch_shell_takes_keys_whatever_the_stale_pane_focus_says() {
        // `focus` is the dash's pane selection and opening the shell does not
        // change it, so it can still say Repos. The shell is a pty regardless.
        let mut r = Router::new();
        for stale in [Focus::Repos, Focus::Worktrees, Focus::Terminal] {
            assert_eq!(
                r.route(Screen::Shell, stale, key('n')),
                Routed::ToPty(key('n')),
                "the shell must take keys with focus {stale:?}"
            );
        }
        // And the prefix still reaches grove from inside it.
        r.route(Screen::Shell, Focus::Repos, ctrl('g'));
        assert_eq!(
            r.route(Screen::Shell, Focus::Repos, key('s')),
            Routed::Act(Action::OpenPicker)
        );
    }

    #[test]
    fn shift_tab_works_whether_reported_as_backtab_or_tab_plus_shift() {
        // Kitty-protocol terminals spell it the second way.
        let mut r = Router::new();
        for ev in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
            assert_eq!(
                r.route(Screen::Dash, Focus::Terminal, ev),
                Routed::Act(Action::CycleFocusBack),
                "shift-tab must work as {ev:?}"
            );
        }
        // Plain tab is still forward, not back.
        r.route(Screen::Dash, Focus::Terminal, ctrl('g'));
        assert_eq!(
            r.route(Screen::Dash, Focus::Terminal, code(KeyCode::Tab)),
            Routed::Act(Action::CycleFocus)
        );
    }

    #[test]
    fn an_overlay_owns_the_keyboard_even_when_a_pane_held_a_pty() {
        // The user opens the palette from the terminal pane. If routing still
        // consulted the dash's focus, every keystroke would vanish into a
        // terminal they can no longer see.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Palette, Focus::Terminal, code(KeyCode::Enter)),
            Routed::Act(Action::Confirm)
        );
        assert_eq!(
            r.route(Screen::Picker, Focus::Terminal, code(KeyCode::Esc)),
            Routed::Act(Action::Cancel)
        );
        assert_eq!(
            r.route(Screen::Palette, Focus::Terminal, key('n')),
            Routed::Text('n')
        );
        // The scratch shell is a real pty, so it still takes keys.
        assert_eq!(
            r.route(Screen::Shell, Focus::Terminal, key('n')),
            Routed::ToPty(key('n'))
        );
    }

    #[test]
    fn the_palette_receives_typing_as_content() {
        // A command line's normal case is "no binding matched"; discarding
        // those keys would make it impossible to type a command into it.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Palette, Focus::Worktrees, key('n')),
            Routed::Text('n')
        );
        // Its own bindings still win over text.
        assert_eq!(
            r.route(Screen::Palette, Focus::Worktrees, code(KeyCode::Enter)),
            Routed::Act(Action::Confirm)
        );
        // And a chord still addresses grove rather than typing a letter.
        r.route(Screen::Palette, Focus::Worktrees, ctrl('g'));
        assert_eq!(
            r.route(Screen::Palette, Focus::Worktrees, key('s')),
            Routed::Act(Action::OpenPicker)
        );
    }

    #[test]
    fn screens_without_an_input_do_not_swallow_keys_as_text() {
        // The picker and the confirm screen have nothing to type into, so an
        // unbound key there is genuinely nothing.
        let mut r = Router::new();
        for screen in [Screen::Picker, Screen::Diff, Screen::EndSession] {
            assert_eq!(
                r.route(screen, Focus::Worktrees, key('z')),
                Routed::Ignored,
                "{screen:?} must not treat an unbound key as text"
            );
        }
    }

    #[test]
    fn overlays_take_keys_directly() {
        // The picker, diff and end-session screens are not ptys.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Picker, Focus::Worktrees, code(KeyCode::Enter)),
            Routed::Act(Action::Confirm)
        );
        assert_eq!(
            r.route(Screen::EndSession, Focus::Worktrees, code(KeyCode::Esc)),
            Routed::Act(Action::Cancel)
        );
    }

    #[test]
    fn the_scratch_shell_gives_esc_to_the_shell_not_to_grove() {
        // It is a pty: `esc` belongs to whatever is running inside it.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Shell, Focus::Terminal, code(KeyCode::Esc)),
            Routed::ToPty(code(KeyCode::Esc))
        );
    }

    #[test]
    fn every_screen_exposes_its_bindings_as_data() {
        // The status bar and help overlay render from this, so a screen with no
        // bindings at all would leave them with nothing to say.
        for screen in [
            Screen::Dash,
            Screen::Palette,
            Screen::Picker,
            Screen::Diff,
            Screen::Shell,
            Screen::EndSession,
        ] {
            let all: Vec<_> = bindings(screen).collect();
            assert!(!all.is_empty(), "{screen:?} exposes no bindings");
            assert!(
                all.iter().all(|b| !b.label.is_empty()),
                "{screen:?} has an unlabelled binding; the help overlay would render a blank"
            );
        }
    }

    #[test]
    fn a_screen_binding_shadows_a_global_one() {
        // The picker binds bare `d` to detach; globally `^g d` opens the diff.
        // They are different actions on the same letter, so this must assert
        // they stay different — an earlier version bound the picker's `d` to
        // OpenDiff and this test passed, blessing the bug instead of catching
        // it.
        let mut r = Router::new();
        assert_eq!(
            r.route(Screen::Picker, Focus::Worktrees, key('d')),
            Routed::Act(Action::Detach)
        );

        let mut r = Router::new();
        r.route(Screen::Picker, Focus::Worktrees, ctrl('g'));
        assert_eq!(
            r.route(Screen::Picker, Focus::Worktrees, key('d')),
            Routed::Act(Action::OpenDiff),
            "the global must still be reachable through the prefix"
        );
    }
}
