## Summary

<!-- What does this change and why? Link the issue or ADR. -->

## Checklist

- [ ] One coherent change; no unrelated refactors
- [ ] Tests added or updated for the new behavior (negative tests for security-sensitive changes)
- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `cargo test --workspace --all-targets --locked` and `cargo test --workspace --doc --locked`
- [ ] `cargo deny check` (new dependencies went through the admission review)
- [ ] Architecture boundaries respected (domain/application stay free of HTTP, SQLx, vendor types)
- [ ] Docs/ADRs updated; `examples/hello-forge` re-blessed and reviewed if generator output changed
- [ ] No secrets, credentials or personal data in code, tests, fixtures or logs

## Validation

<!-- Paste the commands you ran and their results. -->
