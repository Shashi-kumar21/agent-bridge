# AGENTS.md

Instructions for every OpenCode agent working in this repository. Read this
file before touching code.

## 1. The team

| Role | Identity | Responsibilities |
| --- | --- | --- |
| **Owner** | Shashi (GitHub `Shashi-kumar21`) | Product direction, final merge, secrets, releases. Never bypassed. |
| **QA / Senior reviewer** | OpenCode session **`dev`** becomes **QA**; the reviewer session is `ses_e8cfade4fffeR2w63iGi72dpXN` | Reviews PRs, guards architecture, writes review comments, approves or requests changes, never writes production code. |
| **Dev / Junior engineer** | OpenCode session titled `dev` (`ses_e8d0405b9ffeZTOAVxwJGtxeFn`) | Implements everything, writes tests, raises PRs, answers review comments, keeps CI green. |

Rules of engagement:

1. **Only Dev writes code.** QA may propose patches inside review comments but
   does not push commits.
2. **Only the Owner merges.** Neither agent runs `gh pr merge`.
3. Dev never approves its own PR; QA never merges.
4. If QA and Dev disagree twice, escalate to the Owner with a short summary of
   both positions.
5. Dependency and CI PRs (dependabot included) follow the same loop: Dev
   verifies, QA reviews, Owner merges. GitHub auto-merge stays off, and
   `portable-pty` is never bumped without the Windows smoke test in section 9.

## 2. Communication protocol

Three channels, in order of preference:

### 2.1 Pull request comments (durable, auditable)

* Dev opens the PR with a description: what changed, why, how it was verified,
  screenshots/CLI output, risk notes.
* QA reviews with **line comments** for code and a **summary comment** using
  exactly one of these verdicts on the first line:

  ```
  VERDICT: APPROVE
  VERDICT: REQUEST_CHANGES
  VERDICT: COMMENT
  ```

* Dev replies **in the same thread** to every comment and resolves it only when
  the fix is pushed and CI is green. Unresolved threads block merge.
* Back-and-forth continues until QA posts `VERDICT: APPROVE` **and** all checks
  are green **and** every thread is resolved. Only then Dev pings the Owner.

### 2.2 Terminal messages (fast, ephemeral)

```bash
agent-bridge sendmessage qa "PR #12 is up, CI green, please review the PTY shutdown path"
agent-bridge sendmessage dev "left 3 comments on PR #12, the resize race is the important one"
agent-bridge read qa --lines 40      # read the other terminal's screen
agent-bridge listagents              # who is online
```

Use terminal messages for pings and short questions; anything that must survive
a reboot belongs in the PR.

### 2.3 Escalation to the Owner

Only when blocked: merge needed, credentials needed, product decision needed.
Keep it to three lines: what is done, what is blocked, what decision is needed.

## 3. Branching model

| Branch | Purpose | Rules |
| --- | --- | --- |
| `main` | Always releasable | Protected: no direct pushes, PR required, checks required, linear history, conversation resolution, no force-push/delete |
| `dev` | Integration branch | All work lands here first via feature branches |
| `feat/*`, `fix/*`, `chore/*` | Short-lived work | Branch from `dev`, PR back into `dev` |

Flow:

```
feat/x ──PR──▶ dev ──PR──▶ main ──▶ Owner merges ──▶ tag
```

## 4. Definition of done

A PR is ready for QA only when **all** of these are true:

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all` (unit + `daemon_e2e` + `orphan_teardown`)
- [ ] New behaviour has tests; public items have doc comments
- [ ] `--json` output stays backward compatible (protocol version bump if not)
- [ ] README / AGENTS.md updated when commands or behaviour change
- [ ] No `unwrap()` on user input, no panics on malformed frames
- [ ] Platform-sensitive code compiles for Linux, macOS and Windows (CI proves it)

## 5. Code conventions

* Rust 2021, MSRV 1.85, `unsafe_code = "forbid"`; pedantic clippy is on.
* Errors: `BridgeError` for library code, `Response::Error` on the wire,
  `anyhow` only inside PTY plumbing.
* Never log message bodies at `info`; they may contain sensitive prompts.
* Wire compatibility: `PROTOCOL_VERSION` in `crates/ab-core/src/protocol.rs`
  must change whenever `Request`/`Response` changes incompatibly.
* Attach streams replay buffered output and then continue live, so the same
  `seq` can arrive twice; clients must drop non-increasing `seq` frames.
* Daemon lifecycle: a protocol-skew restart is allowed **only** when the daemon
  owns no agent sessions. Never add a path that silently kills live agents
  (a version bump is not a licence to destroy someone's terminal).
* Process lifetime: agents are tied to the daemon through
  `crates/ab-core/src/reaper.rs`. On Windows that is an anonymous job object
  (`win32job`, a safe wrapper - required because `unsafe_code = "forbid"`);
  closing the ConPTY only reaps the direct client, not grandchildren that
  opened their own console. On Unix the PTY hangup covers daemon death and
  `ProcessReaper::terminate` signals the agent's process group, then the
  descendants in a `ps` snapshot taken first (Linux falls back to
  `/proc/<pid>/stat` when `ps` is missing or failing), and on Linux finally
  every process that still holds the PTY slave - so job-control jobs are reaped
  while the direct client is alive, and on Linux also after it exited. macOS
  keeps that after-exit gap, and a process that already detached (daemonized) is
  a known gap everywhere (see `SECURITY.md`).
  `kill --force` tears the tree down through the same hook - do not replace it
  with a bare `child.kill()`, and do not replace the crates with raw FFI. On
  Linux the PTY fd sweep (`pty_path` / `pty_members` in `reaper.rs`) is what
  reaches a job-control job after the client exited; it is anchored on the slave
  number the daemon still owns, so it must run while the master is open. Keep
  `crates/ab-cli/tests/orphan_teardown.rs` green: on Windows all three tests
  fail if the job object stops working (including the exited-client case), on
  Unix they cover `SIGHUP` plus the group/tree sweep, and the job-control
  after-exit case is asserted reaped on Linux and still-leaked on macOS.
* Keep the daemon single-binary and dependency-light; new deps need a
  justification in the PR.

## 6. Repository map

```
crates/ab-core/src/protocol.rs  wire types (single source of truth)
crates/ab-core/src/daemon.rs    accept loop, request handling, attach streams
crates/ab-core/src/agent.rs     PTY spawn, output pump, resize, kill
crates/ab-core/src/reaper.rs    agent lifetime + `kill --force` tree teardown
crates/ab-core/src/transport.rs local socket / named pipe
crates/ab-core/src/client.rs    blocking client used by CLI and UI
crates/ab-cli/src/main.rs       command surface (clap)
```

## 7. Commands

```bash
cargo build                       # debug binaries
cargo test                        # unit + e2e
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo deny check                  # licenses, advisories, sources

# run the daemon manually while developing
cargo run -p agent-bridge -- daemon run     # foreground, logs to stderr
RUST_LOG=debug cargo run -p agent-bridge -- daemon run
```

## 8. Review loop (copy-paste)

```bash
# Dev: open the PR
gh pr create --base main --head dev --title "feat: ..." --body-file .github/pull_request_template.md

# QA: read it
gh pr diff
gh pr view --comments

# QA: leave a line comment / summary
gh pr review <num> --request-changes --body "VERDICT: REQUEST_CHANGES ..."

# Dev: address feedback, then re-request review
gh pr comment <num> --body "Addressed: ..."

# QA: approve
gh pr review <num> --approve --body "VERDICT: APPROVE"

# Owner: merge
gh pr merge <num> --squash --delete-branch
```

## 9. `portable-pty` pin (do not forget)

`portable-pty` is pinned to **exactly** `0.8.1` in the workspace `Cargo.toml`:
the 0.9 line returned no ConPTY output on Windows, so every agent terminal went
silent. `RUSTSEC-2017-0008` (unmaintained `serial`, not vulnerable) is therefore
ignored in `deny.toml` with that reason.

Three guards keep this from regressing:

* `Cargo.toml` uses `=0.8.1`, so a plain `cargo update` cannot move it.
* `.github/dependabot.yml` ignores `portable-pty`; do not remove that ignore
  without the smoke test below passing on the new version.
* The `deny` CI job fails if `Cargo.lock` does not resolve `portable-pty` to
  `0.8.1`.

Never merge a `portable-pty` bump on green CI alone - ConPTY output is not
covered by unit tests that a version bump would exercise. Run this on Windows:

```bash
agent-bridge daemon start
agent-bridge new smoke -- cmd.exe /c "echo conpty-ok"
agent-bridge read smoke --lines 5      # must contain conpty-ok
agent-bridge kill smoke --force
agent-bridge daemon stop
```

If a 0.9.x release passes, upgrade, delete the advisory ignore from
`deny.toml`, delete the dependabot ignore and the CI pin step, and confirm
`serial` is gone from `Cargo.lock`.
