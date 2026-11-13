# agent-bridge

Cross-platform session bus for terminal agents: **list agents, message other
terminals, read their output** - with no `tmux`, no X server and no
platform-specific shell plugins.

The design goal is the workflow popularised by Claude Code's `listagents` /
`sendmessage` tools, but working identically on **Windows** and **macOS/Linux**.

```
┌──────────────┐        ┌──────────────────────────────┐
│ terminal A   │───────▶│  agent-bridge daemon         │
│ (claude)     │        │  ├─ PTY session: claude      │
└──────────────┘        │  ├─ PTY session: dev         │
┌──────────────┐        │  └─ local socket (pipe/UDS)  │
│ terminal B   │◀──────▶│                              │
│ (opencode)   │        └──────────────────────────────┘
└──────────────┘
```

## Why not tmux?

`tmux` does not exist on Windows, so any tool built directly on it is
Unix-only. `agent-bridge` owns the terminals itself through a PTY
(`ConPTY` on Windows, `openpty` on Unix) via [`portable-pty`], and exposes a
single local-socket protocol. One implementation, three operating systems.

## Documentation

- **[Tutorial](docs/tutorial.md)** - hands-on walkthrough: install, the daemon,
  spawning agents, messaging between terminals, `read`/`attach`/`kill`,
  scripting with `--json` and troubleshooting, with copy-pasteable commands for
  Windows, macOS and Linux.
- **[Architecture](docs/architecture.md)** - how the pieces fit, anchored to the
  code with `file:line` references.
- **[Known limitations](docs/known-limitations.md)** - confirmed gaps, with
  reproductions.
- **[Roadmap](docs/roadmap.md)** - the phased plan.

New here? Start with the **[tutorial](docs/tutorial.md)**.

## Install

```bash
cargo install --path crates/ab-cli
```

## Quick start

```bash
# 1. start the background daemon (auto-started by other commands too)
agent-bridge daemon start

# 2. create two agent terminals (any command works)
agent-bridge new claude -- claude
agent-bridge new dev    -- opencode

# 3. see what is running
agent-bridge listagents

# 4. message another terminal (types the text + Enter into its PTY)
agent-bridge sendmessage dev "run the test suite and report back"
agent-bridge sendmessage claude --from dev "tests are green, please review PR #4"

# 5. peek at / drive a terminal from here
agent-bridge read dev --lines 60
agent-bridge attach dev          # Ctrl-] detaches
```

`sendmessage` writes into the target PTY exactly as if a human typed it, so any
interactive CLI works: Claude Code, OpenCode, a shell, a REPL, an editor.

Every message gets a daemon-wide id and a delivery status: `queued`, `writing`,
`written`, `failed` or `unknown`. `written` only means the daemon's PTY write
returned; it is not an acknowledgement from the target application. Payloads
are limited to 64 KiB by default; export `AGENT_BRIDGE_MAX_MESSAGE` (1 KiB..1
MiB) before `daemon start`/`restart`, because the daemon reads it.

Each agent has a bounded write queue (128 jobs / 2 MiB by default,
`AGENT_BRIDGE_QUEUE_LIMIT` and `AGENT_BRIDGE_QUEUE_BYTES`, read when the agent
spawns) and one writer thread, so concurrent senders cannot interleave bytes.
`sendmessage` waits for the terminal status by default; `--queue-timeout` (5s),
`--write-timeout` (5s, `0` waits forever) and `--no-wait` control the wait. A
write that is still blocked when `--write-timeout` expires returns `unknown`
and is amended when the syscall returns; a message that has not started yet
stays `queued` and is still delivered later. Note that `0` means fail fast for
`--queue-timeout` but wait forever for `--write-timeout`. `sendmessage` exits 0
for `queued`/`unknown` too. `written` and `awaiting_ack` are confirmed PTY
writes; `acknowledged` additionally means the receiving integration reported
that it saw the message. `listagents` shows the queue depth.

Add `--require-ack` when you need the receiving side to confirm it saw the
message: the status then becomes `awaiting_ack` after the write, and an
integration that received it runs `agent-bridge ack <name> <id> [--note "..."]`
to record `acknowledged`. `pending <name>` lists the ids still awaiting an ack
alongside the queued/writing/unknown ones. The ack is explicit, target-checked
and idempotent; it reports that the integration saw the message, not that the
application understood or completed the request. `written`, `awaiting_ack` and
`acknowledged` are the only statuses that mean the bytes reached the PTY.

`sendmessage --wait-idle` holds the message until the target reports `idle` or
`unknown` (a fresh agent with no visible output is not blocked), so a command is
not typed into a busy terminal. If `--timeout` (default 30s) expires while the
target is still `busy`, nothing is written and the command fails with the
`timeout` code. `--wait-reply` waits for output after the write to go quiet and
prints the ANSI-stripped new lines plus whether the reply `settled` or
`timed_out`. The daemon keeps at most 64 KiB and 200 lines; `truncated` reports
dropped bytes. It is best effort: a target that never prints yields an empty
`timed_out` reply (a Unix PTY may still echo the typed bytes), a failed write
leaves `reply` null, and a `written` status still only means the PTY accepted
the bytes. `--wait-reply` conflicts with `--no-wait`; `--timeout 0` checks once
instead of waiting.

`listagents` shows a heuristic **READY** column: `busy`, `idle` or `unknown`
(`listagents --json` reports `readiness`). Output that arrived within the quiet
period is `busy`; an agent that printed a visible line and then went quiet past
it is `idle`; a stopped agent, or one that never printed a visible line, is
`unknown`. An optional literal prompt suffix (max 128 characters) on the newest
line can mark `idle` early. The state is a hint, never a guarantee: a command
that printed once and then runs silently is reported `idle`.
Tune it with `AGENT_BRIDGE_QUIET_MS` (default `500`, clamped 50..60000) and
`AGENT_BRIDGE_PROMPT_PATTERN`. Retained output is 256 KiB per agent;
`AGENT_BRIDGE_BUFFER_BYTES` overrides it (clamped 1 KiB..16 MiB). All three are
read by the daemon when it spawns an agent, so export them before
`daemon start`/`restart`; an already-running daemon will not see a later
change.

Multi-line text is bracket-pasted (`ESC[200~`..`ESC[201~`) when the target
application announced support with `ESC[?2004h`; this is `--paste auto`, the
default. Use `--paste plain` for applications that print the markers as literal
text, or `--paste bracketed` to force the markers for an application that
supports paste without announcing it. Detection reads the daemon's PTY stream;
in the default ConPTY configuration on Windows that stream is re-rendered by
ConPTY, which consumes these markers, so `auto` behaves like `plain` there.
`listagents --json` reports `bracketed_paste`.

Options must come **before** the message text, because the text is a trailing
argument (`sendmessage dev --no-wait "run the tests"`).

## Commands

| Command | Purpose |
| --- | --- |
| `agent-bridge daemon start\|stop\|restart\|status\|run` | manage the background daemon |
| `agent-bridge listagents` | table of agents (name, status, readiness, pid, messages, queued, last output) |
| `agent-bridge new <name> [-- <cmd...>]` | spawn an agent in a daemon-owned PTY |
| `agent-bridge sendmessage <name> [--from L] [--paste auto\|bracketed\|plain] [--require-ack] [--wait-idle] [--wait-reply] [--queue-timeout 5s] [--write-timeout 5s] [--timeout 30s] [--no-wait] <text>` | type text into another agent's terminal; prints the message id, status, and any collected reply |
| `agent-bridge ack <name> <id> [--note TEXT]` | record an explicit application-level acknowledgement for a message you received (alias `acknowledge`) |
| `agent-bridge messagestatus <id>` | delivery status of one message (alias `msg-status`) |
| `agent-bridge pending <name>` | messages for an agent that are still queued, writing, unknown or awaiting_ack |
| `agent-bridge read <name> --lines N` | ANSI-stripped tail of its output |
| `agent-bridge attach <name>` | live mirror + keyboard passthrough |
| `agent-bridge kill <name>` | Ctrl-C first; `--force` kills the agent's process tree immediately (Windows: whole job; Unix: process group + best-effort tree snapshot from `ps` or `/proc`, plus a PTY fd sweep on Linux) |
| `agent-bridge log` | global message history (id, status, sender, body) |
| `agent-bridge ping` / `version` | health, protocol details, daemon pid and teardown mechanism |

Every command supports `--json` for scripting and for the Tauri UI.

## Protocol

Newline-delimited JSON over a local socket:

* Unix: `~/.agent-bridge/daemon.sock` (filesystem socket, dir `0700` / socket
  `0600`)
* Windows: `\\.\pipe\agent-bridge-<user>` (named pipe with a DACL for
  the owner, `SYSTEM` and Administrators)

Requests are tagged (`{"cmd":"send", ...}`), responses are tagged
(`{"kind":"sent", ...}`) and `attach` streams `{"event":"output", ...}` frames
with base64 payloads so binary terminal output stays valid UTF-8 JSON.
See [`crates/ab-core/src/protocol.rs`](crates/ab-core/src/protocol.rs).

## Workspace layout

```
crates/ab-core   daemon, PTY sessions, protocol, client
crates/ab-cli    agent-bridge CLI
```

The Tauri desktop UI (next milestone) will reuse `agent-bridge-core` directly
instead of shelling out to the CLI.

## Quality gates

`fmt`, `clippy -D warnings`, unit + cross-platform integration tests, release
builds on Linux/macOS/Windows, `cargo-deny` and coverage run on every pull
request. `main` is protected and only accepts reviewed PRs from `dev`.

## License

MIT

[`portable-pty`]: https://crates.io/crates/portable-pty
