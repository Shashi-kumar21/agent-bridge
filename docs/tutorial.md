# Tutorial to use

A ten-minute walkthrough: install `agent-bridge`, start the daemon, spawn a
couple of agent terminals, and make them talk to each other - on Windows, macOS
or Linux, without `tmux`.

Everything below is copy-pasteable. Commands are shown for both shells where
they differ (`cmd`/PowerShell on Windows, `bash`/`zsh` on macOS and Linux), and
the example paths are placeholders - swap the separators for your OS.

## 1. The mental model

`agent-bridge` is two pieces:

```
agent-bridge daemon   - owns every PTY and the message log, listens on a local socket
agent-bridge <cmd>    - a thin client: it connects to that socket, sends one request
```

You never talk to a terminal directly. You ask the daemon, and the daemon types
into the PTY or reads its output. That is why the same commands work on all
three operating systems: the daemon uses ConPTY on Windows and `openpty` on
Unix, and everything above it is one protocol.

Two consequences worth knowing up front:

* The daemon must be running. Commands start it automatically, but it is a
  separate background process; stop it with `agent-bridge daemon stop`.
* Agents die with the daemon. That is deliberate: no orphaned **foreground**
  terminals after a crash (see [SECURITY.md](../SECURITY.md) for the remaining
  gaps, such as a job-control background job on Unix).

## 2. Install

You need Rust 1.85 or newer (`rustup` is the easiest way to get it) and a
working C toolchain for your platform.

```bash
git clone https://github.com/Shashi-kumar21/agent-bridge.git
cd agent-bridge
cargo install --path crates/ab-cli --locked
agent-bridge --version
```

`agent-bridge --version` prints the CLI version. `cargo install` puts the
binary in `~/.cargo/bin`, which `rustup` already adds to `PATH`; if the command
is not found, add that directory to `PATH` and open a new shell.

## 3. Start the daemon

```bash
agent-bridge daemon start
```

It prints the daemon details, so you can confirm what is running:

```
protocol : 1
daemon   : 0.1.2
pid      : 11228
teardown : windows job object (kill on job close)
socket   : \\.\pipe\agent-bridge-<user>
uptime   : 28s ago
```

* `protocol` is the wire version. A client refuses to talk to a daemon with a
  different version.
* `teardown` is the mechanism that stops agents from outliving the daemon:
  a job object on Windows, PTY hangup plus process-tree signalling on Unix
  (and an extra PTY fd sweep on Linux).
* `socket` is the local endpoint - a named pipe on Windows, a Unix domain
  socket on macOS/Linux. Nothing is ever bound to a network interface.

Handy companions:

```bash
agent-bridge daemon status     # same information as start
agent-bridge ping              # prints "pong" when the daemon answers
agent-bridge daemon stop       # stop it (agents are terminated)
agent-bridge daemon restart    # stop + start
agent-bridge daemon run        # run in the foreground, for debugging
```

You do not have to call `daemon start` before every command: `listagents`,
`new`, `sendmessage`, `read`, `kill`, `log` and `version` all start the daemon
on demand.

## 4. Create your first agent

An "agent" is just a command running inside a daemon-owned PTY. Anything that
works in a terminal works here: a shell, Claude Code, OpenCode, a REPL, an
editor.

```bash
# a plain shell (cmd.exe on Windows, $SHELL or /bin/sh on Unix)
agent-bridge new tutorial-demo

# a real agent, with a working directory (Windows)
agent-bridge new claude --cwd C:\work\project -- claude

# the same on macOS/Linux
agent-bridge new tests --cwd ~/work/project -- cargo watch -x test
```

Rules of the road:

* The name is 1-32 characters of `a-z`, `A-Z`, `0-9`, `-` and `_`.
* Everything after `--` is the command and its arguments. Without `--`, no
  command is taken and you get the platform shell.
* `--cwd` sets the working directory (defaults to the daemon's own).
* `--cols` / `--rows` set the PTY size (default `120x32`); resize later by
  attaching and resizing your terminal.
* Inside the child, `AGENT_BRIDGE_AGENT` holds its agent name, so a program can
  tell that it is running under the bridge.

`new` prints the new agent:

```
name       : tutorial-demo
id         : 1a181fa5fa9-1
status     : running
pid        : 17240
command    : cmd.exe
cwd        : C:\work\agent-bridge
size       : 120x32
created    : now
messages   : 0
subscribers: 0
```

`cwd` is the daemon's working directory here because `new` was called without
`--cwd`; pass `--cwd` to start the agent somewhere else.

## 5. See what is running

```bash
agent-bridge listagents
```

```
NAME           STATUS   PID    MSGS  LAST OUTPUT  COMMAND
-----------------------------------------------------------
tutorial-demo  running  17240  0     now          cmd.exe
```

* `STATUS` is `running` or `exited` (the daemon keeps the last known state).
* `MSGS` counts messages delivered to that agent, `LAST OUTPUT` is the age of
  its most recent output.
* Aliases: `agent-bridge list`, `agent-bridge ls`, `agent-bridge agents`.

For scripts, add the global `--json` flag:

```bash
agent-bridge --json listagents
```

```json
{
  "kind": "agents",
  "agents": [
    {
      "id": "1a181fa5fa9-1",
      "name": "tutorial-demo",
      "status": "running",
      "pid": 17240,
      "command": "cmd.exe",
      "cwd": "C:\\work\\agent-bridge",
      "created_at": 1793182031795,
      "last_output_at": 1793182034009,
      "exited_at": null,
      "exit_code": null,
      "message_count": 0,
      "subscribers": 0,
      "cols": 120,
      "rows": 32
    }
  ]
}
```

## 6. Message another terminal

`sendmessage` types the text into the target PTY and presses Enter - exactly as
if a human had typed it.

```bash
agent-bridge sendmessage tutorial-demo "echo hello from agent-bridge"
agent-bridge sendmessage claude --from dev "tests are green, please review PR #4"
```

```
sent 29 bytes to tutorial-demo
```

* `--from <label>` is only recorded in the message log; the target sees the text.
* `--no-enter` types the text without pressing Enter (useful for multi-step
  prompts, or when the target is a TUI that treats Enter specially).
* Multiple arguments are joined with spaces, so quoting is optional:
  `agent-bridge sendmessage dev run the tests` works.
* If the message starts with `-`, put `--` first:
  `agent-bridge sendmessage dev -- "--help is what I meant"`.

Because it is just keystrokes, the target must be a program that is reading its
terminal - a running shell, a CLI agent, a REPL. Sending to an agent that has
already exited fails with `agent '<name>' has already exited`.

Every message is also appended to the global log:

```bash
agent-bridge log              # last 50 entries
agent-bridge log --limit 200
```

```
2s ago  owner -> tutorial-demo: echo hello from agent-bridge
1s ago  dev -> claude: tests are green, please review PR #4
```

Messages sent without `--from` are attributed to `owner`.

## 7. Read and attach

`read` shows the tail of an agent's output with ANSI escapes stripped, so it is
safe to grep and paste:

```bash
agent-bridge read tutorial-demo --lines 10      # default is 40
```

If the buffer has rotated, the output starts with
`--- tutorial-demo (older output rotated out) ---`.

`attach` is the interactive version: it mirrors the terminal and forwards your
keystrokes, so you can drive the agent by hand.

```bash
agent-bridge attach tutorial-demo
# ... work in the terminal ...
# Ctrl-] to detach (the agent keeps running)
```

`--from-seq <n>` controls the initial replay, it does not trim it: if `n` is at
or above the agent's current sequence number (the `seq` field from
`agent-bridge --json read`), the buffered output is not replayed at all, so you
only see output produced from now on. With a lower `n` the **whole** retained
buffer is replayed, and frames whose sequence is at or below `n` are dropped.

`attach` is interactive and has no `--json` mode.

## 8. Stop an agent

```bash
agent-bridge kill dev          # polite: Ctrl-C, then terminate the client
agent-bridge kill dev --force  # immediate: tear down the whole process tree
```

Plain `kill` writes Ctrl-C, waits up to half a second for the program to exit on
its own, and only then terminates the PTY client. Use it for shells and agents
that handle interrupts.

`--force` skips the polite path and kills the tree:

* **Windows**: the agent's job object is dropped and the kernel terminates every
  process in it, including grandchildren that opened their own console.
* **macOS/Linux**: `SIGKILL` to the agent's process group, plus every descendant
  found in a `ps` snapshot taken first, so jobs moved into their own process
  group are reached too.
* **Linux additionally**: every process that still holds the agent's PTY slave
  on stdin/stdout/stderr is killed, which also reaches a background job after
  the direct client has exited.

Killing an agent never takes the daemon down, and never touches other agents.
A process that daemonized and closed its terminal, or the post-exit job-control
case on macOS, is a documented gap - see [SECURITY.md](../SECURITY.md).

## 9. End-to-end: two agents reviewing a PR

This is the workflow the project exists for. Terminal 1 runs the developer
agent, terminal 2 the reviewer; you drive both from a third terminal.

```bash
# 1. two agents, each in its own checkout - one tree for both would make
#    them fight over the index and HEAD (assumes the repo is at
#    C:\work\agent-bridge)
git worktree add ..\agent-bridge-qa -b qa-review origin/dev
agent-bridge new dev --cwd C:\work\agent-bridge    -- opencode
agent-bridge new qa  --cwd C:\work\agent-bridge-qa -- opencode

# 2. ask the developer to push a branch and open a PR
agent-bridge sendmessage dev "implement the resize race fix, push a branch, open a PR into dev"

# 3. when the PR exists, hand it to the reviewer
agent-bridge sendmessage qa --from dev "PR #23 is up, CI green, please review the PTY shutdown path"

# 4. follow both terminals without attaching
agent-bridge read dev --lines 40
agent-bridge read qa  --lines 40

# 5. relay the verdict back
agent-bridge sendmessage dev --from qa "left 3 comments, the resize race is the important one"

# 6. who said what, in order
agent-bridge log --limit 20

# 7. done for the day
agent-bridge kill qa --force
agent-bridge kill dev --force
```

The point of the bridge is that neither agent needs to know about the other's
terminal, window or OS - it only needs a name and a message.

## 10. Scripting with `--json`

Every command except `attach`, `daemon stop`, `daemon run` and `daemon status`
while the daemon is down accepts the global `--json` flag and prints the raw
protocol response. Examples with `jq`:

```bash
# names of running agents
agent-bridge --json listagents | jq -r '.agents[] | select(.status=="running") | .name'

# how many agents are alive
agent-bridge --json listagents | jq '[.agents[] | select(.status=="running")] | length'

# last 5 lines of output, without the JSON wrapper
agent-bridge --json read dev --lines 5 | jq -r '.lines[]'

# messages from the last minute
agent-bridge --json log --limit 50 | jq -r '.messages[] | "\(.from // "owner") -> \(.target): \(.text)"'
```

Exit codes are `0` on success and `1` on failure, with the reason on stderr
(`error: agent 'x' not found`). That makes the CLI usable from CI steps and
scripts without parsing human-readable output.

## 11. Files, sockets and environment variables

| Item | Location / value |
| --- | --- |
| State directory | `~/.agent-bridge` (`%USERPROFILE%\.agent-bridge` on Windows) |
| Override state directory | `AGENT_BRIDGE_HOME` (any directory) |
| Daemon log | `<state>/daemon.log` (written when the daemon is started in the background) |
| Socket (Unix) | `<state>/daemon.sock`, directory `0700`, socket `0600` |
| Socket (Windows) | `\\.\pipe\agent-bridge-<user>`, DACL for the owner, `SYSTEM` and Administrators |
| Agent name inside the child | `AGENT_BRIDGE_AGENT` |

`AGENT_BRIDGE_HOME` is an environment variable, so export it the way your shell
does. Set it **before** the daemon starts: the daemon inherits the value, and a
CLI pointed at a different directory will start a second daemon on a different
socket (on Windows the pipe name comes from the user name, so the running daemon
keeps answering there).

```bash
# bash / zsh
export AGENT_BRIDGE_HOME=/tmp/ab-state

# cmd.exe
set AGENT_BRIDGE_HOME=C:\work\ab-state

# PowerShell
$env:AGENT_BRIDGE_HOME = "C:\work\ab-state"
```

The state directory is created on first use and is private to your OS user. On
Unix the socket is owner-only; on Windows the named pipe is created with an
explicit DACL for the owner, `SYSTEM` and Administrators.

## 12. Troubleshooting

**`agent '<name>' not found`** - the name is wrong, or the agent was never
created in this daemon. `agent-bridge listagents` shows the exact names.

**`agent '<name>' already exists`** - the daemon still remembers an agent with
that name, even if it has exited. Exited entries are kept for their output and
log until the daemon restarts, so either pick another name or
`agent-bridge daemon restart` (which also terminates every live agent). There is
no per-agent removal yet; that is tracked in
[issue #24](https://github.com/Shashi-kumar21/agent-bridge/issues/24).

**`daemon did not become ready; see ...daemon.log`** - the daemon failed to
start. The log has the reason; common causes are a stale socket from a killed
process, a full disk, or antivirus holding the binary.

**`daemon protocol N does not match client M, and K agent session(s) are still
managed by it`** - the CLI and the running daemon come from different versions.
The CLI restarts the daemon itself only when it manages **no** agent sessions at
all; exited entries count too, because `listagents` is never pruned (only a new
daemon clears them - a restart is the convenient way). Otherwise it refuses, because a restart would kill any
live agents and discard the exited entries' output; run
`agent-bridge daemon restart` when you are ready for that.

**Nothing appears in `read`** - the program has not printed anything yet, or it
is waiting for input. Use `attach` to see the screen and type into it.

**Windows: `Access is denied` while installing or replacing the binary** - a
daemon is still running from that path. Stop it first
(`agent-bridge daemon stop`, or `taskkill /F /IM agent-bridge.exe` if it is
wedged), then install again.

**Everything died when the daemon stopped** - expected. Agents are tied to the
daemon's lifetime on purpose; the job object (Windows) and the PTY hangup (Unix)
make sure no foreground terminal is left behind after a crash. Background
job-control jobs on Unix are a documented exception, as are processes that
detached themselves - see [SECURITY.md](../SECURITY.md).

## 13. Cheat sheet

| Task | Command |
| --- | --- |
| Start / check / stop the daemon | `agent-bridge daemon start` / `status` / `stop` |
| Create an agent | `agent-bridge new <name> [--cwd DIR] [-- <cmd...>]` |
| List agents | `agent-bridge listagents` (`list`, `ls`, `agents`) |
| Type into another terminal | `agent-bridge sendmessage <name> "text"` (`--from`, `--no-enter`) |
| Read the tail of its output | `agent-bridge read <name> --lines 60` |
| Drive it by hand | `agent-bridge attach <name>` (Ctrl-] detaches) |
| Kill it politely / immediately | `agent-bridge kill <name>` / `--force` |
| Message history | `agent-bridge log --limit 200` |
| Health and versions | `agent-bridge ping` / `agent-bridge version` |
| Machine-readable output | add `--json` (every command except `attach`, `daemon stop`/`run`, and `daemon status` while down) |

## 14. Where next

* [README](../README.md) - short overview, command table and protocol notes.
* [SECURITY.md](../SECURITY.md) - threat model, endpoint hardening and the
  documented gaps around process teardown.
* [AGENTS.md](../AGENTS.md) - the review loop this project is developed with,
  including how two agents coordinate over `sendmessage`.
