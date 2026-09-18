//! The Grove daemon: owns every pty, survives the TUI exiting.
//!
//! Never depends on `ratatui`, `crossterm` or `tui-term`. Rendering is not its
//! concern; it serves state over `grove-proto`.
//!
//! See `SPEC.md` §8. Implemented in issues #10, #11 and #12.

fn main() {
    eprintln!("groved: not implemented yet — see issues #10, #11, #12");
}
