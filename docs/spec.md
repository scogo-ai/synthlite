# synthlite specification

**Version:** 0.4.1
**Scope:** prompts to a private fine-tuning dataset
**Binary:** `synthlite`
**Implementation:** small Rust core

This is the full contract: every input rule, hash, refusal message, file, and exit code. The [README](../README.md) is the operator guide. The shorter pages in this folder explain the same behavior task by task.

## 1. Summary

`synthlite` reads prompts, generates one assistant reply per prompt through OpenAI-compatible chat-completion APIs, commits each reply as a crash-safe JSONL row, filters the committed rows through a deterministic quality gate into `train` and `validation` splits, and pushes the result to a private Hugging Face dataset repo.

Prompts come as a plain text file, as JSONL prompt records, or as Taskgen seed records. Taskgen is Scogo's seed generator. It is optional and is to be open-sourced. Its wire format is documented here because seed ids depend on it.

The product is one path with three stages:

```text
prompts.jsonl | prompts.txt | Taskgen tasks.jsonl
        |
        v
synthlite generate      stage 1: identity, schedule, commit rows.jsonl
        |
        v
synthlite gate          stage 2: dedup, filter, split, card, manifest
        |
        v
synthlite push          stage 3: private Hugging Face dataset repo
```

`synthlite` does not author prompts, review them, or judge answers with a model. The published dataset is training-shaped and filtered for cheap, deterministic defects. It does not claim the answers are correct.

## 2. Goals

- Read prompts from a `.txt` file, from JSONL prompt records, or from Taskgen `scogo.taskgen.task.v2` records and `scogo.taskgen.candidate.v1` wrappers.
- Derive a content-addressed `source_task_id` for every prompt. For Taskgen records, use the same canonical JSON and SHA-256 as Taskgen.
- Generate one assistant reply per seed (`variant_index` 0) and keep only replies that finished with `finish_reason: "stop"`.
- Commit `synthlite.sft.v1` rows whose `messages` field is the fine-tuning payload.
- Optionally (`--detailed`), make the assistant content a decision trace from the same single call: typed Evidence, Hypothesis, Action, Verification, and Conclusion steps, then a final answer, in the same row schema.
- Keep `source_task_id`, teacher identity (provider, base URL, requested model, served model), sampling config hash, token usage, and generation time on every row as lineage.
- Schedule across configured providers and keys, with per-key concurrency and request limits, and hard caps on spend per invocation.
- Resume without duplicating a committed row and without a second spend on a failed row.
- Survive a crash during any file write.
- Gate committed rows without a judge model: exact dedup, truncation, length, refusal, repetition, held-out exclusion.
- Split deterministically by `source_task_id` so re-runs produce the same `validation` membership.
- Push `data/train.jsonl`, `data/validation.jsonl`, `README.md`, and `manifest.json` to a private Hugging Face dataset repo, idempotently.
- Ship as a single static binary.

## 3. Non-goals

The binary does not provide:

- LLM-as-judge, grounding, semantic or MinHash near-dedup, or safety scoring;
- prompt authoring, seed review, or review adjudication;
- more than one answer variant;
- Alpaca or other row shapes besides `messages`; Parquet output;
- Anthropic-native, Responses API, or provider batch APIs;
- a global worker pool, fair cross-provider admission, circuit breakers, TPM reservations, or daily quotas;
- JSON-mode, tool calls, or `response_format` (responses are streamed only to see progress and to time out stalls, not to use partial replies). `--detailed` asks for one JSON object in the system message, parses it from `content`, and renders it as text. It sends no `response_format` and no tools, asks for no tool calls, and executes nothing;
- language detection;
- a raw provider-response debug directory or a structured event log;
- account creation, key scraping, CAPTCHA bypass, or quota evasion;
- a web service or multi-user control plane;
- DPO, KTO, or rejected-answer pairs.

`synthlite` output is not fed back to a seed generator as seeds.

## 4. Pipeline

### Stage 1 — generate

Input is one file of prompts (section 7). Each prompt becomes one work item. A single writer appends one `messages` row per successful item to `rows.jsonl` and fsyncs it. Permanent failures append to `rows.errors.jsonl`. There is no separate per-row checkpoint; the committed row is the record of completion.

### Stage 2 — gate

`gate` reads `rows.jsonl`, refuses if generation is incomplete, applies the deterministic checks in section 11, assigns each surviving row to `train` or `validation`, and writes `data/`, `README.md`, `manifest.json`, `rejected.jsonl`, and a receipt. Rejected rows never enter `data/`.

### Stage 3 — push

`push` uploads `data/train.jsonl`, `data/validation.jsonl`, `README.md`, and `manifest.json` to a private Hugging Face dataset repo, in one commit, tagged. It refuses when the gate receipt is missing or stale.

## 5. Decisions

### Rust core

Rust is the language because Taskgen hashes with `serde_json::to_vec` from `serde_json = "1"` without the `preserve_order` feature. `synthlite` uses that same crate and feature set for every hash in this document, so a Taskgen seed gets the same id in both tools.

The two Taskgen golden ids match a compact, key-sorted, UTF-8 JSON encoding, including Go's `encoding/json` on those particular strings. Go is still the wrong implementation: its default encoder escapes `&`, `<`, and `>`, and it renders a whole-number float such as `1.0` as `1`. `generator_config_hash` includes sampling numbers and must use the same bytes as the task id. Matching `serde_json` in Rust is the contract. A Go port would be a second encoder.

Crates: `tokio`, `reqwest` (rustls, no default features), `serde`, `serde_json`, `sha2`, `clap`, `toml`, `url`, `base64`, `chrono`, `eventsource-stream`, `futures-util`, `anyhow`. Test-only: `wiremock`, `tempfile`. No Hugging Face client crate; the Hub is a handful of HTTP calls (section 13).

### The committed row is the checkpoint

A complete line in `rows.jsonl` already proves its work item completed, and resume scans those lines. A second fsynced checkpoint that lists completed keys would be redundant, and rewriting such a list per row grows quadratically. There is one fsync per row and no completed-key list. Failure state lives in `rows.errors.jsonl`; key state lives in a small `state.json`.

### Resume fingerprint

`source_task_id` is the SHA-256 of the identity bytes, so "same id, different identity bytes" cannot happen. Resume compares the id set of the current input against committed and failed keys and reports additions and drops. A `generator_config_hash` that differs from `state.json` does not refuse: see section 10. The one refusal is a `--detailed` mode switch, or a committed row whose hash is neither the current one nor in `config_history`.

### Failed work on resume

A failed work item stays failed across process restarts. Resume does not call the provider for it. `--retry-failed` returns failed items to pending and spends again, with a fresh `max_attempts` budget for that run. A resumed run prints the failed count.

### System prompt

A seed generator's own system prompt, which tells a model how to write prompts, is never sent.

Default generation has no system message. The request and the committed `messages` array are `user` then `assistant`. An optional system message is `SYNTHLITE_SYSTEM_PROMPT`, or `[generation].system_message` when that env var is unset. It is `messages[0]` and is inside `generator_config_hash`. Empty, whitespace-only, or unset means no system message and a JSON `null` in the hash, not `""`.

In `--detailed` mode the system message is fixed. By default it is:

```text
You are a careful IT operations assistant. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.
```

`[generation].persona` names the assistant by replacing that message's role phrase, "a careful IT operations assistant". For example, `persona = "Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations"` gives:

```text
You are Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.
```

The persona is one line of text. Surrounding whitespace and a trailing period are dropped. A value that is empty or spans several lines exits 2 with `[generation].persona must be a non-empty, single-line string`. A persona without `--detailed` exits 2 for `generate` with `[generation].persona requires --detailed`; `gate` and `push` ignore it. The persona is not a separate hash key: it is part of the hashed `system_message`, so an unset persona keeps earlier detailed hashes and changing it needs a new `--out`. Identity lives in the system message only. The teacher instruction tells the teacher not to introduce itself or name the assistant, so answers stay technical.

The system message is `messages[0]` of every detailed row and is hashed as `system_message`. The request appends a blank line and the teacher instruction to it (section 14). The committed row does not. If `SYNTHLITE_SYSTEM_PROMPT` or `[generation].system_message` is set and not whitespace, `generate` exits 2 before preflight:

```text
--detailed uses a fixed system message; unset SYNTHLITE_SYSTEM_PROMPT and [generation].system_message, and use [generation].persona to name the assistant
```

An operator message such as "Follow the user's requested role, format, and safety constraints" would tell the teacher to follow the task's format while the contract demands one JSON object, and it would become the trained cue for an output that always uses the trace format. The check runs only when loading for `generate`. `gate` and `push` do not apply it, so a shell that exports `SYNTHLITE_SYSTEM_PROMPT` does not break them.

### Teacher identity is per row, not per run

A run may pool several keys, and one output directory holds one hash. If `provider`, `base_url`, and `model` were inside `generator_config_hash`, two providers in one run would make two work items per seed. So only the shared generation settings are hashed, and each row records the teacher that served it. Every seed still gets exactly one row, and the manifest counts rows per teacher.

A router such as OpenRouter's `openrouter/free` answers with a different model on each request. The requested model is `metadata.model`; the model the provider reported is `metadata.served_model` (section 9).

### Gate without a judge

The gate runs only checks that are deterministic, cheap, and defensible without a model: exact dedup on normalized prompt and on normalized response, `finish_reason`, length bounds, an opening-window refusal phrase list, repeated-line degeneration, and an optional held-out prompt-hash list. Near-duplicate prompts are the seed author's job. Response-side near-dedup and judged quality are out of scope.

### JSONL, not Parquet

The Hub recommends Parquet for most datasets and accepts JSON Lines for nested data up to several GB. A `synthlite` dataset is under 1 GB at 100k rows. JSONL is byte-identical to the committed rows, diffable, and needs no Arrow dependency in the binary. `datasets`, TRL, Axolotl, and Unsloth load it directly. Parquet is a later conversion, not an output.

### Deterministic split

`validation` membership is a function of `source_task_id` alone, so it is stable across re-runs, across providers, and across gate configs. Dedup runs before the split, so the same normalized prompt cannot appear in both splits.

### Hub transport

`push` speaks the Hub HTTP commit API directly: `preupload`, the Git LFS batch endpoint when the Hub asks for it, `commit`, `tag`. The Hub migrates LFS uploads to Xet storage server-side, so no Xet client is required. `synthlite` does not shell out to the `hf` CLI and does not write `.gitattributes`; the `preupload` response decides upload mode per file.

### Batch APIs

OpenAI's Batch API and Anthropic's Message Batches API halve token cost with a 24-hour completion window. Neither is exposed by most OpenAI-compatible providers, and both change the control flow from per-request admission to submit-and-poll. They are not implemented. The seam is the provider trait: a batch adapter would submit pending work-item keys as `custom_id`, poll, and hand each result to the same writer with the same row shape. Nothing in the row, gate, or publish stages would change.

## 6. Architecture

```text
+----------------------+
| input preflight      |
| fail before any HTTP |
+----------+-----------+
           |
           v
+----------------------+
| source_task_id       |
| work item key        |
| variant_index = 0    |
+----------+-----------+
           |
           v
+----------------------+
| resume scan          |
| rows.jsonl           |
| rows.errors.jsonl    |
| state.json           |
+----------+-----------+
           |
           v
+----------------------+
| per-key admission    |
| concurrency + RPM    |
| 429 cooldown / park  |
| --max-requests cap   |
+----------+-----------+
           |
           v
+----------------------+
| OpenAI-compatible    |
| chat completion      |
| finish_reason check  |
+----------+-----------+
           |
           v
+----------------------+
| single writer        |
| encode, write, fsync |
+----------+-----------+
           |
           v
+----------------------+
| gate                 |
| dedup, filter, split |
| card, manifest       |
+----------+-----------+
           |
           v
+----------------------+
| private HF dataset   |
| data/ card manifest  |
+----------------------+
```

Workers perform network calls. Only the writer appends output. Admission is the sum of per-key slots. There is no global `--workers` pool in front of that sum.

### Output directory

`--out` defaults to `./out`, relative to the current working directory. Every command accepts `--out DIR`. `generate` creates the directory when it is missing. `gate` and `push` do not; a missing directory is exit 2 with `<out> does not exist`. Layout:

```text
DIR/
  rows.jsonl              committed generations (stage 1)
  rows.partial            quarantined torn tail, only after a crash
  rows.errors.jsonl       permanent failures, one line each
  rows.errors.partial     quarantined torn tail of the errors file
  state.json              config hash, parked keys, generation_complete
  .synthlite.lock         advisory lock; never uploaded
  rejected.jsonl          gate rejections, ids and reasons (stage 2)
  gate.json               gate receipt
  data/train.jsonl        publish set (stage 2)
  data/validation.jsonl
  README.md               dataset card
  manifest.json
  publish.json            publish receipt (stage 3)
```

Only `data/`, `README.md`, and `manifest.json` are uploaded. `.synthlite.lock` does not make a directory a synthlite run (section 10).

## 7. Input contract

```bash
synthlite path/to/prompts.jsonl
synthlite path/to/prompts.txt
synthlite path/to/tasks.jsonl
```

That invocation is `generate`. `--out` defaults to `./out`. No config file is required. Command resolution, defaults, and the directory rule are section 14. Resume scanning is section 10.

Preflight reads the whole file before the first provider call. Config load runs first, so a config error exits 2 before the input is read. The first malformed non-empty line aborts the process with the path and 1-based line number. No request is sent. Input lines are UTF-8. A line longer than 1 MiB fails preflight.

The format is chosen by the file extension. A path whose extension is `.txt`, in any case, is a plain text file. Any other path is JSONL.

### Plain text

Each non-empty line, trimmed, is one prompt. Blank and whitespace-only lines are skipped. A text line is the same prompt as a prompt record with that `prompt` and no `id` or `metadata`, so it gets the same `source_task_id`.

### JSONL

Each non-empty line is one JSON object. Whitespace-only lines are ignored. The record kind is chosen per line by its `schema_version` key, so one file may mix kinds:

| `schema_version` | Record |
|---|---|
| absent | prompt record |
| `scogo.taskgen.task.v2` | Taskgen task record |
| `scogo.taskgen.candidate.v1` | Taskgen candidate wrapper |
| any other value | refused: `<path>:<N>: unsupported schema_version <v>` |

### Prompt records

```json
{"id": "bgp-flap-001", "prompt": "Our edge router's eBGP session ...", "metadata": {"topic": "bgp"}}
```

- `prompt`: required. A non-empty string of at most 20000 characters.
- `id`: optional string. It is copied to row `metadata.input_id` and is not part of `source_task_id`.
- `metadata`: optional object. It is copied to row `metadata.input_metadata` and is not part of `source_task_id`.
- Any other key fails the line with `<path>:<N>: unknown field <k> (put extra fields under "metadata")`.

### Taskgen task records

A Taskgen task line must be one JSON object with `additionalProperties: false`, as in Taskgen's task v2 schema.

Required on every object:

- `schema_version`: `scogo.taskgen.task.v2`;
- `prompt`: string length 1 through 20000;
- `category`, `domain`, `subdomain`: non-empty strings;
- `difficulty`: integer 1 through 10;
- `coordinates`: an object with `additionalProperties: false`, all eleven original keys present (`taxonomy_id`, `category_id`, `task_family`, `environment`, `platform_scope`, `platforms`, `incident_mechanism`, `evidence_condition`, `evidence_bundle`, `action_risk`, `presentation`), and optional `curriculum`; `platforms` is unique and has the cardinality `platform_scope` demands (0 for `platform_neutral`, 1 for `single_platform`, 2 for `multi_platform`);
- `taskgen_model`: non-empty string;
- `temperature`: number greater than or equal to 0.

`language` is optional; when present it is a string of length 2 through 16. Unknown fields fail the line.

Taskgen's curriculum extension keeps `schema_version: scogo.taskgen.task.v2`. When present, `coordinates.curriculum` has a non-empty string-valued `context` object, a non-empty `objective`, one or more non-empty `acceptance` strings, and one or more HTTPS `references`. Records without `curriculum` remain valid. The complete coordinates object, including curriculum when present, is included in `source_task_id` and saved row metadata.

`taskgen_model`, `temperature`, and `language` are validated, copied into row `metadata`, and left out of `source_task_id`.

### Taskgen candidate wrappers

Taskgen writes accepted tasks to `tasks.jsonl` and every generated candidate to `candidates.jsonl`. Each line there is a `scogo.taskgen.candidate.v1` wrapper: `{schema_version, candidate_id, sequence, wave, repair_of, repair_count, prompt_sha256, deterministic_checks, candidate}`. `candidate` holds an ordinary task v2 record. Taskgen writes the wrapper right after generation, before its review and dedup acceptance.

- **Unwrapping:** for a wrapper line, preflight takes the `candidate` object and validates it exactly as a task line, unknown fields included. `source_task_id`, the population digest, and row metadata come from that inner task, so a candidate and the same task in `tasks.jsonl` are the same seed and share an id. Wrapper fields are not validated and are not copied into rows.
- **Missing task:** a wrapper without a `candidate` object exits 2 with `<path>:<N>: scogo.taskgen.candidate.v1 record has no candidate object`.
- **Hard failures:** a wrapper whose `deterministic_checks.hard_failures` is non-empty is skipped, because Taskgen never accepts such a candidate. `generate` then prints `preflight skipped <N> Taskgen candidates with deterministic hard failures` before its first line.
- **Review rejections:** a candidate that Taskgen's model review later rejects is still in `candidates.jsonl`, and synthlite cannot see that review. For a Taskgen run with review enabled, read its `tasks.jsonl`.

### Duplicates

Duplicate Taskgen records in one file fail preflight with `<path>:<N>: duplicate source_task_id <id>`. The same prompt twice, as text lines or prompt records, fails with `<path>:<N>: duplicate prompt (same prompt as line <M>)`, even when the `id` or `metadata` differ. A prompt record and a Taskgen record with the same prompt text are not duplicates: they hash different objects (section 8).

### Population digest

Preflight computes `source_population_sha256` over every parsed record with Taskgen's algorithm: rows of `{"source_task_id", "source_task"}` sorted by id, wrapped as `{"schema_version": "scogo.private-hf-source-population.v1", "rows": [...]}`, `serde_json::to_vec`, SHA-256 hex. For a Taskgen line, `source_task` is the validated task object (the inner task for a wrapper). For a prompt, it is the prompt input as parsed. The digest is recorded in `state.json` and the manifest as source lineage. Golden value for the two Taskgen golden tasks in section 8, in that order: `f01f0b22eac765cec916eb7f1b92973ddf53b3472705967445a53c87f0392a76`.

### run.json sidecar

If `run.json` exists beside the input file and parses with `schema_version` `scogo.taskgen.run.v3`, its `run_id` is copied into `state.json` and the manifest as `taskgen_run_id`. Otherwise that field is `null`. Nothing else is read from it.

## 8. Identity and work items

### source_task_id

Every prompt gets `task_` plus the lowercase SHA-256 hex of one identity object. The object depends on the input kind.

A text line or a prompt record hashes:

```json
{"schema_version": "synthlite.prompt.v1", "prompt": "..."}
```

A Taskgen record hashes the same object Taskgen hashes:

```json
{
  "schema_version": "scogo.taskgen.task.v2",
  "prompt": "...",
  "category": "...",
  "domain": "...",
  "subdomain": "...",
  "difficulty": 8,
  "coordinates": {}
}
```

Build the object as a `serde_json::Value` and serialize with `serde_json::to_vec` (compact UTF-8, map keys ordered by `BTreeMap` at every depth, no HTML escaping, array order preserved, numbers re-emitted as parsed). SHA-256 those bytes and format lowercase hex:

```text
task_<64 hex chars>
```

If Taskgen `coordinates` were missing, Taskgen substitutes `{}`. `synthlite` rejects a missing `coordinates` object at preflight, so generation never hashes a substituted object. The hasher still uses this field set only.

Golden vectors:

- the prompt `What does BGP state Active mean?`, as a text line or prompt record, hashes the bytes `{"prompt":"What does BGP state Active mean?","schema_version":"synthlite.prompt.v1"}` to `task_094d3488161cd3c3939ac0e45f81d0997c86a133196afed9f3775fa89f767f1d`;
- the Taskgen fixture `tests/fixtures/canonical/valid-task.json` as stored: `task_cc3e0bec7b87ec223ae0ef01f4d4235ff26f8b5111d6590c39c8c7db13a88a7f`;
- the same Taskgen object with `prompt` replaced by `Unicode café 路由 incident`: `task_8a45fd82c66ff7d94244f25acc28ec84d57d96bb85ebf50f777a3c74bf1ddf34`.

Those strings, plus a fixture prompt that contains `&`, `<`, and `>`, are required identity tests. Do not add `id` or `metadata` to the prompt object, and do not add `language`, `taskgen_model`, or `temperature` to the Taskgen object.

### generator_config_hash

The generation config is shared by every key in the run, whether it came from the environment or from a config file. Hash this object with the same serializer and lowercase SHA-256 hex, prefixed `gen_`:

```json
{
  "schema_version": "synthlite.generator-config.v1",
  "system_message": null,
  "temperature": null,
  "top_p": null,
  "max_output_tokens": null,
  "seed": null,
  "frequency_penalty": null,
  "presence_penalty": null,
  "stop": null,
  "reasoning_effort": null
}
```

`system_message` is the configured system string, or `null` when generation has no system message. In a `--detailed` run it is the fixed system string (section 5). `reasoning_effort` is the string sent to reasoning models, or `null`. Sampling fields are JSON numbers, a JSON integer, an array of strings, or `null` when the request omits that parameter. Absent and unspecified are `null`, not a missing key and not `""`.

`max_output_tokens` is the one exception to "`null` means omitted". `null` means the implicit budget: 32768 is sent, stepped down to 16384, 8192, and 4096 when the provider answers HTTP 400 (section 9). The hash keeps `null`, so neither the implicit value nor a step-down changes it. An explicit integer is hashed as written and sent unchanged; it is never stepped down. Setting `max_output_tokens = 32768` explicitly is a different hash from leaving it out, even though the first request body is the same.

The sampling fields `temperature`, `top_p`, `seed`, `frequency_penalty`, `presence_penalty`, `stop`, and `reasoning_effort` are HTTP request body fields when they are not `null` (section 14). `system_message` becomes `messages[0]`. API keys, timeouts, concurrency, RPM, retry limits, `max_tokens_field`, `require_model_match`, and gate settings are not part of the hash. The input format is not part of the hash either; it is part of each `source_task_id`.

A `--detailed` run (section 14) hashes a larger object. It adds `"detailed": true` and `"teacher_instruction"`, the exact teacher instruction text in section 14, and `system_message` is the fixed system string, not `null`:

```json
{
  "schema_version": "synthlite.generator-config.v1",
  "detailed": true,
  "system_message": "You are a careful IT operations assistant. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.",
  "teacher_instruction": "<teacher instruction, exactly>",
  "temperature": null,
  "top_p": null,
  "max_output_tokens": null,
  "seed": null,
  "frequency_penalty": null,
  "presence_penalty": null,
  "stop": null,
  "reasoning_effort": null
}
```

A plain run, with `detailed` omitted or `false`, hashes exactly the first object, with no `detailed` key and no `teacher_instruction` key. An all-default plain config hashes to `gen_48ca1520349e09dc2f26f6797206185f1aac4acabf61806899ad03777c49e172`, so out dirs written by earlier versions resume. `false` is never hashed: a key that is always present would break every existing resume.

`teacher_instruction` is hashed because it is sent to the provider and shapes every reply. Editing that text in a later synthlite version changes the hash, so one out dir never mixes rows made under two contracts. It also puts the exact contract in `state.json` and `manifest.json` as lineage. `detailed` and `teacher_instruction` are never HTTP body keys.

The teacher that served a row is not in the hash. It is recorded per row as `metadata.provider`, `metadata.base_url`, `metadata.model`, and `metadata.served_model`, so the same model name on two providers, or at two base URLs, is distinguishable row by row and counted separately in the manifest.

### Work-item key

```text
(source_task_id, variant_index, generator_config_hash)
```

There is one variant. `variant_index` is always 0 and is still part of the key. A different `generator_config_hash` is a different key. `state.json` holds the current hash and `config_history` holds earlier ones; each row keeps its own. Any key in the pool may serve any pending work item, and each seed gets exactly one row regardless of which key served it. A directory whose state hash differs from the current config resumes and records the change (section 10). It refuses only for a `--detailed` mode switch (the operator reruns in the directory's mode, section 14, or passes a new `--out`) or a committed row whose hash is unknown to `state.json`. Committed lines are never rewritten. Model, base URL, and API key are not in the hash.

## 9. Output contract

Each committed line of `rows.jsonl` is one UTF-8 JSON object, `synthlite.sft.v1`, ending in `\n`. A row from a prompt record:

```json
{
  "schema_version": "synthlite.sft.v1",
  "source_task_id": "task_<hash>",
  "variant_index": 0,
  "generator_config_hash": "gen_<hash>",
  "messages": [
    {"role": "user", "content": "<input prompt, unchanged>"},
    {"role": "assistant", "content": "<generated reply>"}
  ],
  "metadata": {
    "category": null,
    "domain": null,
    "subdomain": null,
    "difficulty": null,
    "coordinates": {},
    "language": null,
    "taskgen_model": null,
    "input_id": "bgp-flap-001",
    "input_metadata": {"topic": "bgp"},
    "provider": "openrouter",
    "base_url": "https://openrouter.ai/api/v1",
    "model": "openrouter/free",
    "served_model": "<model id the provider returned>",
    "generated_at": "2026-09-28T09:14:03Z",
    "max_output_tokens": 32768,
    "usage": {"prompt_tokens": 812, "completion_tokens": 1460}
  }
}
```

A row from a Taskgen record keeps the Taskgen metadata keys and has no `input_id` or `input_metadata`:

```json
"metadata": {
  "category": "enterprise_netops",
  "domain": "layer3_routing",
  "subdomain": "bgp_route_leak",
  "difficulty": 8,
  "coordinates": {},
  "language": null,
  "taskgen_model": "teacher/model",
  "provider": "groq",
  "base_url": "https://api.groq.com/openai/v1",
  "model": "llama-3.3-70b-versatile",
  "served_model": "llama-3.3-70b-versatile",
  "generated_at": "2026-09-22T09:14:03Z",
  "max_output_tokens": 32768,
  "usage": {"prompt_tokens": 812, "completion_tokens": 1460}
}
```

Key order in the stored JSON is sorted, as `serde_json` writes it; the examples above are ordered for reading.

When a system message is configured, it is the first message and the user message stays the input prompt:

```json
"messages": [
  {"role": "system", "content": "<synthlite system message>"},
  {"role": "user", "content": "<input prompt>"},
  {"role": "assistant", "content": "<generated reply>"}
]
```

`messages` is the column TRL conversational SFT, Axolotl `chat_template` (`field_messages: messages`, default `roles_to_train: ["assistant"]`), Unsloth's role/content path, and `datasets` JSON loading consume. Trainer config still selects the `messages` column and masks non-assistant tokens; that is the trainer's job, not the dataset's. Roles are `system`, `user`, and `assistant`. Every `content` is a non-empty string. The last message is `assistant`. There is no tool role.

The user content is the input prompt string. Taxonomy, seed lineage, and provider facts stay in `metadata` and are not copied into `messages`.

Row metadata fields:

- `category`, `domain`, `subdomain`, `difficulty`, `coordinates`, `language`, `taskgen_model`: copied from a Taskgen record; `language` is `null` when absent. For a prompt, the first four and `language` and `taskgen_model` are `null`, and `coordinates` is `{}`.
- `input_id`, `input_metadata`: prompt rows only. The record's `id` string and `metadata` object, or `null` when absent. Always `null` for a text line.
- `provider`, `base_url`, `model`: the key that served the row. `model` is the requested model id. `base_url` is the parsed URL (`url` crate: lowercase host, default port dropped) with userinfo removed and trailing `/` trimmed.
- `served_model`: the `model` string the provider returned, from the first streamed chunk that carries one or from the JSON body, or `null` when the provider sent none. It matters with routers such as OpenRouter's `openrouter/free`, which pick a different model per request.
- `generated_at`: the writer's UTC time at commit, RFC 3339 with second precision.
- `max_output_tokens`: the output-token cap sent on the attempt that produced the row: 32768 by default, 16384, 8192, or 4096 after a step-down, or the explicit `[generation].max_output_tokens`.
- `usage`: `{"prompt_tokens", "completion_tokens"}` from the response `usage` object, or `null` when the provider omits it.

### Decision-trace rows

A `--detailed` row is still `synthlite.sft.v1` and has exactly the plain `metadata` fields for its input kind. `messages` is always `[system, user, assistant]`: the fixed system message (section 5), the input prompt unchanged, and the rendered trace. The teacher instruction is never stored in `messages`. There is no `metadata.trace` and there are no tool calls.

The assistant `content` is rendered as:

```text
## Decision trace
1. Evidence: <content>
2. Hypothesis: <content>
3. Action: <content>
4. Verification: <content>
5. Conclusion: <content>

## Final answer
<final_answer>
```

Lines are joined with `\n`. Numbering follows the teacher's step order. Every kind appears at least once and a kind may repeat, so a trace has 5 to 24 steps. Kind labels are title case. A step whose content has a newline keeps it. A consumer that needs the parts splits `content` at the first `\n\n## Final answer\n`; the parse rules below make that split exact.

The row does not name the mode. The mode is in `state.json` and `manifest.json` (`generator_config.detailed`) and in the hash.

### Response acceptance

The response is `choices[0]`. A row is written only when all hold:

- `finish_reason` is the string `stop`. `length` is class `truncated` (see below); `content_filter` is `content_filter`; any other value is `unexpected_finish_reason`. A missing `finish_reason` is `no_finish_reason` unless the key sets `require_finish_reason = false`, in which case the row is eligible.
- `message.content` is a string and non-empty after trimming. Null, missing, or whitespace-only content is `empty_assistant`. A non-null `message.refusal` is `refusal`. Reasoning fields such as `reasoning_content` are ignored.
- When `require_model_match` is true (default) and the response includes a `model` string, that string is the requested model, or the requested model plus `-YYYY-MM-DD` (`gpt-4o-mini` accepts `gpt-4o-mini-2024-07-18`), or plus `-` and exactly four digits (`gpt-4` accepts `gpt-4-0613`). Anything else is `model_mismatch`, including `gpt-4o-mini` for a `gpt-4o` request. A response without `model` is eligible. A key for a router that picks the model, such as `openrouter/free`, sets `require_model_match = false`.

A 2xx response with no `choices[0]`, or a body that does not parse, is retried (section 14). Assistant text is trimmed before it is stored.

In a `--detailed` run, a reply that passes the rules above is then parsed as a decision trace. `length` is still `truncated` and a refusal field is still `refusal`; the trace parse runs only after those checks. The trimmed `content` must hold one JSON object: either the whole string, or an object after a preamble such as "Here is the JSON:" or an opening fence, followed by nothing but whitespace or whitespace and one closing ```` ``` ```` fence. A reply that is complete except for the object's final `}` is repaired once by appending it; every rule below still applies. There is no markdown fallback. The object must satisfy:

- exactly the keys `reasoning_steps` and `final_answer`;
- `reasoning_steps` is an array of 1 to 24 objects, each with exactly the keys `kind` and `content`;
- `kind` is `evidence`, `hypothesis`, `action`, `verification`, or `conclusion`, exact lowercase, and each of the five appears at least once;
- `content` is a string, non-empty after trimming, and none of its lines equals `## Decision trace` or `## Final answer` after trimming;
- `final_answer` is a string, non-empty after trimming.

Stored and rendered values are the trimmed strings. The kind-coverage rule and the 24-step cap exist because there is no judge: structure is the only check synthlite makes on a trace.

A reply that fails is class `invalid_trace` with `http_status` 200. It is permanent on the first occurrence and is not retried: a teacher that ignores the contract usually ignores it on every seed, and a retry would spend up to `max_attempts` full generations per seed. It goes to `rows.errors.jsonl` like any permanent failure, with no prompt and no reply text, and a run with any `invalid_trace` ends with exit 4. `--retry-failed` is the deliberate second spend.

### Truncation budget

When `[generation].max_output_tokens` is unset, the implicit budget starts at 32768. Hosted APIs bill tokens generated, not the cap, and a self-hosted server only reserves what it uses, so a high cap costs nothing. A low one does: a reply cut at the cap is thrown away, and regenerating it from scratch at a larger cap pays for the first attempt again. A `length` stop at any budget is therefore recorded as `truncated` at once, with no retry.

Some providers reject a cap above their model's output limit with HTTP 400. On a 400 while the implicit budget is in use, the work item goes back to the front of pending with the next lower budget: 16384, then 8192, then 4096, printing `budget <id> max_output_tokens=<from>-><to> reason=http_400`. A 400 at 4096 is recorded as `invalid_request`. Step-down calls count against `--max-requests`, do not use a `max_attempts` try, and do not advance the recorded `attempt`. When a stepped-down budget then gets a 2xx on the key that rejected the larger one, the key remembers it: later items on that key start at that budget, and `budget <key id> max_output_tokens=<from>-><to>` is printed once. So a provider capped at 8192 costs two rejected calls once per run, not per seed. A stepped budget stays tied to the key that rejected the larger one; on another key an item starts again at that key's own ceiling. The ceiling is not persisted: a new run starts at 32768 again.

When `max_output_tokens` is set explicitly, `truncated` is a permanent failure on the first occurrence.

### Errors sidecar

`rows.errors.jsonl` receives one object per permanent failure: the work-item key (`source_task_id`, `variant_index`, `generator_config_hash`), `provider`, `model`, `attempt`, `error_class`, `http_status` (`null` when there was none), `max_output_tokens` (the cap sent on the failing attempt), and `at`, a UTC timestamp. It does not contain the prompt, the response body, any header, or any credential. It is appended with the same encode-write-fsync sequence as `rows.jsonl`.

## 10. Crash safety and resume

### Single writer

One process owns an output directory. `generate`, `gate`, and `push` each take an exclusive non-blocking advisory lock (`flock`, via `std::fs::File::try_lock`) on `DIR/.synthlite.lock`. `generate` creates `--out` if it is missing, then locks, and holds the lock for the whole run. If another process holds it, the command exits 2 with `another synthlite process is using <out>`, and `generate` makes no provider call. `--dry-run` takes no lock. The lock file stays in the directory and is never uploaded.

Within the process, one writer task owns `rows.jsonl` and `rows.errors.jsonl`. Workers hand it a finished row or a permanent failure. For each:

1. Encode the full JSON line, including the trailing `\n`, into one buffer.
2. Write that buffer. `O_APPEND` is not treated as an atomic line write. Prompt text can be up to 20000 characters and the assistant line is larger than that, so a crashed write can tear a line.
3. `fsync` the file. The directory is fsynced when either file is created.

That is the commit. There is no fourth step.

On a resume, if either file's last line has no `\n`, the discarded tail is appended to `rows.partial` or `rows.errors.partial`, followed by `\n`, and that file is fsynced. Then the source file is truncated to the previous newline. The `.partial` files are append-only, one fragment per line; earlier fragments are kept. A crash between the two steps can duplicate a fragment but cannot lose one. Never rewrite an earlier line. The quarantined tail's key has no committed line, so that work item runs again.

A failed write, or `ENOSPC` from the fsync, truncates the file back to its length before the line and exits 1. Committed lines are untouched.

### state.json

Written atomically (temp file in `DIR`, fsync, rename, fsync directory) when it changes, which is rare:

```json
{
  "schema_version": "synthlite.state.v1",
  "synthlite_version": "0.4.0",
  "generator_config_hash": "gen_<hash>",
  "generator_config": {},
  "source_population_sha256": "<hex>",
  "taskgen_run_id": null,
  "seed_count": 5000,
  "created_at": "2026-09-22T09:00:00Z",
  "generation_complete": false,
  "config_history": [],
  "parked_keys": [{"key": "groq/GROQ_API_KEY/llama-3.3-70b-versatile@https://api.groq.com/openai/v1", "reason": "headerless_429", "at": "..."}]
}
```

A key id is `<provider>/<api_key_env>/<model>@<base_url>`, with `base_url` as in `metadata.base_url`. The env key is `openai/OPENAI_API_KEY/<model>@<base_url>`. Reordering `[[key]]` entries does not move parked state. The `parked <id> <reason>` stderr line prints the id with `base_url` reduced to `host[:port]`, for example `parked groq/GROQ_API_KEY/llama-3.3-70b-versatile@api.groq.com unauthorized`; the full id is only in `state.json`. A parked id that matches no configured key is dropped the next time parked state is written. `reason` is `headerless_429` or `unauthorized`.

`generator_config` is the object hashed in section 8. `config_history` is omitted while empty; each entry is `{generator_config_hash, generator_config, superseded_at}`. `source_population_sha256`, `taskgen_run_id`, and `seed_count` are refreshed at every non-dry-run preflight. `generation_complete` is false when the directory is created and whenever a preflight finds a pending or failed id. It becomes true only when every current input id is committed, and the process writes that before exit 0. A kill after the last row fsync and before that write leaves it false; the next `generate` sees no pending id, makes no provider call, and sets it. The flag is not a second copy of the committed keys; those stay the lines in `rows.jsonl`. `gate` and `push` refuse while it is false, so a crash at 3200 of 5000 cannot be published.

### Resume rules

`generate` resolves `--out` (default `./out`) before any provider call. A synthlite run is a directory that contains `rows.jsonl` or `state.json`. `.synthlite.lock` alone does not count.

- The directory does not exist: create it and start.
- The directory exists and is not a synthlite run: start. An empty directory starts.
- The directory is a synthlite run and `generator_config_hash` matches: resume. `--resume` is not required. A crash at 3200 of 5000 committed rows continues when the operator runs the same command again.
- The directory is a synthlite run and the hash differs from the current config: resume anyway. The run prints `config_change resuming under a new [generation] config (<field: old -> new; ...>)`, moves the previous `generator_config_hash` and `generator_config` into `state.json` `config_history` (oldest first, each with `superseded_at`), and makes the current config the run's config. Committed rows are not rewritten and keep the hash they were written under. This lets an operator finish a run after the original model or settings stop being usable. Returning to an earlier config removes it from the history and current becomes that one.
- The same, but a `--detailed` mode switch, or a committed row whose hash is neither the current one nor in `config_history`: exit 2, before any provider call. Tell the operator to pass a new `--out`. For the mode switch the message names the mode (section 14).

`--resume` is a strict alias for the matching-hash path, so a script can demand an existing run. It exits 2 when the directory is missing (`<out> does not exist; --resume does not create a directory`), when the directory is not a synthlite run (`<out> is not a synthlite run; --resume requires an existing run`), or when the hash mismatches. On a matching run it is the same resume as the automatic path.

On a start or a matching resume, before any provider call:

1. Repair torn tails as above.
2. Refuse if the current config is a `--detailed` mode switch from `state.json`, or if any committed row carries a hash that is neither the current one nor in `config_history`. A plain hash difference resumes and is recorded (section 10).
3. Scan `rows.jsonl`; the set of committed keys is the set of `source_task_id` values present.
4. Scan `rows.errors.jsonl`; a key is failed when it has at least one error record and no committed row.
5. Compute the current input's id set.

Then:

- an id in the input with a committed row: complete, no provider call;
- an id in the input with no row and no error: pending;
- an id in the input that is failed: stays failed unless `--retry-failed`, which makes it pending. Each re-queued item gets a fresh `max_attempts` budget in that run, backoff starts again from the first step, and the implicit output budget starts again at the key's ceiling. The recorded `attempt` keeps counting from the highest recorded one: with `max_attempts = 2`, a first run exhausts at attempt 2 and a retry run exhausts at attempt 4. A later success writes its row and the error records become history;
- a committed id that is not in the input: reported as dropped, its line left in place;
- parked keys stay parked unless `--resume-parked-keys`, which clears the parked list.

A prompt edit changes `source_task_id`. The old id is reported dropped. The new id is pending. Editing only a prompt record's `id` or `metadata` changes nothing: the committed row keeps the values it was written with.

## 11. Quality gate and split

```bash
synthlite gate
```

`--out` defaults to `./out`. `--config` is optional. There are no gate flags.

### Preconditions

Refuse, and write nothing under `DIR/data`, unless all hold:

- the `DIR/.synthlite.lock` lock is free (section 10);
- no torn tail in `rows.jsonl` or `rows.errors.jsonl` (`torn tail in the output directory; resume generate before gate`);
- `state.json` exists (`state.json is missing in <out>; generation is incomplete`) and `generation_complete` is true (`generation_complete is false; pending or failed rows are not publishable`). Generate sets that only when every current input id is committed. A pending or failed id leaves it false. Retry failed ids with `--retry-failed`. There is no partial-gate flag;
- gate and card settings resolve as in section 14. `[gate]` and `[card]` may both be absent;
- every line of `rows.jsonl` is a well-formed row, as below.

A malformed row is a writer bug, not a quality defect. The gate aborts on it, exit 2, instead of rejecting it. Each check names the 1-based line:

| Refusal | Rule |
|---|---|
| `rows.jsonl:<N>: not a synthlite.sft.v1 row` | the line is not JSON, or `schema_version` is not `synthlite.sft.v1` |
| `rows.jsonl:<N>: missing <field>` | `source_task_id` or `generator_config_hash` is missing or empty |
| `rows.jsonl:<N>: invalid source_task_id` | not `task_` followed by 64 lowercase hex characters |
| `rows.jsonl:<N>: generator_config_hash does not match state.json` | |
| `rows.jsonl:<N>: variant_index is missing` | |
| `rows.jsonl:<N>: messages is missing` | `messages` is not an array |
| `rows.jsonl:<N>: message role is missing` | |
| `rows.jsonl:<N>: message content is missing or empty` | any `content` that is not a non-empty string |
| `rows.jsonl:<N>: messages must be [system?, user, assistant]` | anything other than `[user, assistant]` or `[system, user, assistant]`: no user turn, extra turns, a trailing non-assistant turn, or an unknown role |

Whitespace-only content is not refused here; it goes through the per-row checks.

### Configuration

```toml
[gate]
min_assistant_chars = 200
max_assistant_chars = 40000
max_repeated_line = 4
refusal_window_chars = 300
refusal_phrases = [
  "i'm sorry, but i can't",
  "i can't help with",
  "i cannot help with",
  "i'm unable to help",
  "as an ai language model",
  "as an ai assistant",
  "as an ai,",
]
exclude_prompt_hashes = "holdout.sha256"   # optional
validation_per_mille = 30
```

`[gate]` itself may be omitted. Omitted keys use the defaults in that sample. There are no gate flags; a domain retunes these keys in `synthlite.toml`.

| Check | Default |
|---|---|
| `finish_reason` | `stop`, enforced at commit; the gate never sees another value |
| `min_assistant_chars` | 200 |
| `max_assistant_chars` | 40000 |
| `refusal_window_chars` | 300 |
| `refusal_phrases` | the seven strings in the sample above |
| `max_repeated_line` | 4, on a trimmed line of at least 20 characters |
| dedup | `duplicate_prompt`, then `duplicate_response`; first row kept |
| `exclude_prompt_hashes` | unset |
| `validation_per_mille` | 30 |

`gate_config_hash` is the lowercase SHA-256 hex, with no prefix, of this object serialized as in section 8, with all keys present and `exclude_prompt_hashes` replaced by the SHA-256 hex of that file's bytes, or `null` when the key is unset. A set key whose file is missing or unreadable refuses the gate; a typo must not silently disable decontamination.

### Normalization

`normalize(text)` is Unicode lowercase, split on whitespace, join with one space. This is Taskgen's prompt normalization. `norm_hash(text)` is the SHA-256 hex of the UTF-8 bytes of `normalize(text)`. Refusal matching additionally maps U+2019 to `'`.

### Per-row checks

Run on every row, in this order, collecting every reason that fires:

| Reason | Rule |
|---|---|
| `held_out` | `norm_hash(prompt)` is listed in `exclude_prompt_hashes` (one lowercase hex per line, blank lines ignored; key unset means no check) |
| `too_short` | assistant content shorter than `min_assistant_chars` characters |
| `too_long` | assistant content longer than `max_assistant_chars` characters |
| `refusal` | any `refusal_phrases` entry is a case-insensitive substring of the first `refusal_window_chars` characters of the assistant content. In a `--detailed` run, also of the first `refusal_window_chars` characters after the first `\n\n## Final answer\n` |
| `repetition` | some trimmed line of at least 20 characters occurs more than `max_repeated_line` times in the assistant content |

Length is counted in Unicode scalar values, not bytes and not tokens. Truncation is not a gate rule because a row with `finish_reason != "stop"` was never written.

A run is detailed when `state.json` has `generator_config.detailed` set to `true`; `gate` takes no flag for it. A detailed row opens with `## Decision trace` and its steps, so the opening window usually ends before the answer. The second window covers the final answer. Every other check runs on the whole rendered string as in a plain run, and the gate does not parse the trace. Plain runs are unchanged.

### Dedup

Rows with no per-row reason are visited in `rows.jsonl` file order:

- `duplicate_prompt`: `norm_hash(user content)` was already kept. With one variant per seed this also covers `(prompt, response)` pairs.
- `duplicate_response`: `norm_hash(assistant content)` was already kept. Identical replies to different prompts are a mode-collapse signal and are not trained on.

The first occurrence is kept. Two prompts that differ only in case or spacing have different ids, so both are generated, and the gate then keeps only the first. A seed generator that near-dedups prompts before publishing makes `duplicate_prompt` a guard for concatenated inputs, not the primary dedup.

### Split

Each kept row goes to `validation` when

```text
u32::from_str_radix(&source_task_id[5..13], 16) % 1000 < validation_per_mille
```

and to `train` otherwise. `validation_per_mille` is 0 through 1000; a larger value exits 2 at config load with `[gate].validation_per_mille must be between 0 and 1000`. 0 disables the split. If the validation set is empty, `data/validation.jsonl` is not written and the card declares only `train`.

### Outputs

- `data/train.jsonl`, `data/validation.jsonl`: kept rows, byte-identical to their `rows.jsonl` lines, in file order.
- `rejected.jsonl`: one object per rejected row with the work-item key, `reasons` (non-empty array), and for duplicates `duplicate_prompt_of` or `duplicate_response_of`, the `source_task_id` it duplicated. No prompt or response text.
- `README.md`, `manifest.json`: section 12.
- `gate.json`: `rows_sha256`, `gate_config_hash`, `train_sha256`, `validation_sha256` or `null`, `manifest_sha256`, `readme_sha256`, and `counts` (`committed`, `rejected` per reason, `train`, `validation`). `rows_sha256` is the SHA-256 of the `rows.jsonl` bytes the gate read and split, from a single read. A read failure is an error, not the digest of an empty file.

The gate is a pure function of `rows.jsonl`, `state.json`, the config, and the held-out file. Running it twice produces byte-identical `data/`, `README.md`, `manifest.json`, and `rejected.jsonl`. It rewrites those outputs atomically on every run.

## 12. Dataset layout, card, and manifest

### Files in the repo

| Path in the repo | Contents |
|---|---|
| `data/train.jsonl` | train split |
| `data/validation.jsonl` | validation split, when non-empty |
| `README.md` | dataset card |
| `manifest.json` | lineage and counts |

Not uploaded: `rows.jsonl`, `rows.errors.jsonl`, `rejected.jsonl`, any `.partial`, `state.json`, `gate.json`, `publish.json`, the config file, raw provider payloads, prompts as a debug dump.

### Dataset card

`README.md` front matter:

```yaml
---
pretty_name: <card.pretty_name>
license: <card.license, omitted when unset>
license_name: <card.license_name, when license is "other">
license_link: <card.license_link, when license is "other">
language: <card.language, default ["en"]>
task_categories:
  - text-generation
tags:
  - synthetic
  - sft
  - synthlite
size_categories:
  - <n<1K | 1K<n<10K | 10K<n<100K | 100K<n<1M, from train row count>
configs:
  - config_name: default
    data_files:
      - split: train
        path: data/train.jsonl
      - split: validation
        path: data/validation.jsonl
---
```

The card does not declare `dataset_info`, so `datasets` infers the features from `data/*.jsonl`. A declared feature list has to match every row exactly: `load_dataset` fails with `Couldn't cast array of type` on any field it does not declare. Row metadata varies by input: `input_metadata` is any JSON object, and Taskgen's `coordinates.curriculum.context` is a free-form string map, so no fixed declaration fits every run. Split sizes are not declared anywhere in the card, so a loader never fails a size verification; counts live in the manifest and the card body. The `validation` entry is omitted when that file is not written.

Every string scalar in the front matter (`pretty_name`, `license`, `license_name`, `license_link`, `language` entries) is YAML double-quoted. `\\`, `\"`, `\n`, `\r`, and `\t` are escaped; every other control character (U+0000–U+001F, U+007F, U+0080–U+009F) is written as `\uXXXX` with uppercase hex, so the front matter always parses. `license_name` and `license_link` are written only when `license` is `"other"`. `license`, `license_name`, and `license_link` are omitted when `card.license` is unset. No default license is invented. When `[card]` is omitted, `language` is `["en"]`. `pretty_name` is `[card].pretty_name` when set; otherwise it is the dataset name from `--hf-repo` when `push` is the process running `gate`, and `synthlite` when `gate` is invoked on its own.

The body starts with `# <pretty_name>`, with each control character replaced by a space. It states: row counts per split, each teacher (provider, model) with its row count, whether a system message was used, the gate summary (rejected count per reason), the split rule, `source_population_sha256`, `taskgen_run_id`, and that replies are unverified teacher generations. It links to `manifest.json`. It does not embed prompts, replies, system message text, or the config file.

A `--detailed` run adds one paragraph after `Replies are unverified teacher generations. They were not judged for correctness.`:

```text
Generated with --detailed: each assistant reply is a decision trace (## Decision trace, numbered Evidence / Hypothesis / Action / Verification / Conclusion steps, ## Final answer) from one teacher call. Proposed checks are text. No tool was executed and no model judged the answer.
```

Plain runs keep the card byte for byte.

### manifest.json

```json
{
  "schema_version": "synthlite.manifest.v1",
  "synthlite_version": "0.4.0",
  "generator_config_hash": "gen_<hash>",
  "generator_config": {},
  "gate_config_hash": "<hex>",
  "gate_config": {},
  "source": {
    "taskgen_schema": "scogo.taskgen.task.v2",
    "source_population_sha256": "<hex>",
    "taskgen_run_id": null,
    "seed_count": 5000
  },
  "teachers": [
    {"provider": "groq", "base_url": "https://api.groq.com/openai/v1", "model": "llama-3.3-70b-versatile", "rows": 5000}
  ],
  "counts": {
    "committed": 5000,
    "rejected": {"duplicate_response": 12, "refusal": 3, "too_short": 1},
    "train": 4834,
    "validation": 150
  },
  "split": {"method": "source_task_id_hex_mod_1000", "validation_per_mille": 30},
  "generated_at": {"first": "2026-09-22T09:14:03Z", "last": "2026-09-22T13:40:19Z"},
  "usage_totals": {"prompt_tokens": 4060000, "completion_tokens": 7300000, "rows_with_usage": 5000}
}
```

`generator_config` includes the system message text when one was configured; the operator wrote it and it is inside the hash, so it is lineage. In a `--detailed` run it carries `detailed: true`, the fixed system message, and the teacher instruction text. `teachers`, `generated_at`, and `usage_totals` are derived from row metadata (`teachers.rows` counts kept rows), so the manifest is reproducible from `rows.jsonl`. `teachers` groups by the requested model. The manifest never contains API key names, tokens, prompts, or replies.

## 13. Hugging Face publish

```bash
synthlite push --hf-repo OWNER/name
```

`generate` does not run `gate` or `push` and does not read `HF_TOKEN`. `push` is the only stage that talks to the Hub.

`--out` defaults to `./out`. `--hf-repo` is required and has no default. A missing `--hf-repo` (`--hf-repo is required`), a value that is not `owner/name` (`--hf-repo must be owner/name`), or a missing token (`HF_TOKEN is required`) exits 2 before the Hub and before `gate` runs. The token is `HF_TOKEN`, or `HUGGING_FACE_HUB_TOKEN` when `HF_TOKEN` is unset.

`push` refuses, exit 2, no Hub call, when another process holds `DIR/.synthlite.lock`, when a torn tail is still unrepaired (`torn tail in the output directory; resume generate before push`), or when `generation_complete` is not true (`generation_complete is false; pending work is not publishable`). That flag is the only pending-or-failed check. `push` does not read `rows.errors.jsonl`, so error records for seeds no longer in the input, for example after a prompt edit, do not block it.

When generation is complete and `data/train.jsonl` is missing, `push` runs `gate` and then continues. A zero-row train file counts as present. When `data/train.jsonl` is already present, `push` does not run `gate` again.

### When push is allowed

`push` holds the directory lock for the whole run, including that `gate`. After the completeness check, and after the optional `gate` above, refuse and do not call the Hub unless `gate.json` exists and its `rows_sha256`, `gate_config_hash`, `train_sha256`, `validation_sha256`, `manifest_sha256`, and `readme_sha256` all match the current files and config. A stale receipt means "run `gate` again".

### Repo and auth

`--hf-repo` is `owner/name`. The token is read from `HF_TOKEN`, or from `HUGGING_FACE_HUB_TOKEN` when `HF_TOKEN` is unset; the second name is deprecated upstream and kept only as a fallback. It must be a write-scoped or fine-grained token with write access to that repo. It is sent only as `Authorization: Bearer` to the configured Hub endpoint (`https://huggingface.co` unless `HF_ENDPOINT` is set), as described under Transport. It is never accepted on the command line and never written into any file in `DIR`, any uploaded file, or any log.

Create the repo as a private dataset when it does not exist. If it exists and is public, abort without uploading. `push` does not flip visibility.

### Transport

Hub calls go over `reqwest` with the same TLS stack as the provider client, in this order:

1. `GET /api/datasets/{repo}` to learn existence and `private`. On 404, `POST /api/repos/create` with `{"type": "dataset", "name", "organization", "private": true}`; a 409 there re-reads the repo. A public repo exits 2 (`<repo> is public; refusing to upload`). This runs on every push, including a no-op rerun.
2. Unless `--hf-republish`: the idempotence and resume check below, which may call `POST /api/datasets/{repo}/paths-info/main` and `POST /api/datasets/{repo}/tag/{commit_oid}`.
3. `POST /api/datasets/{repo}/preupload/main` with each file's path, size, and a base64 sample of its first 512 bytes. The response says `regular` or `lfs` per file.
4. For the `lfs` files: `POST /datasets/{repo}.git/info/lfs/objects/batch` with `{"operation": "upload", "transfers": ["basic"], "hash_algo": "sha256", "objects": [{"oid", "size"}]}`. For each object with an `upload` action, `PUT` the bytes to `upload.href` with the action's headers, then `POST` `{"oid", "size"}` to `verify.href` when present. An object with no `upload` action is already stored and is skipped.
5. `POST /api/datasets/{repo}/commit/main` as `application/x-ndjson`: one `header` line with the commit summary, one `file` line (base64 content) per `regular` file, one `lfsFile` line (`algo`, `oid`, `size`) per `lfs` file.
6. Write `publish.json` with `tag: null`.
7. `POST /api/datasets/{repo}/tag/{commit_oid}`, the oid returned in step 5 (never `main`, so a later commit by someone else cannot receive the tag), with `{"tag": "synthlite-<first 12 hex of manifest_sha256>"}`. A 409 means the tag already exists and is not an error.
8. Write `publish.json` again with the tag.

The commit summary is `synthlite <manifest_sha256[..12]>: <train> train, <validation> validation`. Push does not delete remote files and does not rewrite history. A changed dataset is a new commit and a new tag on the same repo.

The Hub token goes only to Hub API calls and the LFS batch call. The LFS `PUT` never carries it. The verify `POST` carries `Authorization: Bearer <token>` only when `verify.href` has the same origin (scheme, host, port) as the Hub endpoint and the verify action's headers have no `Authorization`.

Every Hub call has a 10 s connect timeout and a 120 s total timeout, except the LFS `PUT`, whose total timeout is 120 s plus 1 s per 256 KiB of object size (about a 2 Mbit/s floor; a 1 GiB object gets 4216 s). Any non-2xx status not handled above exits 1 with `hub <action> failed: <status>`.

### Idempotence and resume

`publish.json`:

```json
{
  "schema_version": "synthlite.publish.v1",
  "repo_id": "your-org/your-private-dataset",
  "endpoint": "https://huggingface.co",
  "commit_oid": "<oid>",
  "tag": "synthlite-<manifest_sha256[..12]>",
  "files": {"README.md": "<sha256>", "data/train.jsonl": "<sha256>", "manifest.json": "<sha256>"}
}
```

`tag` is `null` when the commit landed and the tag did not. When a later push finds a `publish.json` with the same repo id, endpoint, and file digests, it asks `paths-info` whether every file is present. A 404 from `paths-info` means not present, so a deleted repo is recreated in step 1 and fully uploaded again. Then:

- all present and `tag` set: print `push <repo> unchanged commit=<oid> tag=<tag>`, exit 0, no upload;
- all present and `tag` `null`: retry only the tag, on the recorded `commit_oid`, complete `publish.json`, print `push <repo> commit=<oid> tag=<tag>`, exit 0. No new commit;
- otherwise: upload and commit as above.

A failed tag exits 1 with `hub tag failed: <status>` and leaves the `tag: null` receipt for the next run. `--hf-republish` skips this check and makes a new commit even when the digests match.

## 14. CLI, configuration, and scheduling

Subcommands are `generate`, `gate`, and `push`. There are no other subcommands, no plugins, and no interactive prompts. `synthlite <input>` is exactly `synthlite generate <input>`. `gate` and `push` take no input path. `--out` defaults to `./out` for every subcommand.

| Subcommand | Flags |
|---|---|
| `generate <input>` | `--out DIR`, `--config PATH`, `--resume`, `--dry-run`, `--retry-failed`, `--resume-parked-keys`, `--detailed`, `--max-requests N`, `--max-rows N`, `--progress-interval SECONDS` (default 30, `0` turns it off) |
| `gate` | `--out DIR`, `--config PATH` |
| `push` | `--out DIR`, `--config PATH`, `--hf-repo OWNER/NAME`, `--hf-republish` |

`synthlite generate --help`, `synthlite gate --help`, and `synthlite push --help` print every flag of that subcommand with a one-line description to stdout and exit 0.

`synthlite --version` (or `-V`), alone, prints `synthlite <version>` from `Cargo.toml` to stdout and exits 0. The same version is the last field of the first `generate` line and is written to `state.json` as `synthlite_version`. With no arguments, or with only `--help` or `-h`, `synthlite` prints the short usage in `src/usage.txt` to stderr and exits 2. It does not print a generated flag dump. Any other argument error exits 2 with the full clap message on one line, naming the missing argument or unknown flag.

### Config discovery

`./synthlite.toml` is the file layer when that path exists. `--config PATH` selects another file and exits 2 if the path is missing (`config <path> does not exist`). Top-level tables other than `[generation]`, `[[key]]`, `[gate]`, and `[card]` exit 2 with `unknown config field <name>`. Unknown fields inside them exit 2 with `unknown [generation] field <name>`, `unknown [[key]] field <name>`, `unknown [gate] field <name>`, or `unknown [card] field <name>`. A TOML syntax error prints `config <path>: <message> (line N)` without quoting the source line. The tool does not search `$HOME` or parent directories. Flags override environment variables. Environment variables override the file. The file overrides built-in defaults.

A file that contains one or more `[[key]]` tables is the provider pool. `OPENAI_API_KEY`, `OPENAI_BASE_URL`, `OPENAI_MODEL`, and `SYNTHLITE_MODEL` do not rewrite those tables. `SYNTHLITE_SYSTEM_PROMPT`, when set and not whitespace, overrides `[generation].system_message`. Under `--detailed`, either one set exits 2 when loading for `generate` (section 5).

### Env key

When the file is absent, or it contains no `[[key]]` table, `generate` synthesizes one key. `gate` and `push` do not load keys.

| Field | Resolution |
|---|---|
| `provider` | `openai` |
| API key | `OPENAI_API_KEY`, required. Unset or empty exits 2 at config load with `OPENAI_API_KEY is required` |
| `base_url` | `OPENAI_BASE_URL`, otherwise `https://api.openai.com/v1`. Empty or whitespace-only falls back to the default |
| `model` | `SYNTHLITE_MODEL` if set, otherwise `OPENAI_MODEL`. If both are unset, exit 2 with one line: `set OPENAI_MODEL or SYNTHLITE_MODEL`. There is no default model |
| system message | `SYNTHLITE_SYSTEM_PROMPT`, otherwise `[generation].system_message`, otherwise none. Under `--detailed`, the fixed system message; an operator one exits 2 (section 5) |
| `max_concurrent` | 16, a ceiling for the adaptive limit (Admission, below) |
| `requests_per_minute` | unset. No client-side RPM cap |
| `timeout_seconds` | 600. An idle timeout: the longest wait for the response headers or for the next streamed chunk. A reply that keeps streaming is never cut, however long it runs |
| `max_attempts` | 5 |
| `rate_limit_cooldown_seconds` | 60 |
| `headerless_429_limit` | 3 |
| `max_tokens_field` | `max_completion_tokens` when the `base_url` host is exactly `api.openai.com`, otherwise `max_tokens` |
| `require_model_match` | true |
| `require_finish_reason` | true |

A file `[[key]]` that omits a transport field uses the same defaults, including `max_concurrent = 16`, no RPM cap, and the host-based `max_tokens_field`. An explicit `max_tokens_field` (`max_tokens` or `max_completion_tokens`) overrides the default; any other value exits 2 with `max_tokens_field must be max_tokens or max_completion_tokens`. Each file key requires `provider`, `base_url`, `model`, and `api_key_env` (`[[key]] <field> is required`). The file stores the variable name, never the secret. `api_key_env` must match `[A-Za-z_][A-Za-z0-9_]*`; anything else exits 2 with `[[key]] api_key_env must be an environment variable name ([A-Za-z_][A-Za-z0-9_]*), not a secret`, and the value is not echoed. A variable that is unset or empty fails config load before preflight with `<NAME> is required`. `max_concurrent`, `timeout_seconds`, `max_attempts`, `headerless_429_limit`, and `requests_per_minute` (when set) must be at least 1. Two `[[key]]` entries with the same id (section 10) exit 2 with `duplicate [[key]] <id>`.

### base_url

`OPENAI_BASE_URL` and `[[key]].base_url` must be an absolute `http://` or `https://` URL with a host and no query string or fragment. Otherwise config load exits 2 before any work starts, including under `--dry-run`, with one of:

- `base_url is required`
- `base_url is not a valid URL: <parser reason>`
- `base_url must use http or https`
- `base_url must have a host`
- `base_url must not contain a query string or fragment`

The raw URL is never echoed. Userinfo (`https://user:pass@host/...`) is stripped from the request URL and from `metadata.base_url`. It is not sent as basic auth.

`generate` does not read `HF_TOKEN`. `--max-requests` and `--max-rows` default to unset, which applies no cap.

### Directory rule

The same rule as section 10. A missing directory is created and the run starts. An existing directory that contains neither `rows.jsonl` nor `state.json` starts. A path that exists and is not a directory exits 2 (`<out> is not a directory`). A synthlite run whose `generator_config_hash` matches resumes with no extra flag, so a crash at 3200 of 5000 continues when the same command is run again. A hash difference resumes and is recorded in `config_history`; only a mode switch (below) or an unknown row hash exits 2 before any provider call. When `state.json` shows a mode switch, the message names the mode:

- `state.json` has `"detailed": true` and the current config is plain: `<out> was generated with --detailed; rerun with --detailed or pass a new --out`
- `state.json` has no `detailed` and the current config is detailed: `<out> was generated without --detailed; remove --detailed and [generation].detailed, or pass a new --out`
- a committed row whose hash is neither the current one nor in `config_history`: `generator_config_hash does not match <out>; pass a new --out`

Without the first message, an operator who reruns a crashed `--detailed` run without the flag would be told to start a new out dir, which would start a plain run. `--resume` does not accept the mismatch. `--resume` also exits 2 when the directory is missing or is not a synthlite run. It does not create a directory.

### First line and dry-run

`--dry-run` is a `generate` flag. It resolves the config, preflights, and prints the same line a real run prints (below). It does not create the out dir, does not take the lock, does not write, and does not call the provider. Under `--retry-failed` its `pending` count includes the failed ids. A plan that would start or resume exits 0. A hash mismatch exits 2, still with no provider call. A malformed input line exits 2 in preflight.

The first real `generate` prints one stderr line before any provider call:

```text
model=<model[,model...]> base=<host[:port][,...]> out=<DIR> seeds=<N> start|resume pending=<N> complete=<N> failed=<N> dropped=<N> version=<synthlite version>
```

`base=` is only `host[:port]` for every key (`base=api.openai.com`, `base=127.0.0.1:8000`), never the path, userinfo, or scheme. The line does not include the API key.

### Optional config file

No config file is required. When `./synthlite.toml` is present it has this shape:

```toml
[generation]
system_message = ""
temperature = 0.2
max_output_tokens = 2048        # omitted means the implicit 32768, stepped down on HTTP 400
# top_p, seed, frequency_penalty, presence_penalty, stop, reasoning_effort
# detailed = true              # decision traces; fixed system message, so leave system_message empty
# persona = "Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations"   # names the assistant in the detailed system message

[[key]]
provider = "groq"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
api_key_env = "GROQ_API_KEY"
timeout_seconds = 600
max_concurrent = 2              # omitted means 16 (adaptive ceiling)
requests_per_minute = 30        # omitted means no client-side RPM cap
max_attempts = 5
rate_limit_cooldown_seconds = 60
headerless_429_limit = 3
max_tokens_field = "max_tokens"   # omitted: max_completion_tokens for api.openai.com, else max_tokens
require_model_match = true
require_finish_reason = true

[gate]
# section 11

[card]
pretty_name = "IT Ops SFT"
license = "other"
license_name = "my-org-internal"
license_link = "https://example.com/licenses/internal"
language = ["en"]
```

`[generation]` holds the fields of the `generator_config_hash` object in section 8, except `schema_version` and `teacher_instruction`, which synthlite sets, and applies to every key. Omitted fields are `null` and are omitted from the HTTP body, except `max_output_tokens` (section 8). `detailed` must be a boolean, or config load exits 2 with `[generation].detailed must be a boolean`; `false` is the same as omitting it. `max_output_tokens` and `seed` must be integers; `max_output_tokens` must be at least 1, or config load exits 2 with `[generation].max_output_tokens must be >= 1`. `temperature`, `top_p`, `frequency_penalty`, and `presence_penalty` must be finite numbers >= 0. `stop` is an array of strings. `[[key]]` holds the teacher identity (`provider`, `base_url`, `model`), the credential name, and transport settings; none of these are in the hash. One `[[key]]` is enough. Several keys still pool, and each seed still gets one row.

### Decision-trace mode

```bash
synthlite prompts.jsonl --detailed
```

`--detailed` is a `generate` flag. It turns on decision-trace generation: the fixed system message (section 5), the larger hashed object (section 8), the trace parse and row layout (section 9), and the final-answer refusal window (section 11). A run is detailed when the flag is passed or `[generation].detailed = true`. There is no flag that turns it off; omit the key for a plain run. `gate` and `push` do not take `--detailed`. They read the mode from `state.json` (`generator_config.detailed`), never from the flag or the current config file.

It is still one provider call per seed, and `variant_index` stays 0. There is no judge, no second call, and no tool execution.

### Request

`POST {base_url}/chat/completions` with `Authorization: Bearer <key>` and `Content-Type: application/json`. Body: `model`, `messages`, the sampling fields `temperature`, `top_p`, `seed`, `frequency_penalty`, `presence_penalty`, `stop`, and `reasoning_effort` when not `null`, and the effective output-token cap under that key's `max_tokens_field`. The sampling fields are an explicit allowlist, so `detailed`, `teacher_instruction`, `system_message`, and `max_output_tokens` are never body keys, and a plain body is byte-identical to earlier versions. The cap is always sent: the explicit `max_output_tokens`, or the implicit budget (32768, or a lower step after a 400). A default request to OpenAI therefore carries `max_completion_tokens: 32768`. The body also carries `"stream": true` and `"stream_options": {"include_usage": true}`. `n`, `tools`, and `response_format` are never sent. Connect timeout is 10 seconds.

In a `--detailed` run the user message is still the input prompt. The system message sent to the provider is the training system message (section 5), a blank line, then the teacher instruction. The teacher instruction is the constant `TEACHER_INSTRUCTION` in `src/trace.rs`, with no trailing newline:

```text
Your reply trains an IT operations assistant. An on-call engineer reads the steps as an audit and acts on the final answer. They never see these instructions, so do not mention them.

Coverage. Be complete, not brief: every step adds a new fact, check, inference, or decision; never restate the task. Scale depth to the task: a few steps for a narrow question, many for a multi-cause incident or a production change.
- Answer every explicit request in the task and state material unknowns.
- Tie incident facts to supplied observations; label general domain knowledge as a hypothesis or conditional recommendation.
- For an incident, separate observed signals from possible causes and give a check that distinguishes each material cause.
- For a requested change, give staged steps with the precheck, approval, success check, stop condition, and rollback for each stage.
- For each proposed check, name the expected signal and explain how it changes the next decision.
- Prefer read-only checks. Bound risky actions and state approval needs.

Grounding. Observations come only from the task; nothing ran or changed unless the task says so. Never invent command output, logs, metrics, versions, device names, addresses, or topology. If a detail is missing, say so or make the advice conditional. Grounding limits observations, not expertise: apply your full knowledge of the protocols, products, defaults, and commands involved, say what it implies for this case, and label it as general knowledge. Draw every inference the supplied evidence supports, including what it already rules out.

Steps. Each step is one paragraph with no line breaks, written as a publishable audit, not private chain-of-thought. Evidence cites a supplied observation. A hypothesis names a possible cause and what would confirm or rule it out. An action says what to run or request, why, and what each likely result would mean; never its result. Give the exact read-only command or query when the platform is known; if unsure of the exact syntax, name the output to look for instead of guessing. A verification confirms a named hypothesis or change. A conclusion states the decision, confidence, and open questions.

Final answer. The complete answer in the role and format the task asks for, usable without the steps. Do not introduce yourself or name the assistant. Use Markdown where it helps; headings start at ###.

Before replying, check that the final answer covers each explicit request in the task, then check the draft against Coverage and Grounding and fix gaps.

Output. One JSON object and no other text, with exactly two keys: reasoning_steps, then final_answer. reasoning_steps is an array of at most 24 objects, each with exactly two keys, kind and content. kind is one of evidence, hypothesis, action, verification, conclusion; use each at least once and repeat as needed. content and final_answer are non-empty JSON strings. End the reply with the closing brace of the object.
```

The teacher instruction is sent only in the request. It is hashed (section 8) and never stored in `messages`. If this block and `src/trace.rs` ever differ, the source is the contract. Sampling, streaming, retries, budgets, and key admission are the same as in a plain run.

The reply arrives as server-sent events. Each `data:` chunk's `choices[0].delta.content` is appended to the reply; `delta.reasoning` and `delta.reasoning_content` are counted for progress (section 15) and discarded. `delta.refusal` text is collected as `message.refusal`. The last non-null `finish_reason`, the first `model` (recorded as `metadata.served_model`), and the `usage` object from the final chunk are kept. The folded result goes through the same acceptance rules as a non-streamed body (section 9), so a `length` stop is never committed, however much text arrived. The stream ends at `data: [DONE]`. A server that answers `stream: true` with a plain `application/json` body is read as a non-streamed response, and its body `model` is `metadata.served_model`.

`timeout_seconds` is an idle timeout, not a limit on the whole request. The request fails with a retryable `timeout` only when the response headers, or the next chunk, take longer than that. A 20-minute reply that streams steadily completes. Retryable stream failures, each with `max_attempts` and backoff like a 5xx:

| `reason` | Cause |
|---|---|
| `timeout` | No headers or no chunk for `timeout_seconds` |
| `stream_cut` | The connection closed before `[DONE]` and before any `finish_reason` |
| `stream_error` | A chunk carried a non-null `error` object |
| `bad_body` | A chunk was not JSON, or a 2xx stream or body had no `choices` |

A retried attempt starts the reply from scratch. Nothing from the partial stream is kept.

### Admission

A request is admitted only when that key has a free concurrency slot. When `requests_per_minute` is set, the key must also have fewer than that many admissions in the trailing 60 seconds. When it is unset, there is no client-side RPM cap; a 429 still cools the key down as below. `max_concurrent` is a ceiling, not a fixed count. A key that omits it, including the synthesized env key, has 16. Each key keeps an adaptive limit that starts at `max_concurrent`: a 429 halves it (minimum 1) and prints `throttle <id> max_concurrent=<from>-><to> reason=429`; after as many consecutive non-429 HTTP answers as the current limit, it grows by one, back up to `max_concurrent`, printing `throttle <id> max_concurrent=<from>-><to>`. A key is admitted only below its current limit. This finds a hosted provider's rate limit without configuration, and a self-hosted server runs at the full ceiling. Keys do not borrow each other's slots. Two keys are independent buckets even when they name the same model.

A burst of requests admitted together can all come back 429. Each key has an epoch that the first 429 bumps, and a 429 from a request admitted in an earlier epoch only returns its item to pending: it does not halve the limit again, reset the clean streak, or count toward `headerless_429_limit`. A high concurrency default would otherwise collapse to 1 and park the key on a single burst.

Retries for transport failures, 2xx bodies that do not parse or have no `choices[0]`, HTTP 408, 425, and any 5xx use exponential backoff starting at 1 second, doubling, capped at 60 seconds, with full jitter in whole seconds, up to `max_attempts` tries per work item per run. Exhausting that limit records a permanent failure with class `retries_exhausted` and the last HTTP status, if any. Any other non-2xx status except 401, 403, and 429 is permanent with class `invalid_request`, except HTTP 400 while the implicit budget is in use, which steps the budget down first (section 9).

### 429

Honor `Retry-After` when it parses as delta-seconds or an HTTP date, and cool that key down until then. Otherwise cool the key down for `rate_limit_cooldown_seconds`. The work item stays pending. It is not a permanent failure and does not count against `max_attempts`. The scheduler does not tight-loop the key.

After `headerless_429_limit` consecutive 429 responses on one key that lack a parseable `Retry-After`, park the key with reason `headerless_429` and print `parked <id> headerless_429`. The count is per key. A 429 with a parseable `Retry-After`, a success, a permanent class such as `truncated` or `refusal`, or a retryable HTTP status such as a 5xx resets it. A transport failure with no HTTP status (timeout, connection error) does not. A parked key accepts no further admissions until the operator passes `--resume-parked-keys`. Parking survives a normal resume. Other keys continue. If every key is parked, exit 3 with `all keys are parked; pending work remains`.

HTTP 401 or 403 parks that key with reason `unauthorized`, prints `parked <id> unauthorized`, and returns the work item to pending. It does not mark the row failed. Parked keys are written to `state.json` `parked_keys` when they park.

### Cost caps

- `--max-requests N`: stop admitting after N provider calls in this invocation, counting retries. In-flight requests finish and commit. Unset applies no cap.
- `--max-rows N`: stop admitting after N new rows have been committed in this invocation. Unset applies no cap.
- `--dry-run`: the plan in "First line and dry-run" above. No HTTP.

Either cap stopping the run before all work is committed exits 3 with `stopped by --max-requests or --max-rows; pending work remains`. `generation_complete` stays false.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | `generate`: every work item committed. `gate`, `push`: the stage completed, including a no-op push. `--dry-run`: the plan would start or resume. `<subcommand> --help` and `--version` |
| `1` | Unexpected error: I/O, disk full, a Hub status such as `hub tag failed: <status>`, a transport error |
| `2` | Refused: argument error, no-args or top-level `--help` usage, config (including `base_url` and value checks, and an operator system message under `--detailed`), missing model or key, preflight, hash mismatch including a `--detailed` mode switch (also under `--dry-run`), `--resume` on a missing directory or a directory that is not a run, another process holding `.synthlite.lock`, gate or push precondition, public repo |
| `3` | `generate` stopped with pending work: `stopped by --max-requests or --max-rows; pending work remains`, `all keys are parked; pending work remains`, or `no key can admit pending work` |
| `4` | `generate` has no pending work left but some work items failed permanently, including any `invalid_trace`: `<N> work items failed; rerun with --retry-failed`. `generation_complete` is false. A rerun without `--retry-failed` on a directory whose only unfinished ids are failed also exits 4 |

Transport errors from any HTTP call (provider, Hub, LFS) are printed without the request URL, for example `error sending request: client error (Connect): tcp connect error: ...`.

## 15. Observability

The first real `generate` prints the one resolved-config line from section 14 before any provider call. While it runs, all output goes to stderr, one `key=value` line per event:

| Line | When |
|---|---|
| `<DD-MM-YY:HH:MM:SS> progress requests=<N> committed=<N>/<seeds> in_flight=<N> queued=<N> failed=<N> retry=<N> [budget_retry=<N>] [rate_limit=<N>] tok_s=<F> input_tokens=<N> output_tokens=<N> elapsed=<D> [usage_missing=<N>] finish_in=<D or ?>` | Every `--progress-interval` seconds (default 30; `0` turns it off), even when no request has finished. The leading timestamp uses the machine's local time. `committed` includes earlier invocations of the same output directory. `tok_s` is provider-reported output tokens from completed attempts divided by elapsed invocation time, so it lags active streams. Input and output totals count usage on all completed attempts, including failures and retries; in-flight attempts are included when they finish. Optional counters appear only when nonzero. `finish_in` is an approximate remaining duration based on completed reply size and streaming activity, including when only in-flight replies remain. It is `?` until there is enough data |
| `retry <id> try=<n>/<max_attempts> reason=<reason> backoff=<D>` | A transient failure goes back to pending. `reason` is `timeout`, `connect`, `transport`, `stream_cut`, `stream_error`, `bad_body`, or `http_<status>` (section 14) |
| `budget <id> max_output_tokens=<from>-><to> reason=http_400` | The provider rejected the implicit budget; the item retries at the next lower one (section 9) |
| `budget <key id> max_output_tokens=<from>-><to>` | A stepped-down budget succeeded; later items on that key start there |
| `throttle <key id> max_concurrent=<from>-><to> [reason=429]` | The key's adaptive concurrency limit changed (section 14) |
| `failed <id> class=<class> status=<status or none> max_output_tokens=<N>` | A permanent failure is written to `rows.errors.jsonl`. `class` is any permanent class: `truncated`, `content_filter`, `unexpected_finish_reason`, `no_finish_reason`, `empty_assistant`, `refusal`, `model_mismatch`, `invalid_request`, `retries_exhausted`, or, in a `--detailed` run, `invalid_trace` (`failed <id> class=invalid_trace status=200 max_output_tokens=<N>`) |
| `parked <id> <reason>` | A key parks (section 14) |
| `preflight skipped <N> Taskgen candidates with deterministic hard failures` | Once, before the first line, when a `candidates.jsonl` input has candidates that Taskgen's deterministic checks failed (section 7) |

`<id>` in the retry, budget, and failed lines is the first 12 hex digits of `source_task_id` (`task_cc3e0bec7b87`), enough to grep `rows.jsonl` and `rows.errors.jsonl`. `<D>` is `45s`, `12m04s`, or `1h15m05s`. Input and output totals use the provider's `usage.prompt_tokens` and `usage.completion_tokens`; output includes reasoning. If a 2xx reply lacks valid usage, or a stream is interrupted before usage arrives, `usage_missing` increments and the displayed totals are lower bounds. Rejected HTTP requests often do not report usage. Internal streamed-piece counts support the rough `finish_in` estimate but are not exposed as tokens. `committed` counts rows from earlier runs of the same out dir; all token and request counters describe this invocation. [running.md](running.md) has a field-by-field reading of one line. Rows committed and 429s are not printed one by one; the heartbeat counts them.

At the end it prints `done requests=<N> committed=<N> failed=<N> pending=<N> success=<N> rate_limit=<N> retry=<N> budget_retry=<N> input_tokens=<N> output_tokens=<N> usage_missing=<N> elapsed=<D>` (a run with nothing pending prints `done requests=0 committed=<N> failed=<N> pending=0`), then the exit message, if any. `gate` prints `gate train=<N> validation=<N> rejected=<N>`; per-reason counts are in `gate.json`, the manifest, and the card. `push` prints `push <repo> commit=<oid> tag=<tag>`, or `push <repo> unchanged commit=<oid> tag=<tag>` for a no-op. Prompts, replies, keys, authorization headers, and response bodies are not printed. Base URLs are printed as `host[:port]` only; request URLs are dropped from transport errors.

There is no event-log file. `rows.errors.jsonl` and `rejected.jsonl` are the durable records.

## 16. Security, privacy, and provider boundaries

- API keys and the Hugging Face token come from the environment. They are never on the command line, never committed, logged, or copied into `DIR` or the dataset.
- stderr never carries authorization headers, request URLs from transport errors, raw `base_url` values, or the `api_key_env` value when it is rejected.
- Prompts and replies appear only in `rows.jsonl`, the append-only `.partial` quarantine files, and `data/`. Error and rejection sidecars carry ids and reasons. The card and manifest carry counts and hashes.
- Local output stays on disk until `push`. Push targets a private dataset repo only. The Hub token is never sent to an LFS upload URL, and to a verify URL only on the Hub's own origin.
- Only documented APIs and documented quotas are used.
- The operator confirms that prompts may be sent to each configured provider, that the provider's terms allow the intended use of its outputs, and that the Hugging Face account may store the generated rows.
- The tool does not automate account creation, rotate identities to evade quotas, scrape browser sessions, or bypass CAPTCHAs.
- Format readiness and gate passage are properties of the row. They are not a claim that an answer is factual, safe, or original.

## 17. Failure modes and mitigations

| Failure | Mitigation |
|---|---|
| Malformed input line | Exit 2 during preflight, before any provider call |
| Prompt record with an extra key | Exit 2: `unknown field <k> (put extra fields under "metadata")` |
| Line with an unknown `schema_version` | Exit 2: `unsupported schema_version <v>` |
| The same prompt twice | Exit 2: `duplicate prompt (same prompt as line <M>)` |
| Input is Taskgen's `candidates.jsonl` | Each `scogo.taskgen.candidate.v1` line is unwrapped to its task. Candidates with deterministic hard failures are skipped and counted on stderr. Use `tasks.jsonl` for Taskgen runs with a model review |
| Key env var unset | Exit 2 at config load |
| Invalid `base_url`, `max_output_tokens < 1`, `validation_per_mille > 1000`, `api_key_env` not a variable name, duplicate `[[key]]` | Exit 2 at config load, before preflight |
| Second process on the same `--out` | Exit 2, `another synthlite process is using <out>`; no provider call |
| Model unset on the env key | Exit 2, one line: `set OPENAI_MODEL or SYNTHLITE_MODEL`. No provider call |
| 429 with `Retry-After` | Cool down that key; keep the work item pending |
| Repeated 429 without `Retry-After` | Park the key until `--resume-parked-keys` |
| 401 or 403 | Park the key; the item stays pending; exit 3 if every key is parked |
| `finish_reason: length` | Permanent failure `truncated` at once, at the implicit 32768 or the explicit budget. No row |
| HTTP 400 with the implicit budget | Retry at the next lower budget (16384, 8192, 4096); the key remembers the one that works. A 400 at 4096 is `invalid_request` |
| Empty or null assistant text, or `refusal` set | Permanent failure; no row |
| Provider returns a different model id | Permanent failure `model_mismatch` when `require_model_match` is set. A dated snapshot of the requested id (`-YYYY-MM-DD` or `-NNNN`) is accepted. A router key sets `require_model_match = false` and reads `metadata.served_model` |
| Items fail permanently, nothing pending | Exit 4; rerun with `--retry-failed`, which gives each item a fresh `max_attempts` budget |
| Kill during a line write | Next resume appends the tail to the `.partial` file, fsyncs it, then truncates to the last newline; that item runs again |
| Disk full | Truncate the torn tail; exit 1; committed lines untouched |
| Input prompt edited | New `source_task_id` is pending; old id is reported dropped; old line stays |
| Config hash differs from `state.json` or a row | Exit 2 before any provider call. Pass a new `--out`. `--resume` does not accept the mismatch |
| `--detailed` added or dropped on an existing out dir | Exit 2 before any provider call, with a message that names the mode: rerun with `--detailed` on a detailed out dir, drop it on a plain one, or pass a new `--out` |
| Operator system message with `--detailed` | Exit 2 before preflight: `--detailed uses a fixed system message; unset SYNTHLITE_SYSTEM_PROMPT and [generation].system_message, and use [generation].persona to name the assistant`. No provider call. `gate` and `push` do not apply the check |
| Teacher ignores the JSON contract under `--detailed` (a memo, prose after the object, a missing kind, more than 24 steps) | Permanent `invalid_trace` on the first occurrence, `http_status` 200, no retry, no reply text in the sidecar or on stderr; exit 4. Run the canary first; `--retry-failed` re-spends |
| `--resume` and the directory is missing or has no synthlite run | Exit 2. The flag does not create a directory |
| Failed item, process restarted | Stays failed until `--retry-failed` |
| Misconfigured run | `--dry-run` shows the plan; `--max-requests` bounds spend |
| Teacher repeats one answer | `duplicate_response` rejection; count visible in manifest |
| Teacher refuses | `refusal` rejection with a configurable phrase list |
| Detailed trace whose final answer opens with a refusal | `refusal` rejection: in a `--detailed` run the refusal window also covers the start of the final answer |
| Eval prompts in train | `exclude_prompt_hashes` rejection `held_out` |
| Gate while `generation_complete` is false, or a tail is torn | Exit 2 before writing `data/` |
| Push while `generation_complete` is false, or a tail is torn | Exit 2, no Hub call. Error records for seeds no longer in the input do not block it |
| Push with `data/train.jsonl` missing and generation complete | Run `gate`, then upload |
| Push with `data/train.jsonl` present and `gate.json` missing or stale | Exit 2 before any Hub call |
| Remote repo is public | Exit 2 without uploading, even when nothing changed |
| Same bytes pushed again | No-op when `publish.json` matches and remote files are present |
| Commit landed, tag failed | Exit 1; `publish.json` has `tag: null`; the next push retries only the tag |
| Remote repo deleted after a push | `paths-info` 404 counts as not present; the repo is recreated and fully uploaded |
| Large LFS object | `PUT` timeout grows by 1 s per 256 KiB over the 120 s base |

## 18. Testing strategy

1. Identity golden tests: the prompt golden in section 8, `tests/fixtures/canonical/valid-task.json`, the unicode Taskgen prompt, and a prompt containing `&`, `<`, and `>`. Population digest golden `f01f0b22…`. Assert the work-item key includes `variant_index` 0. Assert `generator_config_hash` changes when any `[generation]` field changes, including `1.0` versus `1`, and does not change when keys, `api_key_env`, timeouts, RPM, `max_tokens_field`, or gate settings change. No provider client in this slice.
2. Input formats. A `.txt` file (also `.TXT`) gives one prompt per non-empty trimmed line and skips blank lines. A text line and a prompt record with the same prompt share an id; `id` and `metadata` do not change it. An extra prompt-record key exits 2 with the `unknown field` message; an unknown `schema_version` exits 2 with `unsupported schema_version`; a repeated prompt exits 2 as a duplicate. A file that mixes prompt records, task records, and candidate wrappers loads. Preflight also covers the first bad line with no HTTP, duplicate ids, `language` bounds, candidate wrappers unwrapped, hard-failed candidates skipped, a candidate and its task sharing one id, a wrapper without `candidate` refused, and the `run.json` sidecar read.
3. Fake OpenAI-compatible server (`wiremock`). One provider, one key. Single-writer commit. Fault injection that kills after a partial line write to `rows.jsonl` and to `rows.errors.jsonl`. A second invocation of the same command, with no `--resume`, truncates, quarantines, and makes exactly one provider call for that item. Unaffected lines are byte-identical before and after. `--resume` on a missing or empty directory exits 2. A hash mismatch exits 2 with or without `--resume`.
4. Same fake server, one key shape: idle timeout on headers and mid-stream (a reply streaming longer than `timeout_seconds` in total still commits), a stream cut before `[DONE]`, SSE assembly (content kept, reasoning dropped, usage from the final chunk, a streamed `length` stop never committed), a plain JSON answer to a streamed request, RPM window, per-key concurrency, 429 with `Retry-After`, 429 without it until the key parks, 401 parks, `finish_reason` `length` and `content_filter`, missing `finish_reason` with both `require_finish_reason` values, null content, `refusal`, `require_model_match`, `usage` present and absent, errors sidecar contents and redaction, `--max-requests`, `--max-rows`, `--dry-run` making no HTTP call and printing seed count, model, base URL, out dir, and start or resume, exit codes. Row metadata: a prompt row has `null` taxonomy fields, `coordinates` `{}`, `input_id`, and `input_metadata`; a Taskgen row keeps its keys; `served_model` comes from the first streamed `model`, from a JSON body, or is `null` when the provider sends none. Missing `OPENAI_MODEL` and `SYNTHLITE_MODEL` on the env key exits 2 with `set OPENAI_MODEL or SYNTHLITE_MODEL` and no HTTP. No arguments prints the short usage and exits 2. `generate --help`, `gate --help`, and `push --help` print every flag to stdout and exit 0. `--version` and `-V` print `synthlite <version>` to stdout and exit 0.
5. A second provider in the same run. Two keys with the same model name and different `provider` or `base_url` do not share a rate-limit bucket; parking one does not affect the other; every seed still gets exactly one row; each row's `metadata.provider`, `base_url`, and `model` name the key that served it; the manifest's `teachers` counts match.
6. Gate fixture: a `rows.jsonl` with at least one row per rejection reason and clean rows. Golden `rejected.jsonl`, golden split membership, card front matter parses as YAML and matches a golden, manifest matches a golden. Running gate twice gives byte-identical outputs. Gate refuses on `generation_complete` false, a torn tail, and a hash mismatch. An omitted `card.license` omits `license`, `license_name`, and `license_link` from the card.
7. Fake Hub (`wiremock`): create with `private: true`, `preupload` returning both `regular` and `lfs`, LFS batch and `PUT`, commit NDJSON with exactly the four paths, tag including the 409 case, `paths-info` via `POST`, and a rerun that retries only a missing tag. `push` with `data/train.jsonl` missing and generation complete runs `gate` then uploads. `push` refuses when `generation_complete` is false, on a missing or stale `gate.json` when `data/train.jsonl` is already present, on a public repo, when `--hf-repo` is missing, and when the token env is unset. `generate` does not call the Hub. The token appears only in `Authorization` headers. Second push of the same bytes makes no upload.
8. Decision traces (`--detailed`). Persona: an unset persona gives the default fixed system message and the earlier hash; a persona replaces the role phrase and changes the hash; an empty or multi-line persona and a persona without `--detailed` exit 2 for `generate` but not for `gate`. Card: no `dataset_info`; curriculum rows load with `datasets`. Parser: accepts five kinds, repeats, a fenced object, a preamble plus a fenced object, a reply missing only its final `}`, and 24 steps, and trims; rejects a missing kind, 25 steps, an extra top-level key such as `tool_calls`, an extra step key, a title-case kind, blank content, a heading line inside a step, a JSON array, trailing prose, a memo, and a markdown document that already looks like a rendered trace. Render matches section 9 byte for byte. Hash: an all-default plain config still hashes to `gen_48ca1520…e172`, `detailed = false` equals omitted, `--detailed` equals `detailed = true`, a non-boolean exits 2, and an operator system message exits 2 for `generate` but not for `gate`. Provider: the detailed body has no `detailed`, `teacher_instruction`, `tools`, `response_format`, or `n`; its system message is the fixed system message, `\n\n`, and the teacher instruction; the plain body is unchanged. Fake server: a detailed seed commits a rendered row with the fixed system message and no teacher instruction; a memo makes exactly one request, writes `invalid_trace`, exits 4, and leaks no reply text; a mode switch exits 2 with the mode-aware message and no request. Gate: a refusal that opens the final answer is rejected only in a detailed run; the card paragraph appears only in detailed runs.
9. Manual canary, before a release: a fixed prompt file, one real key, `--max-requests` set, then `gate`, then `push` into a throwaway private repo. Load it back with `datasets.load_dataset` and check split counts against the manifest. Also a static release build.

## 19. Example commands

```bash
# generate. creates ./out, or resumes it when the hash matches
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=your-model-id
synthlite path/to/prompts.jsonl

# crash at 3200 of 5000: same command, committed rows are skipped
synthlite path/to/prompts.jsonl

# a plain text file: one prompt per line
synthlite path/to/prompts.txt --out out/questions

# generation complete: gate if data/train.jsonl is missing, then a private repo
export HF_TOKEN=hf_...
synthlite push --hf-repo your-org/your-private-dataset

# strict: exit 2 if ./out is missing, not a run, or the hash mismatches
synthlite path/to/prompts.jsonl --resume

# plan only. exit 0, no provider call
synthlite path/to/prompts.jsonl --dry-run

# bounded run in an explicit directory
synthlite path/to/prompts.jsonl --out out/itops-v1 --max-requests 200

# failed rows stay failed (exit 4) until this; fresh retry budget per item
synthlite generate --retry-failed path/to/prompts.jsonl

# stages separately. both default --out to ./out
synthlite gate
synthlite push --hf-repo your-org/your-private-dataset

# every flag of one subcommand
synthlite generate --help

# decision traces: a 20-prompt canary file on a new out dir; pass --detailed again to resume it
head -n 20 path/to/prompts.jsonl > canary.jsonl
synthlite canary.jsonl --detailed --out out/detailed-canary
```

`generate` is safe to interrupt. Running the same command again resumes and skips committed rows, also under a changed `[generation]` config (recorded in `config_history`). `--resume` is optional on that path and errors when the directory is empty. `gate` and `push` run only when `generation_complete` is true. `push` runs `gate` when `data/train.jsonl` is missing.

## 20. References

Sources consulted for this design.

- TRL dataset formats, conversational `messages` with `role`/`content`: https://huggingface.co/docs/trl/main/en/dataset_formats
- Axolotl `chat_template` datasets, `field_messages`, `roles_to_train`: https://docs.axolotl.ai/docs/dataset-formats/conversation.html
- Unsloth datasets guide, role/content versus ShareGPT: https://docs.unsloth.ai/basics/datasets-guide
- `datasets` JSON and JSON Lines loading: https://huggingface.co/docs/datasets/en/loading
- Hub file formats, Parquet versus JSON Lines guidance: https://huggingface.co/docs/hub/en/datasets-adding
- Hub split inference from file and directory names: https://huggingface.co/docs/hub/en/datasets-file-names-and-splits
- Hub manual configuration, `configs` and `data_files`: https://huggingface.co/docs/hub/en/datasets-manual-configuration
- Dataset card metadata (`license`, `language`, `task_categories`, `configs`, `dataset_info`): https://huggingface.co/docs/hub/en/datasets-cards and https://github.com/huggingface/hub-docs/blob/main/datasetcard.md
- `huggingface_hub` upload guide (`upload_folder`, `create_commit`, `preupload_lfs_files`; `upload_large_folder` deprecated): https://huggingface.co/docs/huggingface_hub/en/guides/upload
- Hub commit wire format (`preupload`, NDJSON `header`/`file`/`lfsFile`), LFS batch, `create_repo`, `tag`, `paths-info`: https://github.com/huggingface/huggingface_hub/blob/main/src/huggingface_hub/_commit_api.py, https://github.com/huggingface/huggingface_hub/blob/main/src/huggingface_hub/lfs.py, https://github.com/huggingface/huggingface_hub/blob/main/src/huggingface_hub/hf_api.py
- Xet storage, LFS uploads migrated server-side: https://huggingface.co/docs/hub/en/xet/legacy-git-lfs and https://huggingface.co/docs/hub/en/repositories-getting-started
- `HF_TOKEN`, `HUGGING_FACE_HUB_TOKEN` deprecation: https://huggingface.co/docs/huggingface_hub/en/package_reference/environment_variables
- Hub token roles (read, write, fine-grained): https://huggingface.co/docs/hub/en/security-tokens
- OpenAI chat completion `finish_reason` values (`stop`, `length`, `content_filter`, `tool_calls`): https://github.com/openai/openai-openapi
- OpenAI Batch API, 50% discount, 24-hour window, `custom_id`: https://platform.openai.com/docs/guides/batch
- Anthropic Message Batches, 50% discount, 100k requests or 256 MB per batch, 24-hour expiry: https://docs.claude.com/en/docs/build-with-claude/batch-processing
- OpenRouter free models router and free-tier limits: https://openrouter.ai/docs/api-reference/limits
- Lee et al., Deduplicating Training Data Makes Language Models Better (ACL 2022): https://arxiv.org/abs/2107.06499
- BigCode near-deduplication with MinHash LSH: https://huggingface.co/blog/dedup
- Shumailov et al., The Curse of Recursion / model collapse (Nature 2024): https://arxiv.org/abs/2305.17493 and https://www.nature.com/articles/s41586-024-07566-y
- Wang et al., Self-Instruct, filtering invalid and similar generations: https://arxiv.org/abs/2212.10560
- Zhou et al., LIMA: Less Is More for Alignment: https://arxiv.org/abs/2305.11206
- Dubois et al., Length-Controlled AlpacaEval, verbosity bias: https://arxiv.org/abs/2404.04475
- Yang et al., Rethinking Benchmark and Contamination with Rephrased Samples: https://arxiv.org/abs/2311.04850
- Xu et al., Magpie, filtering large synthetic alignment sets: https://arxiv.org/abs/2406.08464
- Chen et al., On the Diversity of Synthetic Data and its Impact on Training LLMs: https://arxiv.org/abs/2410.15226
- Alpaca-cleaned dataset card, catalogue of teacher-output defects (empty outputs, hallucinated inputs, merged instructions): https://huggingface.co/datasets/yahma/alpaca-cleaned
