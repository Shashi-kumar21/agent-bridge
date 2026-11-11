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
| `protocol.rs` | Wire types, `PROTOCOL_VERSION`, name validation (`validate_name`, `:302`) |
| `transport.rs` | Listener/connect, Unix socket permissions, Windows pipe DACL (`:41`) |
| `daemon.rs` | Accept loop, request handlers, attach streaming |
| `agent.rs` | PTY spawn, output pump, wait loop, write, resize, kill |
| `buffer.rs` | Bounded byte ring, ANSI stripping, tail extraction |
| `reaper.rs` | OS-level lifetime link and `kill --force` tree teardown |
| `client.rs` | One request per connection; `Subscriber` for attach |
| `paths.rs` | State dir resolution (`AGENT_BRIDGE_HOME`), socket/log paths |

## 2. Process and thread model

* The daemon is a single process (`crates/ab-cli/src/main.rs`, `DaemonAction::Run`).
  `Daemon::run` binds the endpoint, then loops on `listener.accept()`
  (`daemon.rs:128-157`).
* Every accepted connection gets its own thread (`daemon.rs:138`). There is no
  connection pool and no request queue: concurrency is one thread per client, and
  shared state is protected by mutexes (`Inner::agents`, `Inner::messages`,
  `daemon.rs:87-93`).
* Each agent starts two threads: a PTY reader pump (`agent.rs:166-169`) and a
  child waiter that polls `try_wait` every 120 ms (`agent.rs:171-174`,
  `:206`).
* Per-agent mutable state lives behind mutexes inside `Agent`
  (`agent.rs:54-75`): PTY master (resize), child handle, output buffer, message
  history, subscriber list, reaper, and the write-queue handle.
* Shutdown: `Request::Shutdown` sets an `AtomicBool` (`Inner::shutdown`, `daemon.rs:411-414`); the
  accept loop observes it, calls `Inner::stop_agents` (`daemon.rs:156`,
  `:161-170`), which sends Ctrl-C to every agent, force-kills the ones still
  running after ~500 ms (`agent.rs:478-496`), and then lets the process exit.

## 3. Request lifecycle (non-streaming)

1. CLI builds a [`Request`], calls `client::request` (`client.rs:16-35`).
2. `request` opens a fresh connection, writes one NDJSON line and reads one line
   back (`client.rs:16-35`); the connection is then dropped.
3. Daemon-side `serve_connection` reads one line, parses it, and either
   dispatches to `Inner::handle` or routes it to `stream_attach`
   (`serve_connection`, `daemon.rs:606-635`).
4. `Inner::handle` maps errors to `Response::Error` with an `ErrorCode`
   (`daemon.rs:172-181`); `try_handle` implements the match
   (`daemon.rs:183-418`).
5. The response is serialized as one line with the whole frame built before the
   write, so frames cannot interleave mid-line (`write_frame`, `daemon.rs:725-731`).

Every request except `Attach` is one request and one response. There is no
pipelining: a second request on the same connection is never read.

## 4. Output lifecycle

1. The PTY reader pump reads up to 8 KiB per `read` (`agent.rs:178-202`).
2. The chunk is appended to the per-agent ring buffer
   (`agent.rs:186-188`, `buffer.rs:28-35`). Capacity is 256 KiB
   (`buffer.rs:6`); the oldest bytes are dropped and `truncated` is latched.
3. The chunk is stamped with the next sequence number (`agent.rs:192`) and
   published to subscribers as a base64 `Event::Output` (`agent.rs:193-197`).
4. Subscribers are bounded channels of 1024 events (`agent.rs:32`, `:423-433`). A
   subscriber that cannot keep up is dropped and logged
   (`agent.rs:437-452`).
5. `Read` returns ANSI-stripped tail lines plus the current `seq`
   (`daemon.rs:358-367`, `agent.rs:377-384`, `buffer.rs:63-77`). This is a line
   view of raw bytes, not a terminal screen.
6. `Attach` subscribes first, sends `Hello`, optionally replays the whole
   retained buffer as one frame tagged with the *current* `seq`
   (`daemon.rs:664-674`), then streams live events until exit or disconnect
   (`daemon.rs:692-720`). Because the replay and the live stream can repeat a
   sequence number, clients drop non-increasing `seq` (`client.rs:79-83`).

Sequence numbers count output chunks, not bytes, and are process-local: they
restart from zero in a new daemon.

## 5. Message (input) lifecycle

1. `Send` validates the target, appends the optional `\r`, and rejects a
   payload above the size limit with `too_large` before allocating an id
   (`daemon.rs:227-250`, `delivery.rs:75-85`). The limit is 64 KiB by default
   and is configurable with `AGENT_BRIDGE_MAX_MESSAGE` (clamped to 1 KiB..1 MiB).
2. The daemon allocates a daemon-wide `message_id` (`delivery.rs:66`), stores
   the record as `queued` in the bounded id-indexed log (2000 entries,
   `daemon.rs:29-78`) and publishes `Event::Message`.
3. The payload becomes one `delivery::Job` and is admitted to the agent's
   bounded queue (`delivery.rs:294-373`). The defaults are 128 jobs and 2 MiB
   per agent, configurable with `AGENT_BRIDGE_QUEUE_LIMIT` and
   `AGENT_BRIDGE_QUEUE_BYTES`. Admission waits up to `--queue-timeout` (5 s
   default, `0` fails fast); a full queue ends `failed { code: queue_full }`
   without a partial write (`delivery.rs:479-492`).
4. Each agent has one writer thread (`delivery.rs:375-452`) that performs one
   `write_all` + `flush` per job, so jobs cannot interleave bytes. A separate
   urgent channel carries Ctrl-C from `kill` and `Input` keystrokes from
   `attach`; urgent jobs are selected between jobs, so they may pass a queued
   message but never split one (`agent.rs:330-364`).
5. The status moves `queued -> writing -> written`, to
   `failed { code, detail }` on exit/queue-full/IO error, or to `unknown` when
   the caller's `--write-timeout` expires while the PTY write is still blocked.
   The writer amends an `unknown` record to `written`/`failed` when the syscall
   returns (`daemon.rs:463-526`, `:531-587`).
6. `sendmessage` waits for the terminal status by default; `--no-wait` returns
   after admission. The response is `Sent { target, bytes, message_id, status }`
   (`protocol.rs:353-365`). It means "bytes were accepted by the PTY", never
   that the target application read, understood or acted on them.
7. `MessageStatus` and `Pending` read the same records (`daemon.rs:389-407`).
   Ids and statuses are per daemon lifetime; a restart clears the history.

The `from` field is a caller-supplied label, stored and forwarded verbatim; it
is never authenticated.

## 6. Process ownership and lifetime

* Every agent is spawned by the daemon inside a fresh PTY (`agent.rs:82-160`):
  `openpty`, `CommandBuilder` with `TERM=xterm-256color` and
  `AGENT_BRIDGE_AGENT=<name>`, then `slave.spawn_command`.
* The child inherits the daemon's environment and working directory (cwd falls
  back to the daemon's cwd when `--cwd` is omitted, `agent.rs:102-111`).
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

* State directory: `~/.agent-bridge` or `AGENT_BRIDGE_HOME` (`paths.rs:20-33`).
  Created with mode `0700` on Unix (`paths.rs:53-61`).
* Unix socket: `daemon.sock` in that directory, mode `0600`
  (`paths.rs:34-36`, `transport.rs:92-98`).
* Windows: named pipe `\\.\pipe\agent-bridge-<user>` (user name sanitised) or
  `AGENT_BRIDGE_PIPE` (`transport.rs:10-21`), with an explicit DACL granting
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
