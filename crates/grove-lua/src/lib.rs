//! Lua runtime, `config.lua` evaluation, and the `grove` module.
//!
//! Shared by both binaries, but **sharing the crate must not share the
//! concerns**. Each process evaluates the config in its own VM and may only
//! name its own surface:
//!
//! - the TUI sees theme, keymaps, commands and columns;
//! - the daemon sees shell, paths, ignore globs and lifecycle events.
//!
//! The two surfaces are disjoint types. A Lua value cannot cross a process
//! boundary, which is what makes this split structural rather than a
//! convention.
//!
//! See `SPEC.md` §9 and §10. Implemented in issue #3.
