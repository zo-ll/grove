//! The Grove TUI.
//!
//! Depends on `grove-domain`, `grove-proto` and `grove-lua` only. It never
//! links a git crate, never spawns a pty, and never touches repositories on
//! disk — everything about repos arrives over the protocol.
//!
//! Terminal *rendering* crates (`vt100`, `tui-term`) are allowed and expected;
//! terminal *spawning* (`portable-pty`) is not.
//!
//! See `SPEC.md` §3 and §4. Implemented in issues #14 through #30.

fn main() {
    eprintln!("grove: not implemented yet — see issues #13 through #30");
}
