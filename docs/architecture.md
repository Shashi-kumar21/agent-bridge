# Architecture

Current implementation, as of `dev` after v0.1.2. Every statement below points at
the code that implements it (`file:line`); line numbers are for the commit that
added this document and will drift, so treat the symbol name as the anchor.

This document describes **what exists**. Planned work is in
[`roadmap.md`](roadmap.md); confirmed limitations are in
[`known-limitations.md`](known-limitations.md).

## 1. Components

```
┌─────────────┐   NDJSON over UDS / named pipe   ┌─────────────────────┐
│ ab-cli      │ ───────────────────────────────▶ │ ab-core daemon      │
│ (main,      │ ◀─────────────────────────────── │ accept loop         │
│  attach)    │        response / event stream   │  + per-conn thread  │
└─────────────┘                                  └──────────┬──────────┘
                                                            │
                                              ┌─────────────┴─────────────┐
                                              │ Agent (one per session)   │
                                              │  PTY master + child       │
                                              │  output ring buffer       │
                                              │  subscriber channels      │
                                              │  ProcessReaper (OS link)  │
                                              └───────────┬───────────────┘
                                                          │
                                                ┌─────────┴─────────┐
                                                │ OS PTY (ConPTY /  │
                                                │ openpty) + child  │
                                                └───────────────────┘
```

| Crate | Responsibility |
| --- | --- |
| `ab-core` | Wire types, daemon, agent/PTY management, reaper, transport, blocking client |
| `ab-cli` | Command surface (`clap`), human/JSON formatting, `attach` mirroring, daemon autostart |

Module map (`crates/ab-core/src`):

| File | Role |
| --- | --- |
| `protocol.rs` | Wire types, `PROTOCOL_VERSION`, name validation (`validate_name`, `:635`) |
| `delivery.rs` | Message ids, size limit, payload composition, bounded write queue, writer thread, timeouts |
| `transport.rs` | Listener/connect, Unix socket permissions, Windows pipe DACL (`:41`) |
| `daemon.rs` | Accept loop, request handlers, attach streaming |
| `agent.rs` | PTY spawn, output pump, wait loop, urgent writes, resize, kill |
| `buffer.rs` | Bounded byte ring (capacity from env), ANSI stripping, tail extraction, newest visible line |
| `readiness.rs` | Quiet-period + prompt-suffix heuristic (`AGENT_BRIDGE_QUIET_MS`, `AGENT_BRIDGE_PROMPT_PATTERN`) |
| `reaper.rs` | OS-level lifetime link and `kill --force` tree teardown |
| `client.rs` | One request per connection; `Subscriber` for attach |
| `paths.rs` | State dir resolution (`AGENT_BRIDGE_HOME`), socket/log paths |

## 2. Process and thread model

* The daemon is a single process (`crates/ab-cli/src/main.rs`, `DaemonAction::Run`).
  `Daemon::run` binds the endpoint, then loops on `listener.accept()`
  (`daemon.rs:143-182`).
* Every accepted connection gets its own thread (`daemon.rs:163-167`). There is no
  connection pool and no request queue: concurrency is one thread per client, and
  shared state is protected by mutexes (`Inner::agents`, `Inner::messages`,
  `daemon.rs:102-108`).
* Each agent starts three threads: a PTY reader pump (`agent.rs:213-215`), a
  child waiter that polls `try_wait` every 120 ms (`agent.rs:218-220`,
  `:262-304`), and one write-queue writer (`delivery.rs:368-372`). The writer exits
  when the agent is marked not running or the queue is dropped.
* Per-agent mutable state lives behind mutexes inside `Agent`
  (`agent.rs:93-122`): PTY master (resize), child handle, output buffer, message
  history, subscriber list, reaper, and the write-queue handle.
* Shutdown: `Request::Shutdown` sets an `AtomicBool` (`Inner::shutdown`, `daemon.rs:506-509`); the
  accept loop observes it, calls `Inner::stop_agents` (`daemon.rs:149`, `:179`,
  `:186-195`), which sends Ctrl-C to every agent, force-kills the ones still
  running after ~500 ms (`agent.rs:593-620`), and then lets the process exit.

## 3. Request lifecycle (non-streaming)

1. CLI builds a [`Request`], calls `client::request` (`client.rs:16-35`).
2. `request` opens a fresh connection, writes one NDJSON line and reads one line
   back (`client.rs:16-35`); the connection is then dropped.
3. Daemon-side `serve_connection` reads one line, parses it, and either
   dispatches to `Inner::handle` or routes it to `stream_attach`
   (`serve_connection`, `daemon.rs:917-945`).
4. `Inner::handle` maps errors to `Response::Error` with an `ErrorCode`
   (`daemon.rs:197-205`); `try_handle` implements the match
   (`daemon.rs:207-514`).
5. The response is serialized as one line with the whole frame built before the
   write, so frames cannot interleave mid-line (`write_frame`, `daemon.rs:1036-1043`).

Every request except `Attach` is one request and one response. There is no
pipelining: a second request on the same connection is never read.

## 4. Output lifecycle

1. The PTY reader pump reads up to 8 KiB per `read` (`agent.rs:225-260`).
2. The chunk is appended to the per-agent ring buffer
   (`agent.rs:236-238`, `buffer.rs:73-81`). Capacity is 256 KiB
   (`buffer.rs:8`); the oldest bytes are dropped and `truncated` is latched.
3. The chunk is stamped with the next sequence number (`agent.rs:243`) and
   published to subscribers as a base64 `Event::Output` (`agent.rs:244-248`).
4. Subscribers are bounded channels of 1024 events (`agent.rs:35`, `:541-555`). A
   subscriber that cannot keep up is dropped and logged
   (`agent.rs:557-574`).
5. A plain `Read` returns ANSI-stripped tail lines plus the current `seq` and
   `next_offset` (`daemon.rs:454-461`, `agent.rs:497-505`, `buffer.rs:149-163`);
   it is a line view of raw bytes, not a terminal screen. `Read { since }`
   returns a base64 `Chunk` of raw bytes from an absolute byte offset, at most
   64 KiB per response (`MAX_READ_CHUNK`, `daemon.rs:41`), with `offset`,
   `next_offset`, `dropped` and `seq` (`daemon.rs:430-453`, `buffer.rs:122-145`).
   `dropped` means the requested offset had rotated out and the chunk starts at
   the oldest retained byte; `since` past the newest byte is a `BadRequest`
   (`CursorAhead`, `error.rs:76-87`). Offsets are per agent and daemon lifetime
   (`Offset`, `protocol.rs:24-29`); there is no blocking follow.
6. `Attach` subscribes first, sends `Hello`, optionally replays the whole
   retained buffer as one frame tagged with the *current* `seq`
   (`daemon.rs:975-988`), then streams live events until exit or disconnect
   (`daemon.rs:1004-1032`). Because the replay and the live stream can repeat a
   sequence number, clients drop non-increasing `seq` (`client.rs:79-83`).
7. Readiness is derived per request from this stream (`Agent::readiness`,
   `agent.rs:338-354`): output newer than `AGENT_BRIDGE_QUIET_MS` (default
   500 ms) is `busy`, older output is `idle`, and an optional
   `AGENT_BRIDGE_PROMPT_PATTERN` literal suffix on the newest line marks `idle`
   early (`readiness.rs:94-121`). A stopped agent, or one without a visible
   output line (control sequences alone do not count), is `unknown`. This is
   a heuristic, never a guarantee.
   Retained output is 256 KiB by default; `AGENT_BRIDGE_BUFFER_BYTES` clamps a
   per-agent override to 1 KiB..16 MiB (`buffer.rs:8-34`).

Sequence numbers count output chunks, not bytes, and are process-local: they
restart from zero in a new daemon.

## 5. Message (input) lifecycle

1. `Send` builds the payload (`delivery::compose_payload`): with `--paste auto`
   (default), text that contains a line break is wrapped in
   `ESC[200~`..`ESC[201~` only when the target announced bracketed paste
   (`ESC[?2004h` in its output, tracked per agent). `bracketed` forces the
   markers and `plain` is the documented fallback; the optional `\r` is appended
   after the closing marker. A payload above the size limit is rejected with
   `too_large` before allocating an id (`daemon.rs:279-292`,
   `delivery.rs:73-87`). The limit is 64 KiB by default and is configurable with
   `AGENT_BRIDGE_MAX_MESSAGE` (clamped to 1 KiB..1 MiB).
2. The daemon allocates a daemon-wide `message_id` (`delivery.rs:67-71`), stores
   the record as `queued` in the bounded id-indexed log (2000 entries,
   `daemon.rs:43-92`) and publishes `Event::Message`.
3. The payload becomes one `delivery::Job` and is admitted to the agent's
   bounded queue (`delivery.rs:336-373`). The defaults are 128 jobs and 2 MiB
   per agent, configurable with `AGENT_BRIDGE_QUEUE_LIMIT` and
   `AGENT_BRIDGE_QUEUE_BYTES`. Admission waits up to `--queue-timeout` (5 s
   default, `0` fails fast); a full queue ends `failed { code: queue_full }`
   without a partial write (`delivery.rs:526-541`).
4. Each agent has one writer thread (`delivery.rs:423-484`) that performs one
   `write_all` + `flush` per job, so jobs cannot interleave bytes. A separate
   urgent channel carries Ctrl-C from `kill` and `Input` keystrokes from
   `attach`; urgent jobs are selected between jobs, so they may pass a queued
   message but never split one (`agent.rs:423-433`).
5. The status moves `queued -> writing -> written`, to
   `failed { code, detail }` on exit/queue-full/IO error, or to `unknown` when
   the caller's `--write-timeout` expires while the PTY write is still blocked.
   A message that is still `queued` at the deadline stays `queued` and keeps
   waiting; `unknown` is only used after the write started. The writer amends an
   `unknown` record to `written`/`failed` when the syscall returns
   (`daemon.rs:562-595`, `:838-863`). When the sender asked for an
   acknowledgement, the completed write lands in `awaiting_ack` instead of
   `written` (`settle`, `daemon.rs:825-835`). `pending` lists `queued`,
   `writing`, `unknown` and `awaiting_ack` records because all four may still
   change.
6. `sendmessage` waits for the terminal status by default; `--no-wait` returns
   after admission. The response is
   `Sent { target, bytes, message_id, status, reply }` (`protocol.rs:538-554`);
   `reply` is `Some` only when `--wait-reply` was requested and the write
   completed, so a failed or abandoned write leaves it `null`. A `written` or
   `awaiting_ack` status means "bytes were accepted by the PTY", never that the
   target application read, understood or acted on them; `queued`/`unknown`
   responses carry `bytes: 0` because nothing is confirmed.
7. `--require-ack` opts into an application-level acknowledgement. The
   receiving integration (a wrapper, or the agent itself with a tool call) sends
   `Request::Ack { target, id, note }` (`protocol.rs:381-395`); only the
   recorded target may acknowledge, a write that started (`written`,
   `awaiting_ack` or `unknown`) can be acknowledged, and repeating the request
   is idempotent. An `unknown` write is accepted because the acknowledgement
   itself proves the bytes arrived. `acknowledge`
   (`daemon.rs:605-668`) stores `Ack { at, note }`, moves the status to
   `acknowledged` and publishes `Event::MessageStatus`. Notes are capped at
   1 KiB because the log is bounded. An acknowledgement is stronger than the
   write outcome, so a late `done` event cannot downgrade it
   (`apply_status`, `daemon.rs:837-863`); only `acknowledged` means the receiver
   reported that it saw the message.
8. `--wait-idle` calls `wait_until_ready` before allocating the id
   (`daemon.rs:294-296`, `:734-752`): it polls `Agent::readiness` every 25 ms,
   proceeds on `idle` and `unknown`, and fails with
   `BridgeError::WaitIdleTimeout` (`error.rs:69-74`, code `timeout`) without
   recording or writing anything when the deadline expires. `--wait-reply`
   subscribes to the agent's event channel before enqueueing (`agent.rs:541-555`)
   and, after the write completes, runs `collect_reply` until output has been
   quiet for the agent's configured period, the PTY reader closes after an exit,
   the channel closes, or the deadline expires (`daemon.rs:757-823`). On exit it
   drains the queued `Event::Output` frames before settling, so the target's
   last line is not lost. The collector retains at most 64 KiB and returns the
   last 200 lines with `settled`/`timed_out`/`truncated` (`Reply`,
   `protocol.rs:258-280`); `truncated` only reports dropped bytes, not dropped
   lines. Each wait uses `--timeout` (default 30 s,
   `delivery.rs:56`, clamped by `MAX_TIMEOUT`); `--wait-reply` is rejected with
   `--no-wait`. The reply is attached to the same `Sent` response
   (`protocol.rs:538-554`), which requires protocol v7 or newer (the cursor
   fields raise it to v8).
9. `MessageStatus` and `Pending` read the same records (`daemon.rs:484-505`).
   Ids, statuses and acknowledgements are per daemon lifetime; a restart clears
   the history.

The `from` field is a caller-supplied label, stored and forwarded verbatim; it
is never authenticated.

### Bracketed paste

`agent.rs` scans the PTY output for `ESC[?2004h` / `ESC[?2004l` (carrying a few
bytes across reads so a split marker still counts) and exposes the last state as
`AgentInfo.bracketed_paste`. The mode is announced by the application, not by
the terminal, and is toggled per prompt by readline-style editors, so the last
marker wins. `PasteMode::Auto` therefore uses the markers only for multi-line
text when the target currently has them enabled. Applications that support paste
without announcing it can be forced with `--paste bracketed`; applications that
echo the markers can be pinned to the old behaviour with `--paste plain`. The
scan runs on the daemon's PTY stream: on Windows that stream is ConPTY output,
which consumes private terminal-mode sequences, so `auto` degrades to plain
there (bracketed paste is a Unix-PTY feature in practice).

## 6. Process ownership and lifetime

* Every agent is spawned by the daemon inside a fresh PTY (`agent.rs:126-223`):
  `openpty`, `CommandBuilder` with `TERM=xterm-256color` and
  `AGENT_BRIDGE_AGENT=<name>`, then `slave.spawn_command`.
* The child inherits the daemon's environment and working directory (cwd falls
  back to the daemon's cwd when `--cwd` is omitted, `agent.rs:146-154`).
* Windows: the child is assigned to an anonymous job object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` immediately after spawn
  (`reaper.rs`, `install`), so the kernel kills the job when the daemon dies.
* Unix: `portable-pty` makes the child a session leader on the PTY; daemon death
  closes the master and the kernel sends `SIGHUP` to the foreground process
  group. `kill --force` additionally signals the process group, the `ps`
  snapshot of descendants (`ps` or, on Linux, `/proc`), and on Linux every
  process still holding the PTY slave (`reaper.rs`).
* Agents are **not** adoptable: buffers, message history and PTY handles live in
  daemon memory only. A daemon restart discards exited entries and terminates
  live agents.

## 7. Endpoint and security boundary

* State directory: `~/.agent-bridge` or `AGENT_BRIDGE_HOME` (`paths.rs:20-26`).
  Created with mode `0700` on Unix (`paths.rs:51-61`).
* Unix socket: `daemon.sock` in that directory, mode `0600`
  (`paths.rs:34-36`, `transport.rs:92-98`).
* Windows: named pipe `\\.\pipe\agent-bridge-<user>` (user name sanitised) or
  `AGENT_BRIDGE_PIPE` (`transport.rs:10-20`), with an explicit DACL granting
  owner, `SYSTEM` and Administrators (`transport.rs:41`).
* Stale Unix sockets are removed only when no daemon answers a `ping`
  (`transport.rs:79-88`).
* There is no authentication or authorization beyond the endpoint's access
  control: any process that can open the socket/pipe can send, read, kill and
  shut down. Caller identity is not derived from the OS; `from` is a label
  (see `roadmap.md` §4 for the planned policy layer).

## 8. CLI daemon management

* `ensure_daemon` pings `Version`, compares `PROTOCOL_VERSION`, and on a skew
  restarts only when `List` reports zero agents - exited entries included,
  because the map is never pruned (`main.rs`, `ensure_daemon`).
* `start_daemon` spawns `current_exe daemon run` with stdout/stderr redirected
  to `daemon.log`, no console window on Windows, and waits up to 5 s for a ping
  (`main.rs`, `start_daemon`).
* `daemon restart` = stop + start, so it terminates live agents by design.

## 9. Tests and CI

* Unit tests live next to the code (`buffer.rs`, `protocol.rs`, `transport.rs`,
  `delivery.rs`, `reaper.rs` on Linux).
* `crates/ab-core/tests/daemon_e2e.rs` exercises a real daemon over the real
  endpoint, including message ids, `messagestatus`, `pending` and the
  oversize rejection; `crates/ab-core/tests/message_delivery.rs` covers the
  concurrent 8x50 write test and `--no-wait`; `crates/ab-cli/tests/orphan_teardown.rs`
  covers process lifetime.
* CI (`.github/workflows/ci.yml`) runs `build` and `test` on the
  Windows/macOS/Linux matrix; `fmt`, `clippy -D warnings`, `deny` and
  `coverage` run on Linux only. All jobs run for pushes to `main`/`dev` and for
  pull requests.

## 10. Related documents

* [`tutorial.md`](tutorial.md) - user-facing walkthrough.
* [`../SECURITY.md`](../SECURITY.md) - mechanism, threat notes, known gaps.
* [`roadmap.md`](roadmap.md) - phased plan.
* [`known-limitations.md`](known-limitations.md) - confirmed limitations with
  reproductions.
