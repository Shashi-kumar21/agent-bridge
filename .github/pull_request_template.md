## What changed

<!-- One paragraph. Link the issue/task if there is one. -->

## Why

<!-- User-facing motivation, or the bug that motivated the change. -->

## How it was verified

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all`
- [ ] Manual check (paste output below)

```
$ agent-bridge ...
```

## Risk / rollback

<!-- Protocol change? Platform-specific? Anything QA must look at twice. -->

## Checklist

- [ ] Tests cover the new behaviour
- [ ] Public items documented, `--json` compatibility preserved
- [ ] README / AGENTS.md updated if commands or behaviour changed
- [ ] No new dependency without justification
- [ ] No secrets, prompts or tokens in logs or fixtures
