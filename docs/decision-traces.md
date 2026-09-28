# Decision traces (--detailed)

By default each row holds the teacher's free-text reply. With `--detailed`, each reply is a decision trace: numbered, typed reasoning steps, then a final answer. It is still one provider call per prompt.

```bash
synthlite examples/prompts.jsonl --detailed --out out/detailed
```

`detailed = true` in `[generation]` does the same.

## What a row looks like

The assistant message of every detailed row has this shape:

```text
## Decision trace
1. Evidence: The prompt reports stale BGP paths after Border Router B restarted.
2. Hypothesis: Graceful restart did not finish, so the peer still holds stale routes.
3. Action: Request "show bgp neighbor detail" from Border Router B. Do not reset the session yet.
4. Verification: Confirm the end-of-RIB marker arrived and the stale paths cleared.
5. Conclusion: The session is up but recovery is incomplete.

## Final answer
Treat this as incomplete graceful-restart recovery. Verify end-of-RIB before any session reset.
```

- The five step kinds are Evidence, Hypothesis, Action, Verification, and Conclusion.
- Every kind appears at least once. A kind may repeat. There are at most 24 steps.
- To split a row, cut at the first `\n\n## Final answer\n`.
- Rows keep the `synthlite.sft.v1` schema and the same `metadata` fields as plain rows.

## The teacher contract

synthlite adds an instruction to the system message it sends. The teacher reads it; the row never contains it. In plain words, it asks the teacher to:

- reply with one JSON object: a list of typed steps and a final answer;
- use only the facts in the prompt as observations, and label general knowledge as such;
- never invent command output, logs, device names, or addresses;
- propose checks as actions, with what each result would mean, but never claim a result;
- prefer read-only checks, and give staged steps with rollback for changes;
- write a final answer that stands on its own, in the format the prompt asks for.

synthlite then checks the structure and renders it as the text above. Nothing is executed. No model judges the answer. The exact text is in [spec.md, section 14](spec.md#request).

## When a trace is rejected

A reply that is not one valid trace object fails with class `invalid_trace`. Common causes:

- a memo or prose instead of JSON;
- extra text after the JSON object;
- a missing step kind;
- more than 24 steps.

`invalid_trace` is final on the first try. It is not retried, because a teacher that ignores the contract usually ignores it every time. Any `invalid_trace` makes `generate` exit 4, and `gate` refuses until those prompts succeed. To spend on them again:

```bash
synthlite examples/prompts.jsonl --detailed --out out/detailed --retry-failed
```

If many prompts fail, try a stronger teacher on a new `--out`.

## The system message and persona

Detailed rows always start with a fixed system message. By default it is:

```text
You are a careful IT operations assistant. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.
```

`[generation].persona` replaces "a careful IT operations assistant". Scogo AI names its assistant Sia:

```toml
[generation]
detailed = true
persona = "Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations"
```

Every row then starts with:

```text
You are Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.
```

- The persona is one line. A trailing period is dropped.
- The teacher is told not to introduce itself, so answers stay technical. The name lives only in the system message.
- The persona is part of the hash. Changing it needs a new `--out`.
- `SYNTHLITE_SYSTEM_PROMPT` and `[generation].system_message` are refused with `--detailed`.

[examples/configs/detailed-sia.toml](../examples/configs/detailed-sia.toml) combines the Sia persona with OpenRouter's free router.

## Tuned for IT operations

The step kinds, the fixed guidance sentence, and the teacher contract are written for IT operations: incidents, changes, and runbooks. To use your own assistant name, set `persona` to it, for example `persona = "Atlas, the on-call assistant at Example Corp"`.

The persona changes the name, not the domain. For another domain, run a canary first and read the traces. To change the contract itself, edit `TEACHER_INSTRUCTION` in `src/trace.rs` and build your own binary. The new text changes the hash, so old and new rows never mix.

## Run a canary first

Try 20 prompts before a full run. Use a 20-prompt file, not `--max-rows`: a capped run exits 3 with work pending, and `gate` runs only on a finished run.

```bash
head -n 20 prompts.jsonl > canary.jsonl
synthlite canary.jsonl --detailed --out out/detailed-canary 2>&1 | tee detailed-canary.log
grep -c 'class=invalid_trace' detailed-canary.log                  # prompts that broke the contract
jq -r '.messages[-1].content' out/detailed-canary/rows.jsonl | less   # read the traces
synthlite gate --out out/detailed-canary                             # only after generate exited 0
```

Proceed when `invalid_trace` is under 5% of the prompts and the steps are not padded, for example `Hypothesis: none`.

Small free models may produce more `invalid_trace` rows than frontier teachers. The canary shows you before you spend on the full file.

## Gate and card

- `gate` and `push` take no flag. They read the mode from `out/state.json`.
- In a detailed run, the refusal check also covers the first 300 characters of the final answer.
- The dataset card says the run used `--detailed`.
