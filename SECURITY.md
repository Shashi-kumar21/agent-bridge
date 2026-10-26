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
    Two caveats: the assignment happens right after `CreateProcessW`, so a
    process forked in that brief window is outside the job; and the
    job does not set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so a child that asks for
    `CREATE_BREAKAWAY_FROM_JOB` now fails with `ERROR_ACCESS_DENIED` instead of
    escaping (some installers/launchers do this).
  * **Unix** - `portable-pty` makes the agent a session leader with the PTY as
    its controlling terminal. Closing the master on daemon death makes the
    kernel send `SIGHUP` to the foreground process group.
  * If the OS refuses the link (for example a restricted job object on a locked
    down host), the daemon logs a warning and spawns the agent anyway; refusing
    to run a terminal would be worse than running it without the safety net.
* `agent-bridge kill --force` tears the agent's process tree down while the
  daemon stays up: Windows drops the job handle (the kernel terminates the job),
  Unix sends `SIGKILL` to the agent's process group. On Unix that reaches the
  agent and every process that stayed in its group - the normal case for a CLI
  agent and the tool subprocesses it starts. An interactive shell with job
  control moves each job into its own process group, so those jobs survive
  `kill --force` (see the known gaps). `kill` without `--force` still tries
  Ctrl-C first and then only terminates the PTY client, so children of a
  stubborn agent survive that path by design.

## Known gaps (tracked, not yet implemented)

* Unix job-control descendants: an interactive shell (`bash`, `zsh`) puts every
  job in its own process group, and `setsid` / `nohup` can leave the session
  entirely. `kill --force` only signals the agent's own group, so such
  processes can outlive both a daemon kill and `kill --force`. Following them
  needs process/session enumeration, which this daemon does not do; the Windows
  job object is the only mechanism here that covers a whole tree.
  `crates/ab-cli/tests/orphan_teardown.rs` exercises the full-tree case on
  Windows and the process-group case on Unix (with a non-interactive shell).
* Running agents are not re-adopted after a daemon restart.
* No optional token handshake for shared machines.
* Denial for a *different* OS user is not exercised by CI yet; only the
  descriptor's validity and same-user connectivity are tested.

