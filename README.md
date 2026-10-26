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

## Commands

| Command | Purpose |
| --- | --- |
| `agent-bridge daemon start\|stop\|restart\|status\|run` | manage the background daemon |
| `agent-bridge listagents` | table of agents (name, status, pid, messages, last output) |
| `agent-bridge new <name> [-- <cmd...>]` | spawn an agent in a daemon-owned PTY |
| `agent-bridge sendmessage <name> <text>` | type text into another agent's terminal |
| `agent-bridge read <name> --lines N` | ANSI-stripped tail of its output |
| `agent-bridge attach <name>` | live mirror + keyboard passthrough |
| `agent-bridge kill <name>` | Ctrl-C first, hard kill on `--force` |
| `agent-bridge log` | global message history |
| `agent-bridge ping` / `version` | health and protocol details (includes the daemon pid) |

Every command supports `--json` for scripting and for the Tauri UI.

## Protocol

Newline-delimited JSON over a local socket:

* Unix: `~/.agent-bridge/daemon.sock` (filesystem socket, dir `0700` / socket
  `0600`)
* Windows: `\\.\pipe\agent-bridge-<user>` (named pipe with an owner-only DACL)

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
