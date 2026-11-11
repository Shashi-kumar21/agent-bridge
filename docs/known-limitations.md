# Known limitations

Confirmed behaviour of the current `dev` build. Each item has a source anchor
and, where practical, a reproduction. This is the evidence list behind
[`roadmap.md`](roadmap.md); items are not planned work until the roadmap section
referenced next to them lands.

Reproductions assume a throwaway state directory so they do not disturb a real
daemon. On Windows the state directory does **not** isolate the daemon (see
§11), so also point the pipe at a throwaway name:

```bash
export AGENT_BRIDGE_HOME=/tmp/ab-audit        # bash / zsh
set AGENT_BRIDGE_HOME=C:\temp\ab-audit        # cmd.exe
$env:AGENT_BRIDGE_HOME = "C:\temp\ab-audit"   # PowerShell
$env:AGENT_BRIDGE_PIPE = "agent-bridge-audit" # PowerShell, Windows isolation
```

## 1. Message delivery is "written", never "delivered"

`sendmessage` writes bytes into the target's PTY and returns
`Sent { bytes, message_id, status }` (`daemon.rs:237-374`). The status records
what the daemon's write path did, not what the target did: the application may
be mid-render, waiting at a prompt, or not reading at all. A bounded queue and a
single writer per agent make the *write* orderly, but there is no retry and no
application-level acknowledgement, so `written` does not mean the request was
understood or executed.

```bash
agent-bridge new shell -- cmd.exe            # or /bin/sh
agent-bridge sendmessage shell "echo hi"     # "sent msg 1 to shell (written, ...)"
agent-bridge kill shell --force              # nothing observed the message
```

Roadmap §2 (application-level acknowledgement only where a receiving
integration can send one).

## 2. A blocked PTY write cannot be cancelled

Each agent has one writer thread and a bounded queue (`delivery.rs:294-378`), so
a slow target no longer blocks *other* senders' writes and memory stays bounded
(128 jobs / 2 MiB per agent by default). A caller still waits up to
`--write-timeout` (forever with `0`) for its own message. Admission waits
`--queue-timeout` (5 s, `0` fails fast) and a full queue returns
`failed { code: queue_full }`.

The remaining gap is the OS write itself: once `write_all` is inside the PTY
there is no portable way to cancel it. `--write-timeout` (5 s by default, `0`
waits forever) makes the caller return `unknown`; the writer keeps waiting and
amends the record to `written`/`failed` when the syscall returns
(`delivery.rs:443-482`, `daemon.rs:489-559`). A target that never drains its PTY
therefore keeps the in-flight byte reservation and the channel fills up behind
it until the agent exits or the daemon is restarted; new messages to that agent
eventually fail with `queue_full`.

Urgent jobs (Ctrl-C, `attach` keystrokes) use a separate channel and may pass a
queued message between writes; they never split a payload (`agent.rs:331-365`).

## 3. Large messages are not atomic at the terminal level

The writer serialises whole jobs, so two `sendmessage` calls cannot interleave
*bytes*. What is not guaranteed is that the terminal application reads the
message as one paste: a multi-line message is delivered as literal bytes plus
`\r`, which shells execute line by line, and TUI agents may interpret
intermediate lines as submissions. Bracketed paste is not used yet (Phase 2C).

```bash
agent-bridge sendmessage shell "echo one
echo two"
```

Roadmap §2 (multi-line handling, bracketed paste where supported, documented
fallback).

## 4. `read` is a line view of raw bytes, not a terminal screen

`read` strips ANSI sequences from the retained byte ring and returns the last N
lines (`buffer.rs:63-77`, `:85-125`). Cursor movement, erase-line/screen,
alternate screen and wrapping are discarded, so a full-screen TUI renders as a
puzzle of redraw fragments.

```bash
agent-bridge new tui --cwd . -- opencode     # any full-screen TUI
agent-bridge read tui --lines 40
```

Roadmap §6 (VT screen model and snapshot).

## 5. No byte offset cursor; `seq` counts chunks

`Read` returns the current `seq` (`protocol.rs:345-355`); `Attach --from-seq`
only decides whether to replay the *whole* retained buffer (`daemon.rs:707-708`).
Because `seq` increments per PTY chunk, not per byte, callers cannot resume from
an arbitrary offset, and a replay frame carries the newest `seq` for bytes that
may include older chunks.

```bash
agent-bridge --json read shell --lines 5     # note "seq"
agent-bridge attach shell --from-seq <seq>   # skips replay only when >= current
```

Roadmap §3 (`--since` byte offset, wrap detection, follow/stream).

## 6. `truncated` is latched for the life of the agent

Once the 256 KiB ring drops any byte (`buffer.rs:28-35`), `truncated` stays true
for that agent even if the output since then fits entirely
(`buffer.rs:57-59`). Consumers cannot tell whether the *requested window* was
truncated.

Roadmap §3 (cursor semantics that report what is actually available).

## 7. Exited agents are never removed; names cannot be reused

The agent map is only inserted into (`daemon.rs:232`); there is no removal
path. `listagents` keeps showing exited entries with their buffers and message
history, and `new` with that name fails with `agent '<name>' already exists`.

```bash
agent-bridge new demo -- cmd.exe             # /bin/sh on Unix
agent-bridge sendmessage demo "exit"         # or close the shell
agent-bridge listagents                       # demo: exited
agent-bridge new demo -- cmd.exe              # error: already exists
agent-bridge daemon restart                   # clears it, kills live agents too
```

Tracked as issue
[#24](https://github.com/Shashi-kumar21/agent-bridge/issues/24).

## 8. Daemon restart kills live agents and discards exited state

Buffers, message history, PTY handles and job objects are in-memory only
(`daemon.rs:87-93`). `daemon restart` stops the daemon, which sends Ctrl-C to
every agent, force-kills any that are still running after ~500 ms
(`daemon.rs:171-180`, `agent.rs:479-497`), and then exits; the next daemon
starts empty. There is no session adoption.

```bash
agent-bridge new keep -- cmd.exe
agent-bridge daemon restart
agent-bridge listagents                       # empty
```

Roadmap §7 (documented contract, health reporting; adoption is platform- and
architecture-dependent).

## 9. Daemon death only reaches the foreground process group on Unix

Closing the PTY master sends `SIGHUP` to the terminal's foreground process
group. A background job moved into its own process group survives a daemon
crash; `kill --force` still reaps it while the direct client is alive and, on
Linux, after it exited (PTY fd sweep). macOS has no `/proc` sweep, so the
after-exit case survives there.

```bash
agent-bridge new sh -- /bin/sh
agent-bridge sendmessage sh "set -m; sleep 600 &"
# kill -9 the daemon, then look for the sleep process
```

`SECURITY.md` known gaps; roadmap §7.

## 10. Agents inherit the daemon's full environment

The child gets everything the daemon has, plus `TERM` and
`AGENT_BRIDGE_AGENT` (`agent.rs:112-113`). Nothing is filtered, so any secret in
the daemon's environment is present in every agent.

```bash
export MY_SECRET=abc                  # set it before the daemon starts
agent-bridge daemon restart           # or stop/start, so the daemon inherits it
agent-bridge new envtest -- /bin/sh
agent-bridge sendmessage envtest 'echo "$MY_SECRET"'
agent-bridge read envtest --lines 5
```

Roadmap §4 (env allowlist with documented override).

## 11. On Windows the pipe name ignores `AGENT_BRIDGE_HOME`

The endpoint is `\\.\pipe\agent-bridge-<USERNAME>` unless `AGENT_BRIDGE_PIPE` is
set (`transport.rs:10-21`), while the socket path (and therefore the state
directory) follows `AGENT_BRIDGE_HOME` (`paths.rs:20-38`). Pointing a second CLI
at a different home therefore still talks to the first daemon, and two projects
cannot be isolated with the home variable alone.

```powershell
$env:AGENT_BRIDGE_HOME = "C:\temp\ab-a"; agent-bridge daemon start
$env:AGENT_BRIDGE_HOME = "C:\temp\ab-b"; agent-bridge listagents  # same daemon
```

Roadmap §4 (document the endpoints, add per-instance isolation).

## 12. Any same-user process can drive any agent

Access control is the endpoint itself: socket `0600` in a `0700` directory, or a
pipe DACL for owner/`SYSTEM`/Administrators (`transport.rs:79-98`,
`transport.rs:41`). There is no caller authentication, no per-session policy and
no audit trail; `from` is an unauthenticated label (`daemon.rs:237-374`).

```bash
agent-bridge kill dev --force        # any local process can do this
```

Roadmap §4 (threat model, policy, identity, audit).

## 13. Output retention ends at 256 KiB and drops slow subscribers

Retention is a fixed 256 KiB ring (`buffer.rs:6`); there is no way to raise it.
An attach client that cannot keep up with 1024 queued events is disconnected
silently (`agent.rs:32`, `:423-433`, `:442-450`) - the CLI sees the stream
close, not an error.

Roadmap §3 (configurable sizes, explicit drop signalling).

## 14. Message history is per-agent and bounded at 500

`record_message` keeps the newest 500 entries (`agent.rs:21`, `:386-390`), and
the global id-indexed log keeps 2000 (`daemon.rs:25`, `:29-77`). Both are
in-memory and vanish with the daemon. Ids are unique only for one daemon
lifetime, so `messagestatus` returns `not_found` after a restart even though the
id was valid before; there is no persistence.

Roadmap §7 (optional metadata persistence).

## 15. Windows: a process forked before job assignment is outside the job

The job object is assigned immediately after `CreateProcessW`
(`reaper.rs`, `install`), so a child forked in that window is not in the job.
The job also does not set `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so programs that ask
for `CREATE_BREAKAWAY_FROM_JOB` fail instead of escaping.

`SECURITY.md` Windows caveats; no roadmap item (platform constraint).

## 16. Terminal size is fixed unless an attach client resizes it

`new --cols/--rows` sets the initial PTY size, and only an interactive `attach`
watches for local resize events and forwards `Resize`
(`crates/ab-cli/src/attach.rs`). A headless agent keeps its spawn size, so TUI
output can wrap differently from what the target expects.

Roadmap §6 (resize handling in the screen model).
