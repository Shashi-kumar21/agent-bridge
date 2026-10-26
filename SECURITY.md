# Security policy

## Supported versions

The latest commit on `main` is the only supported version.

## Reporting a vulnerability

Please email the maintainer instead of opening a public issue. Include:

* affected version / commit,
* reproduction steps,
* impact (local privilege boundary, message injection, DoS, ...).

You should receive a response within 72 hours.

## Threat model

`agent-bridge` is a **local** tool:

* The daemon listens only on a local endpoint (filesystem socket on Unix,
  named pipe on Windows). It never binds a network interface.
* Anyone running as the same OS user can talk to the daemon by design; the
  security boundary is the OS user, not the socket.
* Endpoint hardening on startup:
  * Unix: state directory `0700`, socket `0600` (owner only). A stale socket is
    only removed after proving no daemon answers on it.
  * Windows: the named pipe is created with an explicit DACL,
    `D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)` - owner, `SYSTEM` and
    Administrators only, inheritance disabled. If the descriptor cannot be
    built the daemon falls back to the OS default and logs a warning.
* Messages are written verbatim into another agent's PTY. Treat `sendmessage`
  input as code execution in that terminal - that is the point of the tool.
* Output buffers are kept in memory only (256 KiB per agent) and are never
  written to disk unless the daemon log is enabled.
* Agent lifetime is tied to the daemon, so a crashed or force-killed daemon does
  not leave hidden terminals behind (`crates/ab-core/src/reaper.rs`):
  * **Windows** - each agent is assigned to an anonymous job object with
    `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the daemon exits, crashes, or is
    terminated from Task Manager, the kernel kills the agent and every
    descendant in the job, including processes that opened their own console.
  * **Unix** - `portable-pty` makes the agent a session leader with the PTY as
    its controlling terminal. Closing the master on daemon death makes the
    kernel send `SIGHUP` to the foreground process group.
  * If the OS refuses the link (for example a restricted job object on a locked
    down host), the daemon logs a warning and spawns the agent anyway; refusing
    to run a terminal would be worse than running it without the safety net.

## Known gaps (tracked, not yet implemented)

* Unix descendants that leave the PTY's foreground process group (background
  jobs, `setsid`, `nohup`) can outlive a daemon kill. Only the Windows job
  object covers the whole tree; `crates/ab-cli/tests/orphan_teardown.rs`
  exercises the full-tree case on Windows and the `SIGHUP` case on Unix.
* Running agents are not re-adopted after a daemon restart.
* No optional token handshake for shared machines.
* Denial for a *different* OS user is not exercised by CI yet; only the
  descriptor's validity and same-user connectivity are tested.

