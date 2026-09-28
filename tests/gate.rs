mod common;

use std::fs;
use std::time::Duration;

use serde_json::{json, Value};

use synthlite::gate::{self, GateOpts};

fn hash() -> String {
    "gen_test".into()
}

fn row(id: &str, user: &str, assistant: &str) -> String {
    let task = common::fixture_task();
    format!(
        "{}\n",
        serde_json::to_string(&json!({
            "schema_version": "synthlite.sft.v1",
            "source_task_id": id,
            "variant_index": 0,
            "generator_config_hash": hash(),
            "messages": [
                {"role": "user", "content": user},
                {"role": "assistant", "content": assistant}
            ],
            "metadata": {
                "category": task["category"],
                "domain": task["domain"],
                "subdomain": task["subdomain"],
                "difficulty": task["difficulty"],
                "coordinates": task["coordinates"],
                "language": null,
                "taskgen_model": task["taskgen_model"],
                "provider": "openai",
                "base_url": "https://api.openai.com/v1",
                "model": "teacher-test",
                "generated_at": "2026-09-22T09:14:03Z",
                "usage": {"prompt_tokens": 3, "completion_tokens": 4}
            }
        }))
        .unwrap()
    )
}

fn long(tag: &str) -> String {
    common::long_reply(tag)
}

fn write_run(dir: &std::path::Path, lines: &str, complete: bool) {
    write_run_with(
        dir,
        lines,
        complete,
        json!({
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
        }),
    );
}

fn write_run_with(dir: &std::path::Path, lines: &str, complete: bool, generator_config: Value) {
    let out = dir.join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(out.join("rows.jsonl"), lines).unwrap();
    fs::write(
        out.join("state.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": "synthlite.state.v1",
            "synthlite_version": "0.1.0",
            "generator_config_hash": hash(),
            "generator_config": generator_config,
            "source_population_sha256": "abc",
            "taskgen_run_id": null,
            "seed_count": 6,
            "created_at": "2026-09-22T00:00:00Z",
            "generation_complete": complete,
            "parked_keys": []
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn gate_rejects_short_refusal_and_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let validation_id = "task_0000000000000000000000000000000000000000000000000000000000000000";
    let train_id = "task_0000001e00000000000000000000000000000000000000000000000000000000";
    assert!(synthlite::gate::is_validation(validation_id, 30).unwrap());
    assert!(!synthlite::gate::is_validation(train_id, 30).unwrap());
    let leak = "UNIQUE_PROMPT_BODY_SHOULD_NOT_LEAK";
    let same_reply = long("same-reply");
    let lines = [
        row(
            validation_id,
            "kept validation prompt alpha",
            &long("alpha"),
        ),
        row(train_id, "kept train prompt beta", &long("beta")),
        row(
            "task_0000002a00000000000000000000000000000000000000000000000000000000",
            leak,
            "tiny",
        ),
        row(
            "task_0000002b00000000000000000000000000000000000000000000000000000000",
            "too short prompt",
            "short",
        ),
        row(
            "task_0000002c00000000000000000000000000000000000000000000000000000000",
            "refusal prompt",
            &format!("I'm sorry, but I can't help with that. {}", long("refusal")),
        ),
        row(
            "task_0000002d00000000000000000000000000000000000000000000000000000000",
            "Hello   World",
            &long("dup-prompt-a"),
        ),
        row(
            "task_0000002e00000000000000000000000000000000000000000000000000000000",
            "hello world",
            &long("dup-prompt-b"),
        ),
        row(
            "task_0000002f00000000000000000000000000000000000000000000000000000000",
            "first response prompt",
            &same_reply,
        ),
        row(
            "task_0000003000000000000000000000000000000000000000000000000000000000",
            "second response prompt",
            &same_reply,
        ),
    ]
    .concat();
    write_run(dir.path(), &lines, true);
    let out = dir.path().join("out");
    gate::run(GateOpts {
        out: out.clone(),
        config: None,
        pretty_name_fallback: "synthlite".into(),
    })
    .unwrap();
    gate::run(GateOpts {
        out: out.clone(),
        config: None,
        pretty_name_fallback: "synthlite".into(),
    })
    .unwrap();

    let rejected = fs::read_to_string(out.join("rejected.jsonl")).unwrap();
    assert!(rejected.contains("too_short"), "{rejected}");
    assert!(rejected.contains("refusal"), "{rejected}");
    assert!(rejected.contains("duplicate_prompt"), "{rejected}");
    assert!(rejected.contains("duplicate_response"), "{rejected}");
    assert!(!rejected.contains(leak), "{rejected}");
    assert!(!rejected.contains("I'm sorry"), "{rejected}");
    for line in rejected.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        assert!(value.get("prompt").is_none());
        assert!(value.get("messages").is_none());
        assert!(value.get("content").is_none());
    }

    let train = fs::read_to_string(out.join("data/train.jsonl")).unwrap();
    let validation = fs::read_to_string(out.join("data/validation.jsonl")).unwrap();
    assert!(validation.contains(validation_id), "{validation}");
    assert!(train.contains(train_id), "{train}");
    assert!(!train.contains(validation_id));
    assert!(!validation.contains(train_id));

    let card = fs::read_to_string(out.join("README.md")).unwrap();
    assert!(card.contains("pretty_name:"));
    assert!(!card.contains("num_examples"));
    assert!(!card.contains("license:"));
    assert!(
        card.contains("unverified") || card.contains("Unverified") || card.contains("not judged")
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert!(
        manifest["counts"]["rejected"]["too_short"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(manifest["split"]["validation_per_mille"], 30);

    let first_train = fs::read(out.join("data/train.jsonl")).unwrap();
    let first_rejected = fs::read(out.join("rejected.jsonl")).unwrap();
    let first_card = fs::read(out.join("README.md")).unwrap();
    let first_manifest = fs::read(out.join("manifest.json")).unwrap();
    gate::run(GateOpts {
        out: out.clone(),
        config: None,
        pretty_name_fallback: "synthlite".into(),
    })
    .unwrap();
    assert_eq!(fs::read(out.join("data/train.jsonl")).unwrap(), first_train);
    assert_eq!(
        fs::read(out.join("rejected.jsonl")).unwrap(),
        first_rejected
    );
    assert_eq!(fs::read(out.join("README.md")).unwrap(), first_card);
    assert_eq!(fs::read(out.join("manifest.json")).unwrap(), first_manifest);
}

#[test]
fn gate_refuses_when_generation_is_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    write_run(
        dir.path(),
        &row(
            "task_0000000000000000000000000000000000000000000000000000000000000000",
            "prompt",
            &long("only"),
        ),
        false,
    );
    let err = gate::run(GateOpts {
        out: dir.path().join("out"),
        config: None,
        pretty_name_fallback: "synthlite".into(),
    })
    .unwrap_err();
    assert!(err.to_string().contains("generation_complete"), "{err}");
    assert!(!dir.path().join("out/data").exists());
}

#[test]
fn gate_binary_refuses_incomplete_generation() {
    let dir = tempfile::tempdir().unwrap();
    write_run(
        dir.path(),
        &row(
            "task_0000000000000000000000000000000000000000000000000000000000000000",
            "prompt",
            &long("only"),
        ),
        false,
    );
    let run = common::run(
        dir.path(),
        &["gate", "--out", "out"],
        &[],
        Duration::from_secs(10),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("generation_complete"), "{}", run.stderr);
    assert!(!dir.path().join("out/data").exists());
}

const ID_A: &str = "task_0000001e00000000000000000000000000000000000000000000000000000000";
const ID_B: &str = "task_0000001f00000000000000000000000000000000000000000000000000000000";

fn messages_row(id: &str, messages: Value) -> String {
    let mut value: Value = serde_json::from_str(row(id, "p", "a").trim_end()).unwrap();
    value["messages"] = messages;
    format!("{}\n", serde_json::to_string(&value).unwrap())
}

fn gate_out(dir: &std::path::Path) -> synthlite::error::Result<()> {
    gate::run(GateOpts {
        out: dir.join("out"),
        config: None,
        pretty_name_fallback: "synthlite".into(),
    })
}

#[test]
fn gate_rejects_too_long_and_repetition() {
    let dir = tempfile::tempdir().unwrap();
    let too_long = "x".repeat(40_001);
    let repeated_line = "Restart the ingest worker and watch the queue depth.\n";
    let repetition = format!("{}{}", repeated_line.repeat(5), long("tail"));
    let lines = [
        row(ID_A, "too long prompt", &too_long),
        row(ID_B, "repetition prompt", &repetition),
    ]
    .concat();
    write_run(dir.path(), &lines, true);
    gate_out(dir.path()).unwrap();
    let out = dir.path().join("out");
    let manifest: Value =
        serde_json::from_slice(&fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["counts"]["rejected"]["too_long"], 1, "{manifest}");
    assert_eq!(
        manifest["counts"]["rejected"]["repetition"], 1,
        "{manifest}"
    );
    assert_eq!(manifest["counts"]["train"], 0);
}

#[test]
fn gate_receipt_hashes_the_gated_rows_bytes() {
    let dir = tempfile::tempdir().unwrap();
    write_run(dir.path(), &row(ID_A, "prompt", &long("only")), true);
    gate_out(dir.path()).unwrap();
    let out = dir.path().join("out");
    let receipt: Value = serde_json::from_slice(&fs::read(out.join("gate.json")).unwrap()).unwrap();
    let rows = fs::read(out.join("rows.jsonl")).unwrap();
    assert_eq!(
        receipt["rows_sha256"],
        synthlite::identity::sha256_hex(&rows),
        "{receipt}"
    );
}

#[test]
fn is_validation_refuses_non_ascii_and_signed_ids_without_panicking() {
    assert!(gate::is_validation("task_a\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 100).is_err());
    assert!(gate::is_validation("task_\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 100).is_err());
    assert!(gate::is_validation("task_+1234567", 100).is_err());
    assert!(gate::is_validation("task_", 100).is_err());
}

#[test]
fn gate_refuses_malformed_source_task_id() {
    for id in [
        "task_a\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}",
        "task_+1234567",
        "task_0000000100000000000000000000000000000000000000000000000000000000ff",
        "task_0000000A00000000000000000000000000000000000000000000000000000000",
        "0000000100000000000000000000000000000000000000000000000000000000",
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_run(dir.path(), &row(id, "prompt", &long("only")), true);
        let err = gate_out(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .contains("rows.jsonl:1: invalid source_task_id"),
            "{id}: {err}"
        );
        assert!(!dir.path().join("out/data").exists());
    }
}

#[test]
fn gate_refuses_rows_that_are_not_system_user_assistant() {
    let long_reply = long("reply");
    let shapes = [
        json!([{"role": "assistant", "content": long_reply}]),
        json!([{"role": "system", "content": "sys"}, {"role": "assistant", "content": long_reply}]),
        json!([
            {"role": "user", "content": "one"},
            {"role": "user", "content": "two"},
            {"role": "assistant", "content": long_reply}
        ]),
        json!([
            {"role": "user", "content": "one"},
            {"role": "assistant", "content": long_reply},
            {"role": "user", "content": "two"},
            {"role": "assistant", "content": long_reply}
        ]),
        json!([{"role": "user", "content": ""}, {"role": "assistant", "content": long_reply}]),
        json!([{"role": "user", "content": "prompt"}, {"role": "assistant", "content": ""}]),
        json!([{"role": "user", "content": "prompt"}, {"role": "user", "content": "prompt"}]),
    ];
    for messages in shapes {
        let dir = tempfile::tempdir().unwrap();
        write_run(dir.path(), &messages_row(ID_A, messages.clone()), true);
        let err = gate_out(dir.path()).unwrap_err();
        let text = err.to_string();
        assert!(text.starts_with("rows.jsonl:1: "), "{messages}: {text}");
        assert!(
            text.contains("messages must be [system?, user, assistant]")
                || text.contains("message content is missing or empty"),
            "{messages}: {text}"
        );
        assert!(!dir.path().join("out/data").exists());
    }
}

#[test]
fn gate_accepts_system_user_assistant_rows() {
    let dir = tempfile::tempdir().unwrap();
    let messages = json!([
        {"role": "system", "content": "sys"},
        {"role": "user", "content": "prompt"},
        {"role": "assistant", "content": long("reply")}
    ]);
    write_run(dir.path(), &messages_row(ID_A, messages), true);
    gate_out(dir.path()).unwrap();
    let train = fs::read_to_string(dir.path().join("out/data/train.jsonl")).unwrap();
    assert!(train.contains(ID_A), "{train}");
}

fn detailed_config() -> Value {
    json!({
        "schema_version": "synthlite.generator-config.v1",
        "detailed": true,
        "system_message": synthlite::trace::DEFAULT_SYSTEM_MESSAGE,
        "teacher_instruction": synthlite::trace::TEACHER_INSTRUCTION,
        "temperature": null,
        "top_p": null,
        "max_output_tokens": null,
        "seed": null,
        "frequency_penalty": null,
        "presence_penalty": null,
        "stop": null,
        "reasoning_effort": null
    })
}

fn trace_text(answer: &str) -> String {
    format!(
        "## Decision trace\n1. Evidence: {}\n2. Hypothesis: h\n3. Action: a\n4. Verification: v\n5. Conclusion: c\n\n## Final answer\n{answer}",
        long("evidence")
    )
}

#[test]
fn detailed_gate_checks_the_final_answer_and_labels_the_card() {
    let refusing =
        trace_text("I can't help with changing production routers. Ask the change board.");
    let clean = trace_text("Verify end-of-RIB before any reset.");
    let lines = format!("{}{}", row(ID_A, "p1", &refusing), row(ID_B, "p2", &clean));

    let detailed = tempfile::tempdir().unwrap();
    write_run_with(detailed.path(), &lines, true, detailed_config());
    gate_out(detailed.path()).unwrap();
    let rejected = fs::read_to_string(detailed.path().join("out/rejected.jsonl")).unwrap();
    assert!(
        rejected.contains(ID_A) && rejected.contains("\"refusal\""),
        "{rejected}"
    );
    let train = fs::read_to_string(detailed.path().join("out/data/train.jsonl")).unwrap();
    assert_eq!(train, row(ID_B, "p2", &clean));
    let card = fs::read_to_string(detailed.path().join("out/README.md")).unwrap();
    assert!(card.contains("Generated with --detailed"), "{card}");
    assert!(
        card.contains(
            "They were not judged for correctness.\n\nGenerated with --detailed: each assistant reply is a decision trace (## Decision trace, numbered Evidence / Hypothesis / Action / Verification / Conclusion steps, ## Final answer) from one teacher call. Proposed checks are text. No tool was executed and no model judged the answer.\n\nTrain rows: 1."
        ),
        "{card}"
    );

    let plain = tempfile::tempdir().unwrap();
    write_run(plain.path(), &lines, true);
    gate_out(plain.path()).unwrap();
    assert!(fs::read_to_string(plain.path().join("out/rejected.jsonl"))
        .unwrap()
        .is_empty());
    let plain_card = fs::read_to_string(plain.path().join("out/README.md")).unwrap();
    assert!(!plain_card.contains("--detailed"));
    let front = |card: &str| card.split("\n---\n").next().unwrap().to_string();
    assert_eq!(front(&card), front(&plain_card));
}
