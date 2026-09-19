# groved resource notes

Each terminal owns one `vt100::Parser`. The parser's scrollback is constructed
with the configured line limit and uses a bounded `VecDeque`; live output has no
subscriber queue while no client is attached, and attached-client queues hold at
most 64 chunks before the client must reattach. The only idle retained output is the parser
scrollback plus its visible grid.

On 2026-09-19, eight idle `/bin/sh` PTYs at 24×80 with 1,000 scrollback lines
added 2.4 MiB RSS in a debug test process on Linux. Growth is linear
in terminal count and the configured `(rows + scrollback) × cols` cell bound.
