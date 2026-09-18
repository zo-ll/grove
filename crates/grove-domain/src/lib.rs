//! Pure domain types for Grove. No I/O of any kind.
//!
//! Both lanes build on this crate, so it stays boring on purpose: no git, no
//! filesystem, no async, no runtime. Adding a dependency that performs I/O is a
//! boundary violation and CI rejects it.
//!
//! Types live here so the UI and the daemon agree on what a session, worktree
//! and ownership *are* without either owning the definition.
//!
//! See `SPEC.md` §2. Implemented in issue #1.
