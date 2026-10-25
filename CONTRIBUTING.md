# Contributing

## Branch flow

```
feat/<topic> ──PR──▶ dev ──PR──▶ main
```

* `main` is protected (required checks, linear history, resolved
  conversations). Never push to it directly.
* Work always starts from `dev`.
* Commits follow Conventional Commits: `feat:`, `fix:`, `chore:`, `docs:`,
  `refactor:`, `test:`, `ci:`.

## Before opening a PR

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
cargo deny check
```

## PR expectations

* Title: Conventional Commit style.
* Body: use `.github/pull_request_template.md`.
* One logical change per PR; keep diffs reviewable (< ~400 lines when possible).
* Any protocol change must bump `PROTOCOL_VERSION` and update
  `crates/ab-core/src/protocol.rs` docs.

## Review process

Two agents are involved (see `AGENTS.md`): Dev implements, QA reviews. Review
verdicts are `VERDICT: APPROVE`, `VERDICT: REQUEST_CHANGES` or
`VERDICT: COMMENT` on the first line of the review body. Threads are resolved
by the author once the fix is pushed and CI is green. The owner merges.

## Reporting security issues

Do not open a public issue. See `SECURITY.md`.
