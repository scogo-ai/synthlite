# Configuration

synthlite needs no config file. Environment variables are enough for one OpenAI-compatible endpoint. Use a TOML file for anything more.

## Precedence

**flags > environment > config file > built-in defaults**

- The config file is `./synthlite.toml` when it exists, or the path given with `--config PATH`.
- `$HOME` and parent directories are never searched.
- Unknown tables or fields stop the run with exit 2, so typos fail fast.

## Environment variables

| Variable | Used by | Meaning |
|---|---|---|
| `OPENAI_API_KEY` | generate | API key. Required unless the config file has a `[[key]]` table |
| `OPENAI_MODEL` | generate | Model id. Required unless `[[key]]` is used. There is no default |
| `SYNTHLITE_MODEL` | generate | Overrides `OPENAI_MODEL` |
| `OPENAI_BASE_URL` | generate | Default `https://api.openai.com/v1`. Must be `http(s)://host[:port][/path]` with no query or fragment |
| `SYNTHLITE_SYSTEM_PROMPT` | generate | A system message for every row. Overrides `[generation].system_message`. Not allowed with `--detailed` |
| `HF_TOKEN` | push | Hugging Face token with write access. `HUGGING_FACE_HUB_TOKEN` is the fallback |
| `HF_ENDPOINT` | push | Hub URL. Default `https://huggingface.co` |
| any name in `api_key_env` | generate | The secret for that `[[key]]` table |

`generate` never reads `HF_TOKEN`. `push` never reads provider keys.

Keep secrets in a `.env` file that you never commit. [examples/env.example](../examples/env.example) is a template:

```bash
cp examples/env.example .env      # fill it in
set -a; . ./.env; set +a
```

## The config file

It has four tables. All are optional. [examples/synthlite.toml](../examples/synthlite.toml) shows every field with comments.

| Table | Purpose | Changing it needs a new `--out`? |
|---|---|---|
| `[generation]` | What is sent to the model | Yes. It is hashed into `generator_config_hash` |
| `[[key]]` | Where requests go, and how fast | No |
| `[gate]` | Filters and the validation split | No |
| `[card]` | Dataset card metadata | No |

### [generation]

| Field | Default | Meaning |
|---|---|---|
| `temperature` | not sent | Number >= 0 |
| `top_p` | not sent | Number >= 0 |
| `seed` | not sent | Integer |
| `frequency_penalty` | not sent | Number >= 0 |
| `presence_penalty` | not sent | Number >= 0 |
| `stop` | not sent | Array of strings |
| `reasoning_effort` | not sent | String for reasoning models, such as `"medium"` |
| `max_output_tokens` | 32768, stepped down | Unset: 32768 is sent, then 16384, 8192, or 4096 if the provider answers HTTP 400. Set: sent as-is, never stepped down |
| `system_message` | none | A system message for every row. Not allowed with `detailed` |
| `detailed` | `false` | `true` makes every reply a [decision trace](decision-traces.md). Same as `--detailed` |
| `persona` | none | Names the assistant in detailed rows. Requires `detailed` |

### [[key]]

Each `[[key]]` table is one endpoint and credential. Any `[[key]]` table replaces the environment key, so `OPENAI_*` variables are then ignored. Several tables pool: each prompt still gets exactly one row.

| Field | Default | Meaning |
|---|---|---|
| `provider` | required | A label for the row metadata, such as `"openai"` or `"vllm"` |
| `base_url` | required | The OpenAI-compatible base URL, ending before `/chat/completions` |
| `model` | required | The model id to request |
| `api_key_env` | required | The **name** of the environment variable that holds the key. Never the key itself |
| `max_concurrent` | 16 | Ceiling for requests in flight. A 429 halves the live limit; clean answers grow it back |
| `requests_per_minute` | none | Client-side cap per key |
| `timeout_seconds` | 600 | Idle timeout: the longest wait for headers or the next streamed chunk. A reply that keeps streaming is never cut |
| `max_attempts` | 5 | Tries per prompt for timeouts, dropped streams, 408, 425, and 5xx |
| `rate_limit_cooldown_seconds` | 60 | Pause after a 429 without `Retry-After` |
| `headerless_429_limit` | 3 | Consecutive 429s without `Retry-After` before the key is parked |
| `max_tokens_field` | by host | `max_completion_tokens` for `api.openai.com`, `max_tokens` elsewhere |
| `require_model_match` | `true` | The reply's `model` must match the request (dated snapshots allowed). Set `false` for routers |
| `require_finish_reason` | `true` | A reply without `finish_reason` is a failure. Set `false` for servers that omit it |

### [gate]

| Field | Default | Meaning |
|---|---|---|
| `min_assistant_chars` | 200 | Shorter replies are rejected as `too_short` |
| `max_assistant_chars` | 40000 | Longer replies are rejected as `too_long` |
| `refusal_window_chars` | 300 | How much of the reply start is checked for refusal phrases |
| `refusal_phrases` | 7 phrases | Case-insensitive, such as `"as an ai language model"`. Setting it replaces the list |
| `max_repeated_line` | 4 | A trimmed line of 20+ characters repeated more often is rejected as `repetition` |
| `exclude_prompt_hashes` | none | Path to a file of normalized-prompt SHA-256 hashes to hold out, one per line |
| `validation_per_mille` | 30 | Rows per 1000 that go to `validation`, chosen by id. `0` disables the split |

### [card]

| Field | Default | Meaning |
|---|---|---|
| `pretty_name` | the repo name | Dataset title |
| `license` | omitted | A Hugging Face license id, such as `"apache-2.0"` or `"other"` |
| `license_name`, `license_link` | omitted | Written only when `license = "other"` |
| `language` | `["en"]` | Language codes |

Pick the license with care. The teacher provider's terms may limit how you use its outputs.

## Provider recipes

Each recipe is a complete file in [examples/configs/](../examples/configs/). Run one with `--config`:

```bash
synthlite examples/prompts.jsonl --config examples/configs/openai.toml --out out/openai
```

### OpenAI

No file needed:

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=gpt-4.1-mini
synthlite examples/prompts.jsonl
```

### OpenRouter free router

[examples/configs/openrouter-free.toml](../examples/configs/openrouter-free.toml) costs $0 per token:

```toml
[[key]]
provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = "openrouter/free"
api_key_env = "OPENROUTER_API_KEY"
require_model_match = false
requests_per_minute = 20
max_concurrent = 4
```

- `openrouter/free` is a router. Each request may go to a different free model.
- `require_model_match = false` is required. The router answers with the served model's id, not `openrouter/free`.
- Every row records both: `metadata.model` is `openrouter/free`, and `metadata.served_model` is the model that answered.
- `requests_per_minute = 20` matches OpenRouter's free-tier limit. OpenRouter also caps free requests per day. Check their current limits.
- A key that hits the daily cap gets repeated 429s and is parked. Wait, then run again with `--resume-parked-keys`.
- Free models vary in quality. Read your rows before you train on them.

### vLLM

Start a server, for example `vllm serve Qwen/Qwen2.5-7B-Instruct`. Then use [examples/configs/vllm.toml](../examples/configs/vllm.toml):

```toml
[[key]]
provider = "vllm"
base_url = "http://localhost:8000/v1"
model = "Qwen/Qwen2.5-7B-Instruct"
api_key_env = "VLLM_API_KEY"   # any non-empty value if the server has no --api-key
max_concurrent = 8
```

Tuning a self-hosted server:

- Set `max_concurrent` near the number of sequences the server batches well. Going above it can make each request slower.
- The default output cap is 32768. If prompt plus cap exceeds the server's context length, the server answers HTTP 400 and synthlite steps down once per run.
- Replies that often stop at the cap show up as `truncated`. Raise the server's context length and set `[generation].max_output_tokens` (use a new `--out`).
- `timeout_seconds` is an idle timeout. A healthy stream sends chunks many times per second, so 300 only fires on a real stall.
- List the models a server exposes with `curl -s http://localhost:8000/v1/models`.

### Ollama

Pull a model (`ollama pull qwen2.5:7b`), then use [examples/configs/ollama.toml](../examples/configs/ollama.toml):

```toml
[[key]]
provider = "ollama"
base_url = "http://localhost:11434/v1"
model = "qwen2.5:7b"
api_key_env = "OLLAMA_API_KEY"   # Ollama ignores it; export OLLAMA_API_KEY=ollama
max_concurrent = 2
```

Ollama's default context window is small. Long prompts or replies then end as `truncated`. Raise it on the server, for example with `OLLAMA_CONTEXT_LENGTH`.

### Groq, Together, and other hosted APIs

Any OpenAI-compatible endpoint works the same way. Set `requests_per_minute` to your plan's limit. For Together, use `base_url = "https://api.together.xyz/v1"` and one of its model ids.

```toml
[[key]]
provider = "groq"
base_url = "https://api.groq.com/openai/v1"
model = "llama-3.3-70b-versatile"
api_key_env = "GROQ_API_KEY"
requests_per_minute = 30
```

## Check a config without spending

`synthlite examples/prompts.jsonl --config my.toml --dry-run` loads the config and reads the input. It prints the plan and makes no provider call.
