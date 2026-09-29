# Running and troubleshooting

This page covers what a run prints, how to resume it, how to cap spend, and what to do when something fails.

## What a run prints

Everything goes to stderr, one `key=value` line per event. It reads the same in a terminal, under `nohup`, or in a log file. Prompts, replies, and keys are never printed.

```text
model=openrouter/free base=openrouter.ai out=./out seeds=25 start pending=25 complete=0 failed=0 dropped=0 version=0.4.1
28-09-26:10:00:30 progress requests=4 committed=0/25 in_flight=4 queued=21 failed=0 retry=0 tok_s=? input_tokens=0 output_tokens=0 elapsed=30s finish_in=?
retry task_5b01d2e9a4c0 try=1/5 reason=timeout backoff=1s
28-09-26:10:05:00 progress requests=19 committed=14/25 in_flight=4 queued=7 failed=0 retry=1 tok_s=48.2 input_tokens=5210 output_tokens=14460 elapsed=5m00s finish_in=3m40s
done requests=26 committed=25 failed=0 pending=0 success=25 rate_limit=0 retry=1 budget_retry=0 input_tokens=9120 output_tokens=25980 usage_missing=0 elapsed=8m51s
```

The first line records the plan: model, host, out dir, prompt count, `start` or `resume`, and the synthlite version.

### Event lines

| Line | Meaning |
|---|---|
| `progress` | Heartbeat every 30 s. Change it with `--progress-interval N`; `0` turns it off |
| `retry` | A stall (`timeout`), a dropped stream (`stream_cut`), a connection error, a bad body, or HTTP 408, 425, or 5xx. The prompt is tried again after `backoff` |
| `budget <id>` | The provider rejected the output cap with HTTP 400. The prompt retries one step lower: 32768, 16384, 8192, 4096 |
| `budget <key>` | A lower cap worked. The key uses it for the rest of the run |
| `throttle` | The key got a 429 and halved its concurrency, or had enough clean answers to add a slot back |
| `failed` | The prompt is recorded in `rows.errors.jsonl` with this `class` |
| `parked` | A key stopped taking requests: `unauthorized` (401/403) or `headerless_429` (repeated 429 without `Retry-After`) |
| `done` | The summary. Adds `success` (rows committed in this run) and `pending` (prompts left) |

Ids are shortened to 12 hex digits. `grep task_5b01d2e9a4c0 out/rows*.jsonl` finds the record.

### A progress line, field by field

```text
28-09-26:10:05:00 progress requests=19 committed=14/25 in_flight=4 queued=7 failed=0 retry=1 tok_s=48.2 input_tokens=5210 output_tokens=14460 elapsed=5m00s finish_in=3m40s
```

| Field | Meaning | In this line |
|---|---|---|
| timestamp | Local machine time, `DD-MM-YY:HH:MM:SS` | 28 Sep 2026, 10:05 |
| `requests` | Provider calls in this run, including retries and budget steps. `--max-requests` counts these | 19 calls |
| `committed` | Rows saved in `rows.jsonl`, over the prompts in the input. Includes rows from earlier runs of the same `--out` | 14 of 25 done |
| `in_flight` | Requests the server is working on now | 4 streaming |
| `queued` | Prompts waiting for a slot, a backoff, or a 429 cooldown | 7 waiting |
| `failed` | Prompts recorded in `rows.errors.jsonl` in this run | none |
| `retry` | Calls that failed transiently and were tried again | 1 |
| `budget_retry` | Calls retried with a lower output cap. Shown only when nonzero | not shown |
| `rate_limit` | 429 answers. Shown only when nonzero | not shown |
| `tok_s` | Reported output tokens from finished calls, divided by elapsed seconds. An average, not a live rate. `?` until usage arrives | 48.2 |
| `input_tokens` | Provider-reported prompt tokens for finished calls, including failed and retried ones | 5,210 |
| `output_tokens` | Provider-reported completion tokens for finished calls, including reasoning | 14,460 |
| `elapsed` | Wall time since this run started | 5 minutes |
| `usage_missing` | Replies without usage data. Shown only when nonzero. Token totals are then lower bounds | not shown |
| `finish_in` | Rough time left, from finished reply sizes and current streaming. `?` until there is enough data | about 3m40s |

Token and request counters start over on each run. `committed` does not.

To keep a log:

```bash
synthlite prompts.jsonl --out out/itops 2>&1 | tee out-itops.log
```

## Resume and retry

The out dir is the checkpoint. A row is committed only when its full line is on disk.

```bash
synthlite prompts.jsonl                        # resumes ./out automatically
synthlite prompts.jsonl --resume               # same, but refuses if ./out is not a matching run
synthlite prompts.jsonl --retry-failed         # spend again on prompts in rows.errors.jsonl
synthlite prompts.jsonl --resume-parked-keys   # un-park keys parked on 401/403 or repeated 429
```

- After Ctrl-C, a crash, or a cap, run the same command again. Committed rows are never regenerated.
- A torn last line after a crash is moved to `rows.partial`, and that prompt runs again.
- Failed prompts stay failed until `--retry-failed`. Each gets a fresh retry budget.
- Parked keys stay parked until `--resume-parked-keys`.
- A `--detailed` out dir resumes only with `--detailed` again, or with `detailed = true` in the config.

## Cap spend

```bash
synthlite prompts.jsonl --dry-run            # plan only: counts, model, start or resume; no calls
synthlite prompts.jsonl --max-requests 50    # stop after 50 provider calls
synthlite prompts.jsonl --max-rows 1000      # stop after 1000 new rows
```

A capped run exits 3 with work pending. Run it again, with or without a cap, to continue. There is no cap by default.

## Read failures and rejections

Error and rejection files hold ids and reasons, never prompt or reply text. Count them by class:

```bash
jq -r .error_class out/rows.errors.jsonl | sort | uniq -c    # why generate failed
jq -r '.reasons[]' out/rejected.jsonl | sort | uniq -c        # why gate dropped rows
```

Find the prompt behind an id in your input, then read a committed reply:

```bash
grep -n '"id": "bgp-001"' examples/prompts.jsonl
jq -r 'select(.metadata.input_id == "bgp-001") | .messages[-1].content' out/rows.jsonl
```

With a router, see which models answered:

```bash
jq -r .metadata.served_model out/rows.jsonl | sort | uniq -c
```

## One out dir per config

```bash
synthlite prompts.jsonl --out out/gpt-t02
synthlite prompts.jsonl --out out/local --config examples/configs/vllm.toml
synthlite gate --out out/local
synthlite push --out out/local --hf-repo your-org/itops-sft-local
```

`[generation]` and the persona are hashed into `generator_config_hash`, and every row records the hash it was written under. You can change the `[generation]` settings, the config file, the model, the endpoint, or the gate on the same out dir, and the run picks up where it stopped. This is how you finish a long run when the original model goes away:

```bash
synthlite tasks.jsonl --out runs/big --config old-model.toml            # 80 of 100 done, the rest failed
synthlite tasks.jsonl --out runs/big --config new-model.toml --retry-failed
# config_change resuming under a new [generation] config (max_output_tokens: 32768 -> 65536; ...); rows already written keep their own generator_config_hash
```

Synthlite prints one `config_change` line listing the fields that changed, moves the old config into `state.json` `config_history`, and keeps every committed row as it is. `gate` accepts rows from every recorded config, and the manifest lists them in `generator_config_history`. Each row records its own teacher, so a dataset can hold rows from several models. To keep one config per dataset, use a new `--out`.

The one refusal left is a `--detailed` switch: adding or dropping it changes the shape of every row, so the old out dir exits 2. Pass a new `--out` for that.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Done. For `generate`, every prompt is committed |
| 1 | Unexpected error: I/O, network, Hub status |
| 2 | Refused: bad arguments or config, bad input, hash mismatch, locked out dir, gate or push precondition, public repo |
| 3 | `generate` stopped with work pending: cap reached, all keys parked, or no key can admit |
| 4 | `generate` has nothing pending, but some prompts failed permanently |

## Troubleshooting

| You see | Do this |
|---|---|
| `set OPENAI_MODEL or SYNTHLITE_MODEL` | Export a model id. There is no default |
| `OPENAI_API_KEY is required`, or `<NAME> is required` | Export the variable named in the message |
| `unknown field <k> (put extra fields under "metadata")` | Move that key inside the record's `metadata` object |
| `unsupported schema_version <v>` | Remove `schema_version` from prompt records. Only the two Taskgen versions are accepted |
| `duplicate prompt (same prompt as line N)` | The same prompt appears twice. Remove one copy |
| `duplicate source_task_id <id>` | The same Taskgen task appears twice. Remove one copy |
| `config_change resuming under a new [generation] config (...)` | Not an error. You changed the config on a resume; earlier rows keep their old hash |
| `./out was generated with --detailed; rerun with --detailed or pass a new --out` | Add `--detailed` to resume it, or use a new `--out` for a plain run |
| `./out was generated without --detailed; ...` | Remove `--detailed` and `detailed = true` to resume it, or use a new `--out` |
| `--detailed uses a fixed system message; ...` | Unset `SYNTHLITE_SYSTEM_PROMPT` and `[generation].system_message`. Use `persona` to name the assistant |
| `[generation].persona requires --detailed` | Add `--detailed`, or remove `persona` |
| `another synthlite process is using ./out` | Another run holds the lock. Wait, or use a different `--out` |
| `N work items failed; rerun with --retry-failed` (exit 4) | Read `error_class` in `out/rows.errors.jsonl`, fix the cause, then use `--retry-failed` |
| `error_class: truncated` | The reply hit the output cap. Raise `[generation].max_output_tokens` if the model allows it (new `--out`), or check whether the model loops |
| `error_class: model_mismatch` | The server answered with another model id. Check `/v1/models`, or set `require_model_match = false` for a router |
| `error_class: invalid_trace` | See [decision-traces.md](decision-traces.md#when-a-trace-is-rejected) |
| `error_class: retries_exhausted` with `http_status: null` | Every try failed to connect or stalled. Check the server, or lower `max_concurrent` |
| `parked <key> unauthorized` | Bad API key. Fix it, then run with `--resume-parked-keys` |
| `parked <key> headerless_429` | Rate or daily limit. Wait, then run with `--resume-parked-keys` |
| `all keys are parked; pending work remains` (exit 3) | Same as above for every key |
| Many `retry ... reason=timeout` lines | The server stopped sending for `timeout_seconds`. It is overloaded or stuck. A long reply alone never times out |
| `generation_complete is false; ...` (gate or push) | Finish `generate` with exit 0 first. Pending or failed prompts block publishing |
| `<repo> is public; refusing to upload` | `push` only writes private repos. Use another repo name |
