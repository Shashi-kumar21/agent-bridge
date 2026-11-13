# Roadmap

Source: the Owner's task document, imported 2026-11. **Section order is
execution order**; a phase starts only when the previous one meets its acceptance
criteria and the Definition of Done (Section 11). Planned work described here is
not implemented until the checklist item lands with tests and documentation.

Status: Phase 1 (audit and architecture map) is complete as of the commit that
added this file. Phases 2-9 are open; Section 10 is explicitly out of scope until
P0/P1 is stable.

See [`architecture.md`](architecture.md) for the current implementation and
[`known-limitations.md`](known-limitations.md) for the limitations this roadmap
addresses.

---

> **Goal:** Evolve Agent-Bridge from a cross-platform terminal-session
> manager into a reliable, secure, agent-accessible communication
> runtime, without rewriting the existing Rust architecture.
>
> **Working rule:** Inspect the current implementation before changing
> it. Preserve working behavior, public CLI compatibility, and the
> existing cross-platform design unless a change is justified by tests
> or a documented limitation.
>
> **Section order = execution order.** Follow the sections top to
> bottom. Do not start a later phase before the earlier phase meets its
> acceptance criteria and the Definition of Done (Section 11).

## 0. Instructions for the coding agent

1.  Read the repository's `README`, `CONTRIBUTING`, `SECURITY`, architecture/tutorial docs, workspace `Cargo.toml`, source code, tests, and CI workflows before editing.
2.  Map the current request path: CLI -> client -> IPC -> daemon -> session manager -> PTY, and map the reverse output path.
3.  Identify which items below are already implemented, partially implemented, or missing. Do not duplicate existing functionality.
4.  Work in small, reviewable changes. Add or update tests with every behavior change.
5.  Do not claim a feature is complete unless it is implemented, tested, documented, and works on the relevant operating systems.
6.  Do not silently change existing command behavior or protocol formats. If a breaking change is necessary, version it and document migration.
7.  Do not add a network listener, remote access, automatic command execution, or broad permissions as part of this task.
8.  Before each phase, state the files likely to change and the design. After each phase, run formatting, linting, tests, and relevant platform checks; report exact commands and results.
9.  Never use real API keys or production credentials in tests. Use mock agents and deterministic fixtures.
10. If a requirement conflicts with the current architecture, explain the trade-off and propose the smallest safe implementation rather than forcing a rewrite.
11. **Git workflow:** branch from `dev`, open one pull request per phase (or per clearly separable sub-phase) into `dev`. `main` accepts only reviewed PRs from `dev`. Never push directly to `main`.
12. Prefer adding new CLI subcommands/flags and new protocol fields over changing existing ones. New protocol fields must be optional so old clients keep working; bump the protocol version when behavior changes.
13. Do not use wording such as "guaranteed", "secure", "reliable", or "production-ready" in code comments, docs, or commit messages unless a test demonstrates exactly that property.

## 1. Baseline audit and architecture map

**Priority: P0 - do first. No behavior changes in this phase.**

-   [x] Inspect the complete Rust workspace and identify the responsibilities of `ab-core` and `ab-cli`.
-   [x] Trace daemon startup/shutdown and ownership of child processes and PTY handles.
-   [x] Trace how sessions are registered, looked up, read, written to, and removed. Note name-uniqueness and name-reuse behavior.
-   [x] Inspect IPC framing, request/response types, protocol versioning, timeouts, and error handling.
-   [x] Inspect output buffering, ANSI handling, Unicode handling, and concurrent reads/writes.
-   [x] Inspect Windows ConPTY, named-pipe ACL/DACL, Unix PTY, socket permissions, and process-tree cleanup.
-   [x] Inspect which environment variables and working directory a spawned agent inherits from the daemon.
-   [x] Review all existing tests, CI jobs, open issues, and documented limitations.
-   [x] Create `docs/architecture.md` containing a concise component diagram, request lifecycle, output lifecycle, process ownership model, and platform-specific implementation notes.
-   [x] Create `docs/known-limitations.md` with confirmed limitations and reproduction steps.

**Deliverable:** A baseline report with current behavior, risks, test gaps, and a
prioritized implementation plan. Do not make broad refactors during this phase.

**Acceptance criterion:** Every claim in `architecture.md` and `known-limitations.md`
points to a source file/line or a reproducible command.

## 2. Reliable message delivery

**Priority: P0 - highest functional priority**

The current terminal-input approach must not be described as guaranteed agent-to-agent
delivery. Writing bytes to a PTY only confirms that input was written; it does not prove
that the target application accepted, understood, or completed the request.

Implement a minimal, explicit delivery lifecycle:

-   [x] Add a unique `message_id` to every message.
-   [x] Add a per-session write queue so simultaneous senders cannot interleave bytes.
-   [x] Ensure each message is written atomically at the byte level (one writer, one `write_all` per message). Application-level acknowledgement is still open below.
-   [x] Support multi-line messages safely: bracket-paste (`ESC[200~`..`ESC[201~`) when the target announces it (`ESC[?2004h`), with `--paste bracketed` and the documented `--paste plain` fallback.
-   [x] Return a clear result such as `queued`, `written`, `failed`, or `unknown`; do not label `written` as `delivered` or `acknowledged`.
-   [x] Add optional application-level acknowledgement only where a receiving agent integration can explicitly send one (`--require-ack` for `awaiting_ack`, `ack <target> <id> [--note]` for `acknowledged`, idempotent and target-checked).
-   [x] Add configurable write timeouts and bounded queue sizes.
-   [x] Define queue behavior when the target exits, is unavailable, or the queue is full.
-   [x] Prevent unbounded memory growth and expose queue length/status (`AGENT_BRIDGE_QUEUE_*`, `listagents` queue depth, `messagestatus`/`pending` for status).
-   [x] Add tests for multi-line input (the other cases - concurrent senders, large messages, Unicode, target exit, timeout, queue overflow - were covered in Phase 2B).

**Suggested conceptual API (adapt to existing conventions):**

``` text
send_message(target, text, options) -> message_id
get_message_status(message_id)
list_pending_messages(target)
```

Do not introduce a misleading guarantee of exactly-once delivery. Document the actual
semantics, including cases where the result is uncertain.

**Acceptance criterion:** N concurrent senders each sending M messages to one target
produce zero interleaved bytes in the target's input, and every message ends in a
documented terminal status (`written`, `failed` or `unknown`; `queued` is a transient
state). The test is deterministic and runs on all three CI operating systems.

## 3. Readiness, replies, and output cursors

**Priority: P0 - required for usable agent-to-agent workflows**

Atomic, queued writes still do not tell the sender whether the target is ready for
input. A message can land while the target is generating output or waiting at a
permission prompt. Readiness detection is heuristic and must be documented as such.

-   [x] Detect idle/busy state from output quiescence (configurable quiet period) and optional per-agent prompt patterns.
-   [x] Expose the state in agent status as `busy`, `idle`, or `unknown`, never as a guarantee (`listagents`, `--json` `readiness`, `AgentInfo`).
-   [x] Add `sendmessage --wait-idle [--timeout]` that holds the message until the target appears idle or the timeout expires; it is checked before the message is queued and a timeout fails with `timeout` without writing.
-   [x] Add `--wait-reply [--timeout]` that, after a message is written, waits for output to settle and returns the new output since the write (best effort, bounded to 64 KiB / 200 lines, with explicit `timed_out` / `settled` / `truncated`).
-   [ ] Add output offsets and a cursor-based read: `read <name> --since <offset>` returns new output and the next offset, so callers do not re-read the same data.
-   [ ] Add a bounded follow/stream mode (or long-poll) suitable for MCP clients, with clear behavior when the buffer wraps and older offsets are no longer available.
-   [x] Make the quiet period, prompt patterns, and buffer sizes configurable, with safe defaults (`AGENT_BRIDGE_QUIET_MS`, `AGENT_BRIDGE_PROMPT_PATTERN`, `AGENT_BRIDGE_BUFFER_BYTES`).
-   [ ] Add tests using a mock agent that alternates busy/idle output, prints prompts, and goes silent; cover timeouts and offset expiry. (Readiness, wait-idle timeout/unknown, and wait-reply settled/timeout/exit-drain cases are covered; offset expiry lands with the cursor item above.)

**Acceptance criterion:** With a deterministic mock agent, `--wait-idle` never writes
during a scripted busy period, and `--since` returns each output byte exactly once
across repeated reads. Documentation states that idle detection is a heuristic.

## 4. Security and permissions

**Priority: P0 - required before any agent-controlled tool is exposed (including MCP
read tools)**

Terminal input can execute commands, and terminal output can contain secrets and
untrusted instructions. Treat `send_message` as a high-impact action, and treat `read`
as a sensitive operation too.

-   [ ] Write a threat model in `SECURITY.md` covering same-user processes, compromised agents, malicious prompt content, IPC endpoint access, and accidental command injection.
-   [ ] Verify Unix socket directory/file permissions and Windows named-pipe DACLs in automated platform tests where feasible.
-   [ ] Specify the policy design **before** implementing it: file location, format (for example TOML), schema, precedence, and secure defaults. Record it in `docs/policy.md`.
-   [ ] Add per-session authorization policies for read, write, create, attach, and terminate operations. Reads are authorized, size-bounded and labelled as untrusted content.
-   [ ] Default new sessions to the least privilege necessary.
-   [ ] Add an optional confirmation mode for sensitive actions.
-   [ ] Ensure caller identity is derived from a trusted OS/IPC mechanism where available (peer credentials on Unix sockets, client token/SID on Windows named pipes); never trust a caller-supplied `from` field as authentication. Treat `from` as an unauthenticated label.
-   [ ] Add bounded message sizes, request limits, timeouts, and safe path validation for working directories.
-   [ ] Sanitize or allowlist the environment variables inherited by spawned agents so the daemon's environment does not leak secrets into every child. Document the default and how to override it.
-   [ ] Define agent naming rules: allowed characters and length, uniqueness, reserved names, and reuse after an agent exits.
-   [ ] Avoid logging secrets or unrestricted terminal contents by default.
-   [ ] Add audit events for session creation, message writes, reads, attach, and termination, with configurable redaction.
-   [ ] Add structured diagnostics (for example via `tracing`) with configurable levels, kept separate from the audit log.
-   [ ] Add a fuzz target (for example `cargo-fuzz`) for the IPC frame and request parser, and keep a small seed corpus in the repository.
-   [ ] Document that prompt injection can propagate between agents and that the bridge does not itself validate the semantic safety of agent instructions.

**Acceptance criterion:** Unauthorized clients cannot perform actions outside their
configured permissions. Add a negative test for each protected operation (read, write,
create, attach, terminate), and run the fuzz target for a documented duration without
crashes.

## 5. MCP server mode

**Priority: P0 - major usability improvement (after Section 4)**

Expose Agent-Bridge capabilities as tools that MCP-compatible clients can invoke. MCP
should be an adapter over the existing core APIs, not a second implementation of session
management.

-   [ ] Inspect the current MCP specification and select a maintained Rust MCP SDK compatible with the repository's toolchain and license policy (`deny.toml`).
-   [ ] Add an optional MCP server entry point or CLI subcommand; keep the existing daemon and CLI usable without MCP.
-   [ ] Initially expose read-oriented tools, all subject to the Section 4 authorization, bounds, and audit path:
    -   `list_agents`
    -   `get_agent_status`
    -   `read_agent_output` (supports the `--since` cursor)
-   [ ] Add controlled write/action tools only after the security model is implemented and tested:
    -   `send_message`
    -   `create_agent`
    -   `terminate_agent`
-   [ ] Validate all tool arguments, target names, message sizes, and working directories.
-   [ ] Return structured, bounded results with clear errors. Label terminal output returned to the client as untrusted content.
-   [ ] Ensure MCP tool calls use the same authorization and audit path as CLI calls.
-   [ ] Document configuration for at least one MCP-compatible client using placeholders, not real secrets.
-   [ ] Add protocol-level tests and an end-to-end test with a mock MCP client.

**Security requirement:** Do not expose arbitrary shell execution as an MCP tool.
Creating or terminating sessions must require explicit policy authorization. Do not
allow an agent to grant itself permissions.

**Acceptance criterion:** A mock MCP client can list agents, read output with a cursor,
and (when policy allows) send a message; every denied call returns a structured error
and an audit event.

## 6. Correct terminal output and screen reading

**Priority: P1 - important for interactive/TUI agents**

Stripping ANSI escape sequences from a raw output stream is not equivalent to
reconstructing the current terminal screen. Full-screen applications redraw content and
move the cursor.

-   [ ] Reproduce current `read` issues using a deterministic test program that emits cursor movement, erase-line, erase-screen, colors, Unicode, and split escape sequences.
-   [ ] Evaluate a maintained VT-compatible terminal parser/emulator crate (for example, `vt100`, or another suitable maintained option). Verify license, maintenance, Windows compatibility, and API before selecting it.
-   [ ] Maintain a bounded screen grid with rows, columns, cursor position, and relevant terminal attributes.
-   [ ] Handle terminal resize events.
-   [ ] Preserve a separate raw-output/log stream where useful; do not conflate raw bytes with the rendered screen.
-   [ ] Add an explicit command or output mode for a screen snapshot, while preserving existing `read` behavior or versioning any incompatible change.
-   [ ] Add tests for partial escape sequences, alternate screen behavior where supported, Unicode width, resize, large output, and buffer limits.
-   [ ] Document unsupported terminal features and known rendering differences.

**Acceptance criterion:** For the supported VT subset, a test program's final visible
screen should match the parser's screen snapshot. Do not claim full terminal-emulator
compatibility unless tested.

## 7. Daemon and session lifecycle resilience

**Priority: P1**

First document the current lifecycle contract: whether child agents are intentionally
terminated when the daemon exits. Do not promise daemon-independent sessions unless the
architecture actually supports them.

-   [ ] Add graceful shutdown handling and deterministic cleanup.
-   [ ] Ensure repeated shutdown/termination requests are safe and idempotent.
-   [ ] Detect child exit and publish a final session status and exit code where available.
-   [ ] Persist minimal session metadata if useful, but never persist PTY handles as if they were recoverable.
-   [ ] Evaluate a separate PTY-holder/supervisor process only after documenting the complexity and platform differences.
-   [ ] If session adoption is feasible on a platform, implement it behind a platform capability check; otherwise document why it is unsupported.
-   [ ] Add crash/kill tests and verify there are no orphaned processes or stale IPC endpoints.
-   [ ] Add daemon health/status reporting (uptime, session count, queue depths) and clear stale-socket recovery behavior.

**Acceptance criterion:** After killing the daemon in a test, the documented lifecycle
contract holds on every OS: either children are cleaned up or they are adoptable, and
the next daemon start recovers from a stale endpoint without manual steps.

## 8. Testing, CI, compatibility, and performance evidence

**Priority: P0/P1 - runs alongside every phase above**

-   [ ] Keep CI green on Windows, macOS, and Linux.
-   [ ] Run `cargo fmt --check`, Clippy with warnings denied, unit tests, integration tests, and dependency/license checks (`cargo-deny`).
-   [ ] Add deterministic mock-agent tests; avoid requiring paid LLM APIs in CI.
-   [ ] Add Windows ConPTY integration coverage and Unix PTY integration coverage.
-   [ ] Add stress tests for many sessions, concurrent readers/writers, repeated start/stop, and daemon shutdown.
-   [ ] Add tests for Unicode, large output, long-running sessions, terminal resize, and malformed IPC frames.
-   [ ] Add manual or scheduled (nightly) smoke tests against real agent CLIs (for example Claude Code, OpenCode, and at least one other), kept out of required PR checks and never using committed credentials.
-   [ ] Publish `docs/compatibility.md`: a matrix of tested agents, versions, and OSes, with known quirks (paste handling, prompt patterns, redraw behavior) and the date last verified.
-   [ ] Add benchmarks for session startup, message write latency, output-read latency, and memory use under a documented workload.
-   [ ] Report methodology, hardware/OS, sample size, and results. Do not publish unsupported performance claims.

## 9. Installation, releases, and developer experience

**Priority: P1**

-   [ ] Add a short demo (terminal recording or GIF) showing two mock or real terminal agents exchanging a task and result.
-   [ ] Add a clear comparison explaining where Agent-Bridge complements IDEs, tmux, Zellij, MCP, and agent-native team features.
-   [ ] Add GitHub Release automation for supported OS/architecture binaries, with checksums and provenance where practical.
-   [ ] Document installation paths. Consider crates.io, Homebrew, winget, or Scoop only after package ownership and release automation are ready.
-   [ ] Keep `cargo install --path` documented for contributors.
-   [ ] Add a quick-start that works from a clean environment and clearly states prerequisites.
-   [ ] Add troubleshooting for daemon startup, IPC permissions, PTY failures, and agent exit.
-   [ ] Add a worked example: two agents where one requests a review and the other replies, using `--wait-idle` and `--wait-reply`.

## 10. Optional future work - do not start until P0/P1 is stable

-   [ ] TUI or desktop dashboard for sessions, screen snapshots, messages, and logs.
-   [ ] Remote transport over SSH or authenticated TLS, with an explicit remote threat model.
-   [ ] Agent adapters for structured APIs where available; retain PTY support as a compatibility fallback.
-   [ ] A2A integration if it solves a concrete interoperability requirement. MCP and A2A are different protocols and should not be treated as interchangeable.
-   [ ] Resource monitoring and configurable session limits.
-   [ ] Workspace/task isolation and policy-based working-directory access.

## 11. Definition of done

A phase is complete only when:

-   [ ] Implementation is merged into `dev` via a reviewed PR without unnecessary architectural rewrites.
-   [ ] Tests cover success, failure, concurrency, and relevant platform behavior, and the phase's acceptance criterion is met.
-   [ ] Formatting, linting, and CI pass.
-   [ ] CLI/API and protocol changes are documented.
-   [ ] Security implications and limitations are documented.
-   [ ] No feature is described as reliable, secure, cross-platform, or production-ready beyond what the tests demonstrate.
-   [ ] A concise implementation report lists changed files, design decisions, tests run, results, and remaining risks.

## Execution order (summary)

1.  **Audit** (Section 1): architecture map, known limitations.
2.  **Reliable messaging** (Section 2): IDs, queues, status semantics.
3.  **Readiness and cursors** (Section 3): idle detection, wait-reply, `--since`.
4.  **Security** (Section 4): policy, identity, audit, fuzzing.
5.  **MCP adapter** (Section 5): read tools first, then gated write tools.
6.  **Screen model** (Section 6): VT parsing and snapshots.
7.  **Lifecycle resilience** (Section 7).
8.  **Testing and compatibility evidence** (Section 8): continuous, with a final consolidation pass.
9.  **Demo, binaries, releases, docs** (Section 9).

**Do not attempt every phase in one giant change.** Complete and validate one phase at a
time, keeping the project buildable and usable after each phase.
