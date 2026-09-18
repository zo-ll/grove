//! All git access. Backend lane only — the TUI never links this crate.
//!
//! The read and write operations Grove performs are enumerated in `SPEC.md` §7
//! and that list is closed: if git cannot answer something, Grove does not
//! display it.
//!
//! Implemented in issues #4 and #5.
