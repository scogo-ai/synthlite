# synthlite

**Prompts in, a private fine-tuning dataset out.**

One static binary · one teacher call per prompt · no LLM judge · crash-safe resume · any OpenAI-compatible endpoint

[![CI](https://github.com/scogo-ai/synthlite/actions/workflows/ci.yml/badge.svg)](https://github.com/scogo-ai/synthlite/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/scogo-ai/synthlite)](https://github.com/scogo-ai/synthlite/releases/latest)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

![synthlite generating 10 rows on OpenRouter's free router, interrupted and resumed with no duplicates, then gated and pushed to a private dataset](docs/assets/synthlite-demo.gif)

<sub>Ten prompts on OpenRouter's free router: interrupt, resume without duplicates, gate, private push. Waits for replies are cut; the timestamps show real time.</sub>

## What it does

You give synthlite a file of prompts. It asks a teacher model for one reply per prompt, filters the replies with fixed rules, and uploads a private Hugging Face dataset.

```text
prompts.jsonl ──generate──▶ out/rows.jsonl ──gate──▶ out/data/{train,validation}.jsonl ──push──▶ private HF dataset
               one call per prompt,          length, refusal, repetition,         card + manifest,
               resumable, capped             exact dedup, fixed split             one tagged commit
```

Each stage writes plain files you can read before the next one.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/scogo-ai/synthlite/main/install.sh | sh
```

Other ways:

- **Release binaries** for Linux (x86_64, aarch64; static) and macOS (arm64, x86_64): [releases page](https://github.com/scogo-ai/synthlite/releases/latest). Check them against `SHA256SUMS`.
- **Docker:** `docker run --rm ghcr.io/scogo-ai/synthlite --version`
- **From source** (Rust 1.89+): `cargo install --locked --git https://github.com/scogo-ai/synthlite`

## Quickstart for $0

This uses OpenRouter's free router, so the teacher costs nothing. Get a key at [openrouter.ai](https://openrouter.ai/) and a Hugging Face token with write access.

```bash
git clone --depth 1 https://github.com/scogo-ai/synthlite && cd synthlite   # for the examples
export OPENROUTER_API_KEY=sk-or-...
synthlite examples/prompts.jsonl --config examples/configs/openrouter-free.toml
synthlite gate
export HF_TOKEN=hf_...
synthlite push --hf-repo you/my-first-sft
```

1. `synthlite examples/prompts.jsonl ...` generates 25 IT-operations replies into `./out`. Every company, host, and address in `examples/` is fictional. If it stops, run the same command again. It resumes. Exit code 4 means some prompts failed; see [docs/running.md](docs/running.md).
2. `synthlite gate` filters and splits. Read `out/rejected.jsonl` and `out/README.md` before you publish.
3. `synthlite push` creates a **private** dataset repo and uploads it.

Before you publish rows or train on them, check the terms of each model that answered. The free router picks a model per request, and some free endpoints allow internal evaluation only; NVIDIA's API trial service, for example, forbids sharing its outputs. `metadata.served_model` names the model behind every row.

With OpenAI instead, no config file is needed:

```bash
export OPENAI_API_KEY=sk-... OPENAI_MODEL=gpt-4.1-mini
synthlite examples/prompts.jsonl --out out/openai
```

Add `--dry-run` to any `generate` command to check the plan without a single provider call.

## What you get

```text
out/
  rows.jsonl              every committed reply, one line per prompt
  rows.errors.jsonl       permanent failures: ids and classes, no text
  state.json              config hash and run state
  rejected.jsonl          gate rejections: ids and reasons
  gate.json               gate receipt
  data/train.jsonl        ┐
  data/validation.jsonl   │ uploaded by push
  README.md               │ dataset card
  manifest.json           ┘ lineage and counts
  publish.json            push receipt
```

One row of `data/train.jsonl`:

<!-- DEMO_ROW -->
```json
{
  "schema_version": "synthlite.sft.v1",
  "source_task_id": "task_4c7e57a10c5a345e29965317492726246e541d0f44238b74d585c3c77c942c9d",
  "variant_index": 0,
  "generator_config_hash": "gen_48ca1520349e09dc2f26f6797206185f1aac4acabf61806899ad03777c49e172",
  "messages": [
    {
      "role": "user",
      "content": "On-call chat, #inc-checkout, 02:14 local time.\n\nPriya (SRE): Checkout API p95 latency at ExampleMart jumped from 180 ms to 2.4 s at 01:52. Error rate is 3.1%, m …"
    },
    {
      "role": "assistant",
      "content": "> **SEV-2 — Checkout API degraded.** p95 latency 13× (180ms→2.4s), 3.1% HTTP 504s from edge LB. No deploy, DB CPU normal. **Key signal: app-03 (10.20.4.13) hold …"
    }
  ],
  "metadata": {
    "input_id": "triage-001",
    "input_metadata": {
      "topic": "incident-triage"
    },
    "provider": "openrouter",
    "base_url": "https://openrouter.ai/api/v1",
    "model": "openrouter/free",
    "served_model": "inclusionai/ling-3.0-flash-fin:free",
    "generated_at": "2026-09-27T20:03:41Z",
    "max_output_tokens": 32768,
    "usage": {
      "completion_tokens": 1500,
      "prompt_tokens": 222
    }
  }
}
```

A real row from the demo dataset, with the prompt and the reply shortened here. `model` is what you asked for; `served_model` is the free model that answered.

Browse the full demo datasets, made with the commands above for $0 on OpenRouter's free router: [IT-operations SFT](https://huggingface.co/datasets/ScogoAI/synthlite-demo-itops-sft) (25 prompts, 9 free models) and [decision traces with the Sia persona](https://huggingface.co/datasets/ScogoAI/synthlite-demo-itops-decision-traces).
<!-- /DEMO_ROW -->

`messages` is the column TRL, Axolotl, Unsloth, and `datasets` load. `metadata` keeps lineage: your `id` and `metadata`, the requested model, the model that answered (`served_model`), token usage, and the time.

## Why it's different

| | synthlite | A for-loop over the OpenAI SDK | Generate-then-judge pipelines |
|---|---|---|---|
| Calls per prompt | One. Retries only for timeouts and 5xx | One, plus the retries you write | Two or more: generate, then judge |
| Resume after a crash | Run the same command. Committed rows are skipped | You write it | Depends on the pipeline |
| Duplicate rows | One row per prompt id. The gate drops exact duplicate prompts and replies | Easy to get on a rerun | Depends on the pipeline |
| Spend caps | `--max-requests`, `--max-rows`, `--dry-run` | You write them | Usually per stage |
| Runtime needed | One static binary | Python and the SDK | Python stack and a judge model |
| Output format | `messages` JSONL, dataset card, manifest, private Hub repo | Whatever you write | Varies |

What you give up: nobody grades the answers. See [docs/design.md](docs/design.md).

## Decision traces

`--detailed` turns each reply into typed reasoning steps plus a final answer, from the same single call:

```text
## Decision trace
1. Evidence: The prompt reports stale BGP paths after Border Router B restarted.
2. Hypothesis: Graceful restart did not finish.
3. Action: Request "show bgp neighbor detail" from Border Router B. Do not reset the session yet.
4. Verification: Confirm end-of-RIB arrived and the stale paths cleared.
5. Conclusion: The session is up but recovery is incomplete.

## Final answer
Treat this as incomplete graceful-restart recovery and verify end-of-RIB before any reset.
```

Name your assistant with `[generation].persona`. Scogo AI uses `persona = "Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations"`; replace it with yours. The contract is tuned for IT operations. See [docs/decision-traces.md](docs/decision-traces.md) and [examples/configs/detailed-sia.toml](examples/configs/detailed-sia.toml).

## Design in one screen

- **One call per seed.** One prompt, one teacher call, one row.
- **Seeds in, dataset out.** You bring the prompts. synthlite does not write or review them.
- **Deterministic gate, no judge.** Fixed rules, same output every time. Correctness is not checked.
- **The committed row is the checkpoint.** A crash loses at most the rows in flight.
- **Content-addressed identity.** A prompt's id is a hash of its text. Add prompts and only the new ones run.
- **One static binary.** No runtime, no database, no plugins.
- **Any OpenAI-compatible endpoint.** Hosted or local. Several keys can pool.
- **Private and quiet by default.** Private repos only. Keys, prompts, and replies never reach stderr.
- **Training-shaped output.** `messages` rows that trainers load directly.

Each principle and its cost: [docs/design.md](docs/design.md).

## Providers

| Provider | How |
|---|---|
| OpenAI | `OPENAI_API_KEY` and `OPENAI_MODEL`, or [openai.toml](examples/configs/openai.toml) |
| OpenRouter free router | [openrouter-free.toml](examples/configs/openrouter-free.toml) |
| vLLM | [vllm.toml](examples/configs/vllm.toml) |
| Ollama | [ollama.toml](examples/configs/ollama.toml) |

Groq, Together, LiteLLM, and any other OpenAI-compatible API work too. See [docs/configuration.md](docs/configuration.md).

## Docs

| Page | Read it when |
|---|---|
| [docs/input.md](docs/input.md) | You want to bring your own prompts: `.txt`, JSONL records, ids, duplicates |
| [docs/configuration.md](docs/configuration.md) | You need env vars, `synthlite.toml`, or a provider recipe |
| [docs/running.md](docs/running.md) | You are watching a run, resuming it, or fixing an error |
| [docs/decision-traces.md](docs/decision-traces.md) | You want `--detailed` rows or your own persona |
| [docs/design.md](docs/design.md) | You want to know why it works this way |
| [docs/spec.md](docs/spec.md) | You need the full contract: hashes, refusals, exit codes, tests |

Every subcommand lists its flags: `synthlite generate --help`, `synthlite gate --help`, `synthlite push --help`.

## FAQ

**Do I need Taskgen?**
No. A `.txt` file or JSONL prompt records are enough. Taskgen files are also accepted.

**Is the output verified?**
No. Rows are unjudged teacher output. The gate removes cheap defects only. Filter and evaluate the data before you train on it.

**Can I publish the dataset publicly?**
`push` only creates and writes private repos. Change the visibility on Hugging Face yourself when you are ready.

**Which license applies to the generated data?**
The teacher provider's terms decide what you may do with its outputs. Check them. Set the card license with `[card].license`.

**Does it train models?**
No. It builds the dataset. Use TRL, Axolotl, Unsloth, or your own trainer.

## Contributing, security, license

- Contributing: [CONTRIBUTING.md](CONTRIBUTING.md). Changes: [CHANGELOG.md](CHANGELOG.md).
- Security reports: [SECURITY.md](SECURITY.md). Please do not open public issues for vulnerabilities.
- License: [Apache-2.0](LICENSE).

## Built by Scogo AI

synthlite is built by [Scogo AI](https://scogo.ai). We use it to build training data for Sia, which delivers Autonomous Agentic IT Operations. That is why the examples are about IT operations. Change the prompts and the persona to fit your domain.

Taskgen, Scogo's seed generator, will be open-sourced too.
