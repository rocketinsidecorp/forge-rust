# Contributing to Forge

Thanks for helping build Forge. This project is MIT-licensed; by submitting a
contribution you agree that it is licensed under the same [MIT License](LICENSE)
(inbound = outbound). No CLA is required.

## Before you start

- Read [CLAUDE.md](CLAUDE.md) / [AGENTS.md](AGENTS.md) — they are the binding
  engineering rules for humans and coding agents alike — and the relevant
  [ADRs](docs/adr/README.md). Decisions that change public contracts, security
  or several crates need a new ADR.
- For anything larger than a small fix, open an issue first so the design can be
  agreed before you invest time.
- Security vulnerabilities must **not** be reported in public issues or PRs; see
  [SECURITY.md](SECURITY.md).

## Development

The toolchain is pinned in `rust-toolchain.toml`. Run the same gates CI runs:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo install --locked cargo-deny && cargo deny check
```

Database integration tests are `#[ignore]`d unless their URLs are set
(`FORGE_TEST_DATABASE_URL`, `FORGE_TEST_MYSQL_URL`, `FORGE_TEST_MARIADB_URL`);
run them with `-- --include-ignored`. See [docs/test-strategy.md](docs/test-strategy.md).

## Pull requests

- Keep each PR to one coherent change with tests; avoid unrelated refactors.
- Use conventional commit messages (`feat:`, `fix:`, `docs:`, `deps:`, `ci:` …).
- New dependencies go through the
  [dependency admission review](docs/dependency-policy.md) and must pass
  `cargo deny check`.
- Generated-code changes must keep `examples/hello-forge` in sync
  (`FORGE_BLESS=1 cargo test -p forge-cli`) and you must review the diff.
- Never commit secrets, credentials or real customer data, including in tests,
  fixtures or snapshots.
- `main` is protected: every change lands through a pull request with passing
  checks and a code-owner review. Workflows from first-time and external
  contributors run after a maintainer approves them.
