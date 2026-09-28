# Bring your own prompts

synthlite reads one input file. Each prompt in it becomes one row in your dataset. There are three ways to write that file.

## Plain text

Use a file whose name ends in `.txt` (any case). Each line is one prompt.

```text
What does BGP state Active mean on a Cisco router?
How do I read the output of "show ip ospf neighbor"?

Which DHCP options does a Cisco IP phone need?
```

- Each line is trimmed.
- Blank lines are skipped.
- A prompt cannot span several lines. Use JSONL for multi-line prompts.

```bash
synthlite examples/prompts.txt --out out/questions
```

## Prompt records (JSONL)

Any file that does not end in `.txt` is read as JSONL: one JSON object per line.

```json
{"id": "dns-001", "prompt": "Users at the Pune office cannot resolve intranet.example.com ...", "metadata": {"topic": "dns", "team": "netops"}}
```

| Key | Required | What it does |
|---|---|---|
| `prompt` | yes | The user message. A non-empty string of at most 20000 characters. Use `\n` for line breaks. |
| `id` | no | Your own label. Copied to the row as `metadata.input_id`. |
| `metadata` | no | Any JSON object. Copied to the row as `metadata.input_metadata`. |

Any other key stops the run before the first request:

```text
prompts.jsonl:7: unknown field topic (put extra fields under "metadata")
```

Move the key inside `metadata` and run again.

See [examples/prompts.jsonl](../examples/prompts.jsonl) for 25 records. Every company, host, and address in them is fictional.

## Taskgen records (optional)

Taskgen is Scogo's seed generator. It will be open-sourced. You do not need it. synthlite still reads its two files:

- `tasks.jsonl`: `scogo.taskgen.task.v2` records. Validation is strict.
- `candidates.jsonl`: `scogo.taskgen.candidate.v1` wrappers. Each is unwrapped to its task. Candidates that failed Taskgen's checks are skipped.

synthlite tells the kinds apart line by line with the `schema_version` key:

| `schema_version` | Line is read as |
|---|---|
| absent | a prompt record |
| `scogo.taskgen.task.v2` | a Taskgen task |
| `scogo.taskgen.candidate.v1` | a Taskgen candidate |
| anything else | an error: `unsupported schema_version <v>` |

One file may mix kinds. The full Taskgen format is in [spec.md, section 7](spec.md#7-input-contract). A sample line is [examples/tasks.sample.jsonl](../examples/tasks.sample.jsonl).

## Convert what you already have

A text file, one prompt per line, with ids from the line number:

```bash
jq -Rc 'gsub("^\\s+|\\s+$"; "") | select(length > 0) | {id: "q\(input_line_number)", prompt: .}' questions.txt > prompts.jsonl
```

One column of a CSV file (here `question`, with `ticket_id` as the id):

```bash
python3 -c 'import csv,json,sys; [print(json.dumps({"id": r["ticket_id"], "prompt": r["question"].strip()}, ensure_ascii=False)) for r in csv.DictReader(sys.stdin) if r["question"].strip()]' < tickets.csv > prompts.jsonl
```

Check the file before you spend anything. `--dry-run` reads every line and makes no provider call:

```bash
synthlite prompts.jsonl --dry-run
```

## Duplicates

The same prompt twice stops the run:

```text
prompts.jsonl:12: duplicate prompt (same prompt as line 3)
```

- A different `id` or `metadata` does not make a prompt unique.
- Prompts that differ only in case or spacing are allowed. The gate keeps only the first of them.

## How ids are derived

Every prompt gets a `source_task_id`. It is `task_` plus the SHA-256 of this JSON, written compactly with sorted keys:

```json
{"prompt":"What does BGP state Active mean?","schema_version":"synthlite.prompt.v1"}
```

That prompt gets `task_094d3488161cd3c3939ac0e45f81d0997c86a133196afed9f3775fa89f767f1d`. You can compute an id yourself:

```bash
python3 -c 'import hashlib,json,sys; print("task_" + hashlib.sha256(json.dumps({"prompt": sys.argv[1], "schema_version": "synthlite.prompt.v1"}, separators=(",", ":"), ensure_ascii=False).encode()).hexdigest())' "What does BGP state Active mean?"
```

What this means for you:

- `id` and `metadata` are not part of the id. You can rename them without regenerating anything.
- A text line and a prompt record with the same prompt get the same id.
- Editing a prompt gives it a new id. On the next run the old row is reported as dropped and the new prompt is generated.
- Resume works on ids. Add prompts to the file and run the same command: only the new ones are generated.
- Taskgen records hash a different object, described in [spec.md, section 8](spec.md#8-identity-and-work-items).
