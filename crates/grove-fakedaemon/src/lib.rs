//! A scripted `grove-proto` server serving deterministic fixtures.
//!
//! This exists so the UI lane can build and test every screen with no real
//! daemon and no git repositories on the machine, in states that are
//! impractical to conjure for real: a worktree both dirty and owned by another
//! session, a prune list with one row per disqualifying reason, a terminal with
//! a busy foreground process.
//!
//! It is the project's test harness, not a throwaway stub.
//!
//! Implemented in issue #13.
