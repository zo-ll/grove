# groved resource notes

Each terminal owns one `vt100::Parser`. The parser's scrollback is constructed
with the configured line limit and uses a bounded `VecDeque`; live output has no
subscriber queue while no client is attached, and attached-client queues hold at
most 64 chunks before the client must reattach. The only idle retained output is the parser
scrollback plus its visible grid.

On 2026-09-19, eight idle `/bin/sh` PTYs at 24×80 with 1,000 scrollback lines
added 2.4 MiB RSS in a debug test process on Linux. Growth is linear
in terminal count and the configured `(rows + scrollback) × cols` cell bound.

Session effects are centralized in `SessionOrchestrator`. Opening fetches only
session members and starts missing owned-worktree shells; detach keeps those
PTYs; close kills them without touching Git; and confirmed end is the only
orchestration path that calls `git worktree remove`. `end_plan` reports dirty
file counts and foreground commands before that confirmed call. Successfully
removed worktrees are forgotten from state one at a time, so a partial Git
failure leaves the session retryable and never expands the removal set beyond
its recorded ownership.
