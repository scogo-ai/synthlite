# Design

synthlite does one job: turn a file of prompts into a private fine-tuning dataset. This page explains the choices behind it and what each one costs you. The full contract is [spec.md](spec.md).

## The flow

```text
 prompts.jsonl / prompts.txt
            |
            v
 +---------------------+     one call per prompt, any OpenAI-compatible endpoint
 | synthlite generate  |---> out/rows.jsonl           committed rows, crash-safe
 +---------------------+     out/rows.errors.jsonl    failures, ids and classes only
            |
            v
 +---------------------+     length, refusal, repetition, exact dedup, split
 | synthlite gate      |---> out/data/train.jsonl, out/data/validation.jsonl
 +---------------------+     out/README.md (card), out/manifest.json
            |
            v
 +---------------------+
 | synthlite push      |---> private Hugging Face dataset, one commit, tagged
 +---------------------+
```

Each stage reads only files on disk. You can stop after any stage and inspect the output. The out dir is the only state. There is no server, database, or cache.

## Principles

### One call per seed

**What it means.** Each prompt gets exactly one teacher call and one reply. Retries happen only for transport failures, such as a timeout or a 5xx. `--detailed` asks for steps and an answer in that same call.

**Why.** Cost and time scale with the number of prompts, and nothing else. You can predict a run from its input size.

**What you give up.** No best-of-n sampling, no rewrite pass, and no preference pairs. If you want several answers per prompt, run several out dirs.

### Seeds in, dataset out

**What it means.** You bring the prompts. synthlite does not write, expand, or review them. The input is a text file or JSONL, and the output is a dataset repo.

**Why.** The prompts decide what the model learns. Keeping them in your hands keeps that decision visible and reviewable.

**What you give up.** Prompt diversity and quality are your job. Use a seed generator upstream if you need one. Taskgen, Scogo's seed generator, will be open-sourced.

### Deterministic gate, no judge

**What it means.** The gate runs cheap checks with fixed rules: length, refusal phrases, repeated lines, exact duplicates, and a held-out list. The same rows and config always give the same dataset.

**Why.** A judge model costs a second call per row and has its own biases. Fixed rules are free, fast, and easy to audit.

**What you give up.** Nobody checks that an answer is correct. Rows are unverified teacher output. Evaluate or filter them before you train.

### The committed row is the checkpoint

**What it means.** A row counts as done only when its full line is on disk. There is no separate progress file. Rerun the same command after a crash and it continues.

**Why.** One source of truth cannot disagree with itself. A torn last line is set aside, and that prompt runs again.

**What you give up.** One fsync per row. On very fast local servers this can be a small cost.

### Content-addressed identity

**What it means.** Each prompt's id is a SHA-256 of its text. Adding prompts generates only the new ones. Editing a prompt gives it a new id. The same prompt twice is refused.

**Why.** Ids never drift between runs or machines. Resume, dedup, and the train/validation split all key on the same id.

**What you give up.** You cannot keep two rows for the same prompt in one run. Your own `id` field is a label, not the key.

### One static binary

**What it means.** One file, no runtime, no Python environment, no database. Linux builds are static and run on any distro.

**Why.** It runs the same on a laptop, a CI job, or a GPU box, with nothing to install first.

**What you give up.** No plugins. Changing behavior means changing Rust code and building your own binary.

### Any OpenAI-compatible endpoint

**What it means.** It speaks `POST /chat/completions` with streaming. OpenAI, OpenRouter, vLLM, Ollama, Groq, Together, and LiteLLM all work. Several keys can pool in one run.

**Why.** Almost every hosted API and local server speaks this protocol. You can switch teachers without switching tools.

**What you give up.** No provider-native APIs, no batch APIs, and no tool calls.

### Private and quiet by default

**What it means.** `push` creates private repos only and refuses a public one. Keys, prompts, and replies never appear on stderr or in error files. The dataset card holds counts and hashes, not text.

**Why.** Prompts often hold internal details. Logs and CI output get copied to places you do not control.

**What you give up.** Publishing publicly is a manual step on Hugging Face. Debugging a bad reply means reading `rows.jsonl`, not a log.

### Training-shaped output

**What it means.** Each row is a `messages` array: optional system, user, assistant. TRL, Axolotl, Unsloth, and `datasets` load it directly. Lineage stays in `metadata`.

**Why.** The dataset should be ready to train on the moment it lands, with no conversion script.

**What you give up.** No Alpaca format, no Parquet, and no tool-call rows. Convert later if you need them.

## What synthlite is not

- Not a prompt generator. Bring prompts, or use a seed generator upstream.
- Not an evaluator. It never scores answers.
- Not a trainer. It stops at the dataset.
- Not a service. There is no daemon, web UI, or multi-user control plane.

## Glossary

| Term | Meaning |
|---|---|
| seed | One input prompt. It becomes one row, or one entry in the errors file |
| teacher | The model that writes the replies, reached through an OpenAI-compatible endpoint |
| served model | The model id the provider reported for a reply. Behind a router it differs from the requested model |
| row | One committed line of `rows.jsonl`: `messages` plus `metadata` |
| out dir | The directory given by `--out` (default `./out`). It holds one run's rows, state, and dataset files |
| `generator_config_hash` | A hash of everything that shapes a reply: sampling settings, system message, and `--detailed`. One out dir holds one hash |
| gate | The deterministic filter and splitter between generate and push |
| card | The dataset's `README.md` on Hugging Face: counts, teachers, gate summary, and lineage |
| canary | A small first run, such as 20 prompts on its own out dir, to check a teacher before a full run |
