# Changelog

All notable changes to synthlite are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and synthlite uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version bump may change behavior; each entry says what to watch for.

## [Unreleased]

## [0.4.0] - 2026-09-28

First public release. Earlier versions below were released privately at Scogo AI and are listed for context.

### Added

- Plain prompt input. A seed file can be JSONL prompt records or a `.txt` file of prompts, so no upstream task generator is needed.
- `metadata.served_model` on every row: the model name the provider reported serving the reply, next to the model that was requested.
- `--help` on each subcommand.
- Install script (`install.sh`) that picks the right release binary for Linux or macOS, verifies it against `SHA256SUMS`, and installs to `~/.local/bin` without sudo.
- Multi-arch container image (`linux/amd64`, `linux/arm64`) published to `ghcr.io/scogo-ai/synthlite`: the static binary on an empty base, running as a non-root user.
- Continuous integration: rustfmt, clippy with warnings denied, tests on Linux and macOS, a minimum-Rust-version build, and cargo-deny license, advisory, and source checks.
- Contributor docs: `CONTRIBUTING.md` (DCO sign-off), `CODE_OF_CONDUCT.md`, `SECURITY.md`, issue and pull request templates.

### Changed

- Licensed under the Apache License 2.0.
- The crate declares its minimum supported Rust version: 1.89.
- A JSONL line without `schema_version` used to be refused; it is now read as a prompt record.
- A line that is not valid JSON now reports `malformed JSON: … (for one plain prompt per line, use a .txt file)` instead of `malformed task: …`, and a line with an unknown `schema_version` reports `unsupported schema_version <v>`.
- The positional argument of `generate` is named `INPUT` (it was `TASKS_JSONL`).

### Fixed

- A streamed reply whose first chunk carries an empty `model` (as Azure OpenAI's content-filter chunk does) no longer hides the model named by later chunks.

### Known limitations

- `manifest.json` still reports `source.taskgen_schema: scogo.taskgen.task.v2` for runs made from prompt records or `.txt` files.

## [0.3.2] - 2026-09-27

### Fixed

- Seed files in the `candidates.jsonl` format written by Taskgen (an optional upstream seed generator, to be open-sourced) are accepted. Each candidate is unwrapped and validated exactly like a task line, so a candidate and the same task in `tasks.jsonl` produce the same seed id. Candidates Taskgen rejected (non-empty `deterministic_checks.hard_failures`) are skipped and counted on stderr.

## [0.3.1] - 2026-09-27

### Added

- `[generation].persona` names the assistant in the `--detailed` system message ("You are <persona>. ..."), so a product can train its own named assistant, for example `persona = "Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations"`. It requires `--detailed`; an empty or multi-line persona is refused. The persona is part of the hashed config.

### Changed

- The `--detailed` teacher instruction asks for exact read-only commands or queries when the platform is known, and for a precheck, approval, success check, stop condition, and rollback for each stage of a requested change. The final answer no longer introduces itself or names the assistant, so the persona lives only in the system message. The instruction is hashed: detailed runs need a new `--out`.
- The dataset card no longer declares `dataset_info`. A declared feature list has to match every row, which free-form seed context cannot promise; the Hugging Face `datasets` library now infers features, so pushed datasets always load.

## [0.3.0] - 2026-09-27

Published as release candidates `v0.3.0-rc.1` and `v0.3.0-rc.2`.

### Added

- `--detailed` (or `[generation].detailed = true`) decision-trace mode, still one teacher call per seed. The teacher returns one JSON object of typed steps and a final answer; synthlite checks its structure and commits it as `## Decision trace` and `## Final answer` text. A reply that breaks the contract fails as `invalid_trace` immediately, with no retry. Nothing in a reply is executed.
- The teacher instruction for `--detailed` asks for complete traces grounded in the supplied evidence, applies protocol and product knowledge labeled as general knowledge, and ends with a self-check that every explicit request is answered.
- `gate` checks the final answer of a detailed row for refusals, and dataset cards say when a run used `--detailed`.

### Changed

- `--detailed` uses a fixed system message; passing your own system message together with `--detailed` is refused. Plain (non-detailed) configs hash exactly as before.
- Release tags with a hyphen (such as `v0.3.0-rc.1`) are published as pre-releases and never become the latest release.

## [0.2.3] - 2026-09-25

### Fixed

- Taskgen task records with the optional `coordinates.curriculum` object (context, objective, acceptance criteria, references) are accepted. Records without it remain valid.

## [0.2.2] - 2026-09-24

### Changed

- Progress lines start with a local timestamp and report `input_tokens` and `output_tokens` as the provider reported them, across every completed attempt including retries. `tok_s` is output tokens over elapsed time, and `finish_in` estimates the time left (`?` until there is enough evidence). The done line adds `input_tokens`, `output_tokens`, and `usage_missing`. `budget_retry` and `rate_limit` are shown only when nonzero.

## [0.2.1] - 2026-09-24

### Added

- Progress lines carry a timestamp.

### Changed

- Release binaries are built on native Linux x86_64, Linux aarch64, and macOS runners.

## [0.2.0] - 2026-09-23

### Added

- `synthlite --version` (and `-V`). The first line of every run log ends with `version=`, so each log records the build that produced it.
- Progress while a run is in flight: a heartbeat every `--progress-interval` seconds (default 30, `0` turns it off) with committed, failed, queued, and in-flight counts, retries, rate limits, tokens, elapsed time, and an estimate of the time left; plus one line per retry, output-budget step-down, and permanent failure. Prompts, replies, and keys are never printed.

### Changed

- Completions are streamed. Progress moves before any reply finishes, and `timeout_seconds` is now an idle timeout, so a long reply that keeps streaming is no longer cut off and re-sampled. A plain JSON answer is still accepted. New retry reasons: `stream_cut`, `stream_error`.
- With `max_output_tokens` unset, the output cap starts at 32768. A provider that rejects it with HTTP 400 gets the next lower cap once per run, not once per seed. An explicit `max_output_tokens` is sent as-is.
- Concurrency defaults to 16 per key and adapts: a 429 halves the key's limit and clean answers grow it back, so free-tier keys and dedicated servers both work without tuning.

## [0.1.0] - 2026-09-23

### Added

- First release: one teacher call per seed against any OpenAI-compatible chat API, crash-safe resume keyed on a hash of the generation config, `gate` to filter rows, and `push` to a private Hugging Face dataset.

[Unreleased]: https://github.com/scogo-ai/synthlite/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/scogo-ai/synthlite/releases/tag/v0.4.0
