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

## 1. Message delivery is "written" unless the receiver acked it

`sendmessage` writes bytes into the target's PTY and returns
`Sent { bytes, message_id, status }` (`daemon.rs:241-385`). The status records
what the daemon's write path did, not what the target did: the application may
be mid-render, waiting at a prompt, or not reading at all. A bounded queue and a
single writer per agent make the *write* orderly, but there is no retry, so
`written` does not mean the request was understood or executed.

```bash
agent-bridge new shell -- cmd.exe            # or /bin/sh
agent-bridge sendmessage shell "echo hi"     # "sent msg 1 to shell (written, ...)"
agent-bridge kill shell --force              # nothing observed the message
```

`--require-ack` adds one explicit step: after the write the status becomes
`awaiting_ack`, and it only becomes `acknowledged` when the receiving
integration sends `agent-bridge ack <target> <id>` (`Request::Ack`). An
integration finds its ids with `pending <target>`; a message it never asks
about stays `awaiting_ack` forever. An ack is opt-in and unauthenticated: it
reports that the integration saw the message, not that the application
understood or completed the request, and any process that can reach the daemon
socket could claim one. Ids are per daemon lifetime, so an ack after a restart
fails with `message not found`.

```bash
agent-bridge sendmessage dev --require-ack "run the suite"
agent-bridge pending dev                     # awaiting_ack, id 7
agent-bridge ack dev 7 --note "suite finished"
```

Roadmap §2 (application-level acknowledgement only where a receiving
integration can send one).

## 2. A blocked PTY write cannot be cancelled

Each agent has one writer thread and a bounded queue (`delivery.rs:334-419`), so
a slow target no longer blocks *other* senders' writes and memory stays bounded
(128 jobs / 2 MiB per agent by default). A caller still waits up to
`--write-timeout` (forever with `0`) for its own message. Admission waits
`--queue-timeout` (5 s, `0` fails fast) and a full queue returns
`failed { code: queue_full }`.

The remaining gap is the OS write itself: once `write_all` is inside the PTY
there is no portable way to cancel it. `--write-timeout` (5 s by default, `0`
waits forever) makes the caller return `unknown`; the writer keeps waiting and
amends the record to `written`/`failed` when the syscall returns
(`delivery.rs:483-499`, `daemon.rs:501-530`, `:676-730`). A target that never
drains its PTY therefore keeps the in-flight byte reservation and the channel
fills up behind it until the agent exits or the daemon is restarted; new
messages to that agent
eventually fail with `queue_full`.

Urgent jobs (Ctrl-C, `attach` keystrokes) use a separate channel and may pass a
queued message between writes; they never split a payload (`agent.rs:416-426`).

## 3. Multi-line delivery depends on the target application

The writer serialises whole jobs, so two `sendmessage` calls cannot interleave
*bytes*. For multi-line text, `--paste auto` (default) wraps the message in
`ESC[200~`..`ESC[201~` only when the target announced bracketed paste
(`ESC[?2004h` on its output). A target that announced it receives the block as
one paste plus one `\r`; `--paste plain` always sends raw bytes, which shells
execute line by line and TUI agents may interpret as separate submissions.

```bash
agent-bridge sendmessage shell --paste plain "echo one
echo two"
agent-bridge sendmessage tui "line one
line two"
```

Limits of the detection: the marker must appear in the PTY output while the
agent is alive (the scan is on the live stream, not a retained snapshot), an
application that silently supports bracketed paste needs `--paste bracketed`,
and a target that prints the markers as literal text needs `--paste plain`.
`listagents --json` reports the current state as `bracketed_paste`, and the
detail block printed by `new` shows `paste : bracketed|plain`. A scripted
`new` followed immediately by `sendmessage` can reach the target before it has
printed the marker; the first multi-line message then falls back to plain. Wait
for the prompt, or use `--paste bracketed`.

The state is sampled when the request is handled
(`daemon.rs:253-261`), not when the writer finally reaches the message. With a
slow queue the target may have disabled paste in between (readline does this
while a command runs), and the wrapped text is then typed literally. Composing
inside the writer would narrow that window but not close it, because the
application can toggle the mode at any time.

Message bodies are typed verbatim and are not sanitised: a body containing
`ESC[201~` closes the paste early, and one containing `ESC[?2004h` can latch the
detector through the PTY echo. This is not a new privilege - any sender can
already write arbitrary control bytes to the target - but it is not a trust
boundary either. `--no-enter` still writes the closing marker but omits the
`\r`, so the block is closed and not submitted.

In the default ConPTY configuration used by `portable-pty` (no passthrough) the
daemon reads re-rendered ConPTY output, not a raw PTY: ConPTY consumes the
private terminal-mode sequences, so `ESC[?2004h` never reaches the daemon and
`auto` always falls back to plain. `--paste bracketed` is accepted but its
markers are consumed by ConPTY before the child sees them, so it has no
observable effect there. Bracketed paste is therefore Unix-only in practice;
the byte composition itself is platform-neutral and unit-tested. A manual
Windows smoke test confirmed the fallback (`paste : plain`, marker stripped from
`read`), but there is no automated Windows test for it - the three byte-capture
e2e tests are Unix-only.

Roadmap §2 (multi-line handling, bracketed paste where supported, documented
fallback).

## 4. `read` is a line view of raw bytes, not a terminal screen

`read` strips ANSI sequences from the retained byte ring and returns the last N
lines (`buffer.rs:86-100`, `:102-119`, `:127-167`). Cursor movement, erase-line/screen,
alternate screen and wrapping are discarded, so a full-screen TUI renders as a
puzzle of redraw fragments.

```bash
agent-bridge new tui --cwd . -- opencode     # any full-screen TUI
agent-bridge read tui --lines 40
```

Roadmap §6 (VT screen model and snapshot).

## 5. No byte offset cursor; `seq` counts chunks

`Read` returns the current `seq` (`protocol.rs:457-466`); `Attach --from-seq`
only decides whether to replay the *whole* retained buffer (`daemon.rs:813-814`).
Because `seq` increments per PTY chunk, not per byte, callers cannot resume from
an arbitrary offset, and a replay frame carries the newest `seq` for bytes that
may include older chunks.

```bash
agent-bridge --json read shell --lines 5     # note "seq"
agent-bridge attach shell --from-seq <seq>   # skips replay only when >= current
```

Roadmap §3 (`--since` byte offset, wrap detection, follow/stream).

## 6. `truncated` is latched for the life of the agent

Once the 256 KiB ring drops any byte (`buffer.rs:51-58`), `truncated` stays true
for that agent even if the output since then fits entirely
(`buffer.rs:80-82`). Consumers cannot tell whether the *requested window* was
truncated.

Roadmap §3 (cursor semantics that report what is actually available).

## 7. Exited agents are never removed; names cannot be reused

The agent map is only inserted into (`daemon.rs:236`); there is no removal
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
(`daemon.rs:91-97`). `daemon restart` stops the daemon, which sends Ctrl-C to
every agent, force-kills any that are still running after ~500 ms
(`daemon.rs:175-184`, `agent.rs:564-584`), and then exits; the next daemon
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
`AGENT_BRIDGE_AGENT` (`agent.rs:156`). Nothing is filtered, so any secret in
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
no audit trail; `from` is an unauthenticated label (`daemon.rs:241-385`).

```bash
agent-bridge kill dev --force        # any local process can do this
```

Roadmap §4 (threat model, policy, identity, audit).

## 13. Output retention ends at 256 KiB and drops slow subscribers

Retention defaults to a 256 KiB ring (`buffer.rs:6`). `AGENT_BRIDGE_BUFFER_BYTES`
raises or lowers it per agent, clamped to 1 KiB..16 MiB (`buffer.rs:8-29`), and is
sampled at spawn, so existing agents keep their size. Even at the maximum, older
bytes are dropped and only the `truncated` flag says so - no byte offset survives.
An attach client that cannot keep up with 1024 queued events is disconnected
silently (`agent.rs:35`, `:509-519`, `:523-538`); the CLI sees the stream close,
not an error.

Roadmap §3 (explicit drop signalling, offset expiry).

## 14. Message history is per-agent and bounded at 500

`record_message` keeps the newest 500 entries (`agent.rs:24`, `:472-481`), and
the global id-indexed log keeps 2000 (`daemon.rs:25`, `:33-82`). Both are
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

## 17. Readiness is a heuristic, not a prompt detector

`listagents` reports `busy`, `idle` or `unknown` from output quiescence
(`readiness.rs:90-117`, `agent.rs:329-345`). A command that prints nothing
for longer than `AGENT_BRIDGE_QUIET_MS` (default 500 ms) is reported `idle`
even while it is still running, and an application that redraws a spinner is
`busy` forever. The optional literal `AGENT_BRIDGE_PROMPT_PATTERN` suffix only
helps when the newest retained line actually ends with that text; it is not a
regex and cannot distinguish a prompt from ordinary output. Configuration is
sampled when the agent is spawned, so changing the environment later affects
only new agents. Treat the state as a hint, never as proof that the target is
ready for input.

Roadmap §3 (heuristic readiness with documented caveats).
