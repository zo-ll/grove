# grove-git

This crate is the only place Grove invokes Git. Its public operations mirror the
closed read/write tables in `SPEC.md` section 7.

Repository discovery deliberately continues below a repository root. This makes
independently nested repositories visible. A `.git` file is treated like a `.git`
directory, so an initialized submodule is also returned as a repository. Git
metadata directories and symlinked directories are never traversed.
Unreadable directories beneath the scan root are skipped so one inaccessible
subtree cannot suppress every otherwise usable repository; an unreadable root is
still an error.

Diff patches currently invoke Git once for the numstat and once per changed file.
This keeps each patch paired with its path without parsing patch headers; reducing
the subprocess count is a future optimization if large diffs show it matters.

The `discovery_of_thirty_repositories_is_startup_scale` test records the discovery
time and enforces a ten-second ceiling. On 2026-09-19, a debug build on the initial
development machine scanned 30 initialized repositories in 39 ms (repository
creation is outside the measurement). The deliberately generous CI ceiling catches
blocking regressions without turning scheduler noise into failures.
