//! Session persistence. Backend lane only.
//!
//! Holds only what git cannot know: session name, membership, ownership and
//! state. Git stays authoritative for everything it can answer, so this store
//! can never contradict reality — delete it and you lose grouping, never work.
//!
//! See `SPEC.md` §6. Implemented in issues #6 and #7.
