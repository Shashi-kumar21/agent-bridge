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
  (`daemon.rs:74-92`).
* Every accepted connection gets its own thread (`daemon.rs:78`). There is no
  connection pool and no request queue: concurrency is one thread per client, and
  shared state is protected by mutexes (`Inner::agents`, `Inner::log`,
  `daemon.rs:27-33`).
* Each agent starts two threads: a PTY reader pump (`agent.rs:150-153`) and a
  child waiter that polls `try_wait` every 120 ms (`agent.rs:155-158`,
  `:191`).
* Per-agent mutable state lives behind mutexes inside `Agent`
  (`agent.rs:60-69`): PTY writer, PTY master (resize), child handle, output
  buffer, message history, subscriber list, reaper.
* Shutdown: `Request::Shutdown` sets an `AtomicBool` (`daemon.rs:223-226`); the
  accept loop observes it, calls `Inner::stop_agents` (`daemon.rs:94`,
  `:101-110`), which sends Ctrl-C to every agent, force-kills the ones still
  running after ~500 ms (`agent.rs:404-415`), and then lets the process exit.

## 3. Request lifecycle (non-streaming)

1. CLI builds a [`Request`], calls `client::request` (`client.rs:16-35`).
2. `request` opens a fresh connection, writes one NDJSON line and reads one line
   back (`client.rs:16-35`); the connection is then dropped.
3. Daemon-side `serve_connection` reads one line, parses it, and either
   dispatches to `Inner::handle` or routes it to `stream_attach`
   (`daemon.rs:294-322`).
4. `Inner::handle` maps errors to `Response::Error` with an `ErrorCode`
   (`daemon.rs:112-120`); `try_handle` implements the match
   (`daemon.rs:123-231`).
5. The response is serialized as one line with the whole frame built before the
   write, so frames cannot interleave mid-line (`daemon.rs:413-420`).

Every request except `Attach` is one request and one response. There is no
pipelining: a second request on the same connection is never read.

## 4. Output lifecycle

1. The PTY reader pump reads up to 8 KiB per `read` (`agent.rs:163-187`).
2. The chunk is appended to the per-agent ring buffer
   (`agent.rs:170-172`, `buffer.rs:28-35`). Capacity is 256 KiB
   (`buffer.rs:6`); the oldest bytes are dropped and `truncated` is latched.
3. The chunk is stamped with the next sequence number (`agent.rs:177`) and
   published to subscribers as a base64 `Event::Output` (`agent.rs:178-182`).
4. Subscribers are bounded channels of 1024 events (`agent.rs:24`, `:343`). A
   subscriber that cannot keep up is dropped and logged
   (`agent.rs:361-368`).
5. `Read` returns ANSI-stripped tail lines plus the current `seq`
   (`daemon.rs:192-201`, `agent.rs:309-315`, `buffer.rs:63-77`). This is a line
   view of raw bytes, not a terminal screen.
6. `Attach` subscribes first, sends `Hello`, optionally replays the whole
   retained buffer as one frame tagged with the *current* `seq`
   (`daemon.rs:343-365`), then streams live events until exit or disconnect
   (`daemon.rs:380-408`). Because the replay and the live stream can repeat a
   sequence number, clients drop non-increasing `seq` (`client.rs:79-83`).

Sequence numbers count output chunks, not bytes, and are process-local: they
restart from zero in a new daemon.

## 5. Message (input) lifecycle

1. `Send` assembles `message` + optional `\r` (`daemon.rs:167-180`).
2. `Agent::write_bytes` checks liveness, takes the writer mutex, and calls
   `write_all` + `flush` (`agent.rs:286-297`). Because the mutex is held for the
   whole payload, two concurrent senders cannot interleave *within* a call; there
   is no queue, so a slow or blocked write simply blocks the caller's thread.
3. After the write succeeds, the message is recorded on the agent (bounded to
   500 entries, `agent.rs:20`, `:318-327`), appended to the global log (bounded
   to 1000, `daemon.rs:24`, `:252-261`), and published to subscribers as
   `Event::Message` (`daemon.rs:181-190`).
4. The response is `Sent { target, bytes }` (`protocol.rs:238-244`). It means
   "bytes were accepted by the PTY", not that the target application read,
   understood or acted on them.
5. `Input` (used by `attach`) writes base64 keystrokes through the same
   `write_bytes` path (`daemon.rs:207-214`).

The `from` field is a caller-supplied label, stored and forwarded verbatim; it
is never authenticated.

## 6. Process ownership and lifetime

* Every agent is spawned by the daemon inside a fresh PTY (`agent.rs:74-120`):
  `openpty`, `CommandBuilder` with `TERM=xterm-256color` and
  `AGENT_BRIDGE_AGENT=<name>`, then `slave.spawn_command`.
* The child inherits the daemon's environment and working directory (cwd falls
  back to the daemon's cwd when `--cwd` is omitted, `agent.rs:94-103`).
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
  `reaper.rs` on Linux).
* `crates/ab-core/tests/daemon_e2e.rs` exercises a real daemon over the real
  endpoint; `crates/ab-cli/tests/orphan_teardown.rs` covers process lifetime.
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
