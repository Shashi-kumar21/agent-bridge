# Security policy

## Supported versions

The latest commit on `main` is the only supported version.

## Reporting a vulnerability

Please email the maintainer instead of opening a public issue. Include:

* affected version / commit,
* reproduction steps,
* impact (local privilege boundary, message injection, DoS, ...).

You should receive a response within 72 hours.

## Scope

`agent-bridge` is a **local** tool: a daemon owns PTYs and the CLI drives it
over a local endpoint. It never binds a network interface (`transport.rs:63-103`).
The security boundary is the **OS user**: every process running as that user may
drive every agent, and no other user may reach the endpoint (`transport.rs:41`,
`transport.rs:92-98`).

## Assets

* **Terminal input.** Anything written to an agent's PTY is executed by that
  terminal. `sendmessage` is therefore a code-execution primitive on purpose.
* **Terminal output.** May contain secrets, credentials and untrusted
  instructions. `read`, `attach` and `--wait-reply` expose it to any same-user
  client.
* **Message records.** Text, target, sender label and delivery status for the
  last 2000 messages (`daemon.rs:26`), plus the newest 500 records held per agent
  (`agent.rs:25`). Memory only.
* **The daemon process** and the agents it owns. They run with the user's full
  privileges, and agents inherit the daemon's environment (see the gaps).

## Actors and trust boundaries

| Actor | Trust | Reach |
| --- | --- | --- |
| The owner (same OS user) | Trusted by design | Everything |
| Other OS users | Untrusted | Nothing: denied by the socket mode / pipe DACL |
| An agent process | Same privileges as the user | Can open the endpoint itself, like any process |
| Message content | Untrusted data | Executed as keystrokes in the target terminal |
| Terminal output | Untrusted data | Rendered by consumers (CLI, editors, models) |

There is no authentication or authorization beyond the endpoint's access
control. A request can name any agent, and `from` is stored exactly as supplied
and never verified (`protocol.rs:290-291`); treat it as an unauthenticated
label. The roadmap's §4 policy layer is what would add per-session scopes.

## Threats and mitigations

### T1. Same-user processes

Any process running as the same OS user can call every request: create, send,
read, attach, kill and shutdown (`daemon.rs:212-528`). This is accepted: the
boundary is the account, not the socket, so malware under that account is
equivalent to the user. What is enforced is that *other* users cannot connect -
`0700` state directory and `0600` socket on Unix (`paths.rs:51-61`,
`transport.rs:79-98`), an explicit owner/`SYSTEM`/Administrators DACL on
Windows (`transport.rs:41`, `transport.rs:46-61`).

### T2. Compromised or malicious agent

An agent is a normal process under the same user, so it can connect to the
endpoint and drive every other agent, read their output, or shut the daemon
down. There is no per-agent token or scoped credential; `AGENT_BRIDGE_AGENT` is
a convenience label, not identity (`agent.rs:143-158`), and nothing separates an
agent's requests from the user's. The containment that does exist is process
lifetime: `kill --force` tears a tree down and `daemon stop` ends every terminal
(see **Process lifetime**). Per-session authorization is roadmap §4 and is not
implemented.

### T3. Malicious prompt content

Output is raw terminal data. Line reads strip escape sequences, but cursor
reads return the raw bytes and `--json` returns the fields verbatim, so a
consumer that renders them in a terminal (or feeds them to a model) can be
driven by the content: control sequences, fake prompts, and prompt-injection
instructions all pass through. The daemon never interprets output as
instructions and performs no semantic validation - it cannot tell safe text
from hostile text.

Prompt injection propagates between agents and their consumers: an agent can
print text that another agent (or a human's assistant) later reads via `read`,
and nothing marks it as untrusted. **The bridge does not validate the semantic
safety of agent instructions.** Consumers must treat every byte from the
endpoint as hostile input and must not execute instructions found in output.
Mitigations today are bounded retention (256 KiB per agent, `buffer.rs:8-34`)
and the fact that the tool never reads output on its own.

### T4. IPC endpoint access

* Unix: state directory `0700`, socket `0600` (`paths.rs:51-61`,
  `transport.rs:92-98`). A stale socket is removed only while no daemon answers
  on it, so a live daemon cannot be stomped (`transport.rs:79-88`).
* Windows: the named pipe is created with
  `D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)` - owner, `SYSTEM` and
  Administrators only, inheritance disabled (`transport.rs:41`). If the
  descriptor cannot be built the daemon falls back to the OS default and logs a
  warning (`transport.rs:46-61`), which is broader than this tool needs.
* `AGENT_BRIDGE_HOME` and `AGENT_BRIDGE_PIPE` choose the endpoint. They are
  per-process and controlled by the caller, so they do not cross a privilege
  boundary, but they do let a user point the CLI at a different daemon.
* Residual risk: on Windows, `Administrators` and `SYSTEM` can drive agents. On
  every platform, denial for a *different* user is not exercised by CI yet (see
  the gaps).

### T5. Accidental command injection

The daemon writes the message bytes plus a trailing Enter by default; it never
interprets them through a shell (`daemon.rs:256-432`, `delivery::compose_payload`).
The risk is semantic: the bytes are typed into a live terminal. Newlines submit
commands, and with bracketed paste disabled each line executes as it arrives;
`--paste` forces the bracketed form even for one line, while the default `auto`
wraps only multi-line text and only when the target enabled bracketed paste
(`delivery.rs:121-123`). A message sent to a shell that is not at a prompt is
still typed into it.

Target selection is by name. A live name is unique (`daemon.rs:239-241`) and an
exited agent's entry is kept, so writing to it fails with `conflict` instead of
reaching a recycled process (`agent.rs:430-432`). There is no confirmation mode,
dry run or per-session policy yet (`--wait-idle` is a heuristic and gates
nothing), so a mistyped target or a message with unexpected newlines executes
immediately. Bounds keep the blast radius finite: 64 KiB per message by default
(`delivery.rs:19`, `delivery.rs:78-87`), 1 KiB per ack note (`daemon.rs:30`),
2000 logged messages (`daemon.rs:26`) and 256 KiB of output per agent
(`buffer.rs:8-34`).

## Process lifetime

Agent lifetime is tied to the daemon, so a crashed or force-killed daemon does
not leave hidden terminals behind (`crates/ab-core/src/reaper.rs`):

* **Windows** - each agent is assigned to an anonymous job object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When the daemon exits, crashes, or is
  terminated from Task Manager, the kernel kills the agent and every descendant
  in the job, including processes that opened their own console. Two caveats:
  the assignment happens right after `CreateProcessW`, so a process forked in
  that brief window is outside the job; and the job does not set
  `JOB_OBJECT_LIMIT_BREAKAWAY_OK`, so a child that asks for
  `CREATE_BREAKAWAY_FROM_JOB` now fails with `ERROR_ACCESS_DENIED` instead of
  escaping (some installers/launchers do this).
* **Unix** - `portable-pty` makes the agent a session leader with the PTY as its
  controlling terminal. Closing the master on daemon death makes the kernel
  send `SIGHUP` to the foreground process group.
* If the OS refuses the link (for example a restricted job object on a locked
  down host), the daemon logs a warning and spawns the agent anyway; refusing to
  run a terminal would be worse than running it without the safety net.

`agent-bridge kill --force` tears the agent's process tree down while the daemon
stays up: Windows drops the job handle (the kernel terminates the job), Unix
sends `SIGKILL` to the agent's process group, then to every descendant found in
a process snapshot taken just before the signals (`ps`, falling back to
`/proc/<pid>/stat` on Linux when `ps` is missing or failing), and on Linux
finally sweeps every process that still holds the agent's PTY slave open on
stdin/stdout/stderr. While the direct client is still running, the first two
passes also reach the jobs an interactive shell moved into their own process
group and children that called `setsid`. Once the client has exited its pid and
process group may name an unrelated tree, so the snapshot is skipped - but the
Linux PTY sweep still finds the tree, because the slave number cannot be reused
while the daemon holds the master and a background job keeps the terminal on its
standard streams. macOS has no `/proc`, so a job-control job left behind there
survives (see the known gaps). `kill` without `--force` still tries Ctrl-C first
and then only terminates the PTY client, so children of a stubborn agent survive
that path by design.

Output buffers and message bodies are memory only: they are never written to
disk. The daemon log records lifecycle events and errors, not terminal contents
or message bodies, and message text is never logged at `info`.

## Non-goals

* Defending against malware or an attacker who already controls the same OS
  user account (T1).
* Remote or multi-user access, which would need an authenticated transport
  (roadmap §10).
* Sandboxing agents. They run with the user's full privileges; environment
  inheritance is a known gap below.
* Validating prompt semantics or guaranteeing that an agent obeys an
  instruction (T3).
* Per-session authorization, confirmation of sensitive actions and an audit
  trail. These are roadmap §4 and are not implemented yet.

## Known gaps (tracked, not yet implemented)

* Unix job-control job after the client exited: **closed on Linux**, where the
  PTY fd sweep (`/proc/<pid>/fd/0..2`) still finds the job after the client, its
  process group and its session are gone. Still open on macOS - enumerating
  another process's descriptors there needs `lsof` or FFI - and for a process
  that closed or redirected all three standard streams, which no longer carries
  the PTY anchor.
* Unix snapshot window: the tree comes from a process snapshot, so a process forked
  after it is not signalled, and a snapshotted pid that exits and is recycled
  before the sweep can be signalled instead. Best-effort by design.
* Unix `ps` unavailable or failing: **closed on Linux**, where the tree is read
  from `/proc/<pid>/stat` instead, so a container without procps still gets the
  snapshot `ps` would have produced. macOS has no `/proc` (enumerating it would need `lsof` or FFI),
  so there the snapshot is skipped and only the process-group signal runs. When
  both sources fail - Linux with an unreadable `/proc` - `descendants()` logs one
  warning with the `ps` failure.
* Unix already-detached processes: a process that daemonized (double-fork,
  reparented to init) before `kill --force` is not in the snapshot and survives.
  Following it needs cgroups on Linux, or a supervisor / process-enumeration
  mechanism elsewhere; the Windows job object is the only mechanism here that
  keeps daemonized children in the tree.
* Unix daemon death: only the foreground process group gets `SIGHUP`, so a
  job-control background job survives a daemon crash. `kill --force` still reaps
  it - while the direct client is running everywhere, and after it exited on
  Linux; on macOS that post-exit case is the gap above. Reaping on daemon death
  itself would need a supervisor outside the daemon.
* Running agents are not re-adopted after a daemon restart.
* No optional token handshake for shared machines.
* Denial for a *different* OS user is not exercised by CI yet; only the
  descriptor's validity and same-user connectivity are tested.
* **Environment inheritance.** Spawned agents inherit the daemon's environment;
  only `TERM` and `AGENT_BRIDGE_AGENT` are set explicitly (`agent.rs:143-158`),
  so every secret exported before the daemon started reaches every agent. An
  allowlist or scrub is roadmap §4.
* **Unbounded request frame.** The daemon reads one JSON line per connection
  with no length cap (`daemon.rs:952-954`). Message bodies are bounded at
  64 KiB (`delivery.rs:19`, `delivery.rs:78-87`), but a same-user client can
  still grow daemon memory with an oversized frame before that check runs.
* **No read authorization or redaction.** Any same-user client can read every
  agent's output; output that contains secrets is returned verbatim and is not
  filtered.
* **No audit trail.** Session creation, writes, reads, attach and termination
  are not recorded with a caller identity, and there is no redaction policy.
