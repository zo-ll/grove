//! User keymaps from `config.lua` (SPEC §10.3, issue #33).
//!
//! `grove.keymap("^g w", fn)` binds a key to Lua. Three things make that safe
//! enough to put in a render loop.
//!
//! **A collision is reported, not silently won.** A config that quietly takes
//! `^g s` from the session picker leaves the user with a key that used to work
//! and now does something else, and nothing on screen to explain it. The
//! override still happens — it is the user's config — but it is named at load.
//!
//! **A callback that throws disables itself.** §10.5: the registration is
//! dropped and reported once, rather than throwing on every keystroke.
//!
//! **A callback that runs too long is cut off.** User code runs inside the
//! TUI's own loop, so an accidental `while true do end` would freeze the
//! screen with no way back. mlua's instruction hook stops it.

use std::collections::HashMap;

use ratatui::crossterm::event::{KeyCode, KeyModifiers};

/// A key as `config.lua` writes it: `"^g w"`, `"^g tab"`, `"q"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub prefixed: bool,
    pub key: KeyCode,
}

/// Parse `"^g w"` into a chord.
///
/// Deliberately narrow: a spelling this does not understand is refused and
/// reported rather than guessed at, because a guessed binding is a key that
/// does something the user did not ask for.
pub fn parse(spelling: &str) -> Option<Chord> {
    let trimmed = spelling.trim();
    let (prefixed, rest) = match trimmed.strip_prefix("^g") {
        Some(rest) => (true, rest.trim()),
        None => (false, trimmed),
    };
    if rest.is_empty() {
        return None;
    }
    let key = match rest {
        "tab" => KeyCode::Tab,
        "shift-tab" => KeyCode::BackTab,
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        one if one.chars().count() == 1 => KeyCode::Char(one.chars().next()?),
        _ => return None,
    };
    Some(Chord { prefixed, key })
}

/// What happened while loading the user's keymaps, for the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The spelling was not understood, so nothing was bound.
    Unparsed(String),
    /// It took a key grove already uses. The user's wins — it is their config
    /// — but they are told which one they replaced.
    Overrides { key: String, was: &'static str },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparsed(key) => write!(f, "keymap {key:?} is not a key grove understands"),
            Self::Overrides { key, was } => write!(f, "keymap {key:?} now overrides {was}"),
        }
    }
}

/// The user's bindings, by chord.
#[derive(Debug, Default)]
pub struct UserKeys {
    /// Chord to the index of its callback in the runtime's registrations.
    bound: HashMap<Chord, usize>,
    /// Spellings, kept so the help overlay can show them as written.
    spellings: HashMap<Chord, String>,
    /// Registrations that threw and were disabled (§10.5).
    disabled: Vec<usize>,
}

impl UserKeys {
    /// Take the registrations, reporting what could not be bound and what was
    /// taken from grove.
    ///
    /// `built_in` answers what a chord already does, so the report can name
    /// it. The caller owns that question because the keymap tables are the
    /// caller's.
    pub fn load<F>(spellings: &[String], built_in: F) -> (Self, Vec<Note>)
    where
        F: Fn(Chord) -> Option<&'static str>,
    {
        let mut keys = Self::default();
        let mut notes = Vec::new();
        for (index, spelling) in spellings.iter().enumerate() {
            let Some(chord) = parse(spelling) else {
                notes.push(Note::Unparsed(spelling.clone()));
                continue;
            };
            if let Some(was) = built_in(chord) {
                notes.push(Note::Overrides {
                    key: spelling.clone(),
                    was,
                });
            }
            keys.bound.insert(chord, index);
            keys.spellings.insert(chord, spelling.clone());
        }
        (keys, notes)
    }

    /// The callback bound to a key, if one is and it has not disabled itself.
    pub fn bound(&self, prefixed: bool, key: KeyCode, modifiers: KeyModifiers) -> Option<usize> {
        // A user binding is a plain key: `^g W` is a different chord from
        // `^g w`, and that difference is already in the `KeyCode`.
        if modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::ALT) {
            return None;
        }
        let index = *self.bound.get(&Chord { prefixed, key })?;
        (!self.disabled.contains(&index)).then_some(index)
    }

    /// Stop offering a registration that threw.
    pub fn disable(&mut self, index: usize) {
        if !self.disabled.contains(&index) {
            self.disabled.push(index);
        }
    }

    /// Every live binding, as the user spelled it, for the help overlay.
    pub fn spellings(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self
            .bound
            .iter()
            .filter(|(_, index)| !self.disabled.contains(index))
            .filter_map(|(chord, _)| self.spellings.get(chord).map(String::as_str))
            .collect();
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(prefixed: bool, key: KeyCode) -> Chord {
        Chord { prefixed, key }
    }

    #[test]
    fn the_spellings_config_uses_are_understood() {
        assert_eq!(parse("^g w"), Some(chord(true, KeyCode::Char('w'))));
        assert_eq!(parse("^g tab"), Some(chord(true, KeyCode::Tab)));
        assert_eq!(parse("q"), Some(chord(false, KeyCode::Char('q'))));
        assert_eq!(parse("^g up"), Some(chord(true, KeyCode::Up)));
        assert_eq!(parse("  ^g   w  "), Some(chord(true, KeyCode::Char('w'))));
    }

    #[test]
    fn a_spelling_grove_does_not_understand_is_refused_not_guessed() {
        // A guessed binding is a key that does something the user did not ask
        // for, which is worse than one that does nothing and says why.
        assert_eq!(parse("ctrl-w"), None);
        assert_eq!(parse("^g"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("^g meta-x"), None);

        let (keys, notes) = UserKeys::load(&["ctrl-w".into()], |_| None);
        assert!(
            keys.bound(false, KeyCode::Char('w'), KeyModifiers::NONE)
                .is_none()
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].to_string().contains("not a key grove understands"));
    }

    #[test]
    fn taking_a_key_grove_uses_is_reported_and_still_taken() {
        // Acceptance: a user keymap overriding a built-in is reported at load.
        // It is their config, so it wins — but silently winning leaves a key
        // that used to work doing something else with nothing to explain it.
        let (keys, notes) = UserKeys::load(&["^g s".into()], |chord| {
            (chord.key == KeyCode::Char('s') && chord.prefixed).then_some("sessions")
        });
        assert_eq!(notes.len(), 1);
        let note = notes[0].to_string();
        assert!(note.contains("^g s"), "{note}");
        assert!(note.contains("sessions"), "{note}");
        assert_eq!(
            keys.bound(true, KeyCode::Char('s'), KeyModifiers::NONE),
            Some(0),
            "the user's binding still wins"
        );
    }

    #[test]
    fn a_binding_that_threw_stops_being_offered() {
        // §10.5: a throwing callback disables that registration rather than
        // throwing again on every keystroke.
        let (mut keys, _) = UserKeys::load(&["^g w".into()], |_| None);
        assert_eq!(
            keys.bound(true, KeyCode::Char('w'), KeyModifiers::NONE),
            Some(0)
        );
        keys.disable(0);
        assert!(
            keys.bound(true, KeyCode::Char('w'), KeyModifiers::NONE)
                .is_none(),
            "a disabled binding is not offered again"
        );
        assert!(keys.spellings().is_empty(), "nor listed in help");
    }

    #[test]
    fn a_modified_key_is_not_a_user_chord() {
        // `^g w` is the prefix and a plain letter. A ctrl- or alt-modified key
        // arriving here is something else, and matching it would bind a key
        // the user did not write.
        let (keys, _) = UserKeys::load(&["^g w".into()], |_| None);
        assert!(
            keys.bound(true, KeyCode::Char('w'), KeyModifiers::CONTROL)
                .is_none()
        );
    }

    #[test]
    fn help_sees_the_user_bindings_as_written() {
        let (keys, _) = UserKeys::load(&["^g w".into(), "q".into()], |_| None);
        assert_eq!(keys.spellings(), vec!["^g w", "q"]);
    }
}
