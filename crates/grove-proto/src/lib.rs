//! Wire protocol between `grove` (TUI) and `groved` (daemon).
//!
//! Every message crossing the unix socket is defined here, along with framing
//! and version negotiation. The protocol is versioned from the first commit so
//! a stale daemon is detected rather than misparsed.
//!
//! See `SPEC.md` §8. Implemented in issue #2.
