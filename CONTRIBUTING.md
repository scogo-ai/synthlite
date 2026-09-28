# Contributing to synthlite

Thanks for helping. Bug reports, docs fixes, and pull requests are all welcome. For a security issue, do not open a public issue; follow [SECURITY.md](SECURITY.md) instead.

## Prerequisites

- Rust 1.89 or newer (the `rust-version` in `Cargo.toml`). Install with [rustup](https://rustup.rs); CI builds on current stable and checks 1.89.
- git.

Nothing else. The test suite needs no API keys and no network.

## Build and check

```sh
cargo build                               # debug build at target/debug/synthlite
cargo build --release --locked            # the binary we ship
cargo fmt --all                           # format
cargo clippy --all-targets -- -D warnings # lint; warnings fail CI
cargo test                                # full suite
```

Provider and Hugging Face Hub traffic in tests goes to local [wiremock](https://crates.io/crates/wiremock) fakes, so `cargo test` runs offline. Never put a real key, token, or private prompt in a test or fixture.

If you add or bump a dependency, run `cargo deny check` ([cargo-deny](https://github.com/EmbarkStudios/cargo-deny)). CI fails on a license outside the list in `deny.toml` or on a known advisory.

## Docs change with behavior

`README.md` and `docs/spec.md` are the contract users rely on. A pull request that changes a flag, default, config key, environment variable, output file, exit code, or refusal message updates those docs in the same pull request. Add a line under `## [Unreleased]` in `CHANGELOG.md` for anything a user would notice.

## Sign your commits (DCO)

Every commit must carry a `Signed-off-by` line. It certifies the [Developer Certificate of Origin](https://developercertificate.org): you wrote the change or have the right to submit it under the project license.

```sh
git commit -s -m "Explain what changed and why"
```

Forgot? `git commit --amend -s` fixes the last commit; `git rebase --signoff main` fixes a branch.

## Pull request flow

1. For anything larger than a small fix, open an issue or a discussion first so we can agree on the shape before you write code.
2. Fork the repo and branch from `main`.
3. Keep each pull request to one change, with tests that fail without it.
4. Run fmt, clippy, and test locally, then open the pull request and fill in the template.
5. CI runs rustfmt, clippy, tests on Linux and macOS, the MSRV build, and cargo-deny. All must pass.
6. A maintainer reviews. Expect questions; small follow-up commits are fine.

## License

synthlite is licensed under the [Apache License 2.0](LICENSE). By contributing, you agree that your contributions are licensed under the same terms.

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
