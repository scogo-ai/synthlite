# synthlite

**Prompts in, a private fine-tuning dataset out.**

A single static Rust binary · one teacher call per prompt · no LLM judge · crash-safe resume · any OpenAI-compatible endpoint

[![CI](https://github.com/scogo-ai/synthlite/actions/workflows/ci.yml/badge.svg)](https://github.com/scogo-ai/synthlite/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/synthlite?logo=rust&color=e43717)](https://crates.io/crates/synthlite)
[![Release](https://img.shields.io/github/v/release/scogo-ai/synthlite?logo=github)](https://github.com/scogo-ai/synthlite/releases/latest)
[![Rust 1.89+](https://img.shields.io/badge/rust-1.89%2B-f74c00?logo=rust)](https://www.rust-lang.org)
[![Platforms](https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-4c1)](https://github.com/scogo-ai/synthlite/releases/latest)
[![Demo datasets](https://img.shields.io/badge/%F0%9F%A4%97%20demo%20datasets-Hugging%20Face-ffcc4d)](https://huggingface.co/datasets/ScogoAI/synthlite-demo-itops-sft)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

![synthlite generating 10 rows on OpenRouter's free router, interrupted and resumed with no duplicates, then gated and pushed to a private dataset](docs/assets/synthlite-demo.gif)

<sub>Ten prompts on OpenRouter's free router: interrupt, resume without duplicates, gate, private push. Waits for replies are cut; the timestamps show real time.</sub>

## What it does

synthlite is a command-line tool written in Rust. You give it a file of prompts. It asks a teacher model for one reply per prompt, filters the replies with fixed rules, and uploads a private Hugging Face dataset.

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
- **crates.io** (Rust 1.89+): `cargo install synthlite --locked`
- **Latest source** (Rust 1.89+): `cargo install --locked --git https://github.com/scogo-ai/synthlite`

## Quickstart

Try it with OpenRouter's free router, so the teacher costs nothing. Get a key at [openrouter.ai](https://openrouter.ai/) and a Hugging Face token with write access. For a dataset you will train on, pick a stronger teacher: see [Choosing a teacher](#choosing-a-teacher).

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

## Choosing a teacher

synthlite has no LLM-as-judge stage. Every row is what the teacher wrote, filtered only by the gate's rules, so the teacher sets the quality ceiling.

| You want to | Use | Why |
|---|---|---|
| Try synthlite, test prompts and configs | OpenRouter's free router ([openrouter-free.toml](examples/configs/openrouter-free.toml)) or a local model on vLLM or Ollama | Costs nothing. Quality varies by request, and each free model has its own terms |
| Build a dataset you will train on | A frontier model such as Claude Opus 5.5 or GPT-6-Sol ([openrouter-frontier.toml](examples/configs/openrouter-frontier.toml)) | No judge checks the answers afterwards, so the strongest teacher gives the best odds of correct, complete rows |

```toml
[[key]]
provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = "anthropic/claude-opus-5.5"  # or "openai/gpt-6-sol"
api_key_env = "OPENROUTER_API_KEY"
```

Run a canary first: add `--max-rows 20`, read `out/rows.jsonl`, then run the same command without it to finish. `--max-requests` caps spend.

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

Browse the full demo datasets, made with the quickstart commands on OpenRouter's free router: [IT-operations SFT](https://huggingface.co/datasets/ScogoAI/synthlite-demo-itops-sft) (25 prompts, 9 free models) and [decision traces with the Sia persona](https://huggingface.co/datasets/ScogoAI/synthlite-demo-itops-decision-traces).
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

## Design philosophy

synthlite makes one teacher call per prompt and keeps everything around that call boring and reliable. Each choice trades breadth for speed, cost, and a result you can reproduce.

| Principle | What it means | What you give up |
|---|---|---|
| One call per prompt | Each prompt gets exactly one teacher call and one row. Retries only for timeouts, 5xx, and rate limits | Best-of-K selection. Quality rides on the teacher and the prompt |
| Prompts in, dataset out | You bring the prompts. synthlite never writes, paraphrases, or evolves them | Built-in prompt synthesis |
| Deterministic gate, no judge | Rows are filtered by explainable rules: exact dedup, length, refusal phrases, repeated lines, held-out prompts. Same input, same output | Correctness checks. The dataset card says replies are unverified |
| The committed row is the checkpoint | One writer appends and fsyncs each row. Kill it anytime and rerun the same command to resume | Distributed workers. One process per output folder |
| Content-addressed identity | A prompt's id is a hash of the prompt, and the generation settings are hashed too. One output folder holds one config | Changing a generation setting needs a new output folder |
| One static Rust binary | About 7 MB. No Python environment, no services, no database, no GPU | A plugin system or Python API |
| Any OpenAI-compatible endpoint | Hosted APIs or local servers, several keys pooled, adaptive concurrency, hard spend caps | Provider-native and batch APIs |
| Private and quiet by default | Pushes only to private Hugging Face repos. Keys, prompts, and replies never reach logs | One-command public releases |
| Training-shaped output | Chat `messages` rows that TRL, Axolotl, Unsloth, and `datasets` load as-is. `--detailed` adds decision traces | DPO pairs, multi-turn data, other formats |

Each principle in more depth: [docs/design.md](docs/design.md).

## Where it fits

Most open-source synthetic-data tools are Python frameworks or apps that also write the prompts, run multi-step pipelines, or score the data with a model. synthlite does one narrow job: you already have prompts, and you want one good answer to each, as a clean dataset, from a single binary.

| Tool | What it is | Runtime | Judge or scoring | How synthlite differs |
|---|---|---|---|---|
| **synthlite** | CLI that turns prompts you already have into an SFT dataset, one call per prompt | One static Rust binary | None. Fixed rules: dedup, length, refusals, repeated lines | |
| [distilabel](https://github.com/argilla-io/distilabel) | Framework for synthetic data and AI feedback, built from research-paper tasks | Python | Built-in judge tasks such as UltraFeedback, added as pipeline steps | No pipelines, prompt evolution, or AI feedback. Pick distilabel for preference (DPO) data |
| [Curator](https://github.com/bespokelabsai/curator) | SDK for bulk inference and data curation | Python | None built in; you can add one as another LLM call | Closest in spirit. Curator gives structured outputs, batch APIs, and code control; synthlite needs no code |
| [NeMo Data Designer](https://github.com/NVIDIA-NeMo/DataDesigner) | Builds datasets column by column from samplers, LLM columns, and optional seed data | Python library and CLI | Optional LLM-judge columns and validators | synthlite doesn't design or sample fields. Pick Data Designer when you don't have prompts yet |
| [synthetic-data-kit](https://github.com/meta-llama/synthetic-data-kit) | CLI that turns documents into Q&A and chain-of-thought data | Python CLI | Optional `curate` step: an LLM rates each pair from 1 to 10 | synthlite doesn't parse documents or write questions. Pick it when your source is PDFs or web pages |
| [Kiln](https://github.com/Kiln-AI/Kiln) | App and library for building AI products: evals, synthetic data, fine-tuning | Desktop app and Python library | LLM-judge evals, run separately; people review synthetic data in the app | synthlite is headless and only generates. Pick Kiln for a GUI with evals and fine-tuning in one place |
| [Easy Dataset](https://github.com/ConardLi/easy-dataset) | App that turns documents into fine-tuning, RAG, and eval datasets | JavaScript desktop app, npm, or Docker | Optional AI quality scoring that you trigger | No GUI or document chunking in synthlite. Pick Easy Dataset for document-to-Q&A with visual review |
| [DataDreamer](https://github.com/datadreamer-dev/DataDreamer) | Library for prompting, synthetic data, and training workflows | Python | Optional filter, rank, and judge steps | synthlite is not a workflow or training library. Pick DataDreamer for reproducible research pipelines |
| [DeepFabric](https://github.com/nolabs-ai/deepfabric) | Reasoning and tool-calling data from topic trees, for agents | Python; tools run in a sandbox | Schema validation; no LLM judge documented | synthlite writes no topics or tool traces. Pick DeepFabric for agent data |
| [Oumi](https://github.com/oumi-ai/oumi) | Platform for the whole model lifecycle; `oumi synth` is one feature | Python | A separate `oumi judge` command | synthlite is one generation step. Pick Oumi for synthesis, judging, and training in one stack |
| [DataFlow](https://github.com/OpenDCAI/DataFlow) | Operator pipelines that generate, refine, evaluate, and filter data | Python, Docker, web UI | Evaluator and filter operators, including LLM judges | synthlite's gate is fixed rules. Pick DataFlow for multi-stage cleaning and scoring |

**Choose synthlite** when you already have prompts, want a dataset by morning, and want it to run in CI, cron, or a locked-down box without Python or a GPU. **Choose one of the others** when you need prompt synthesis, documents as input, multi-turn or preference data, or model-scored filtering.

<sub>Checked against each project's README and docs in September 2026. If a row is out of date, please open an issue or a pull request.</sub>

## Providers

| Provider | How |
|---|---|
| OpenAI | `OPENAI_API_KEY` and `OPENAI_MODEL`, or [openai.toml](examples/configs/openai.toml) |
| OpenRouter free router | [openrouter-free.toml](examples/configs/openrouter-free.toml) |
| A frontier model on OpenRouter | [openrouter-frontier.toml](examples/configs/openrouter-frontier.toml) |
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

We plan to open-source Taskgen, Scogo's seed generator, next.
