mod common;

use std::fs;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn no_args_prints_usage_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let run = common::run(dir.path(), &[], &[], Duration::from_secs(5));
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert_eq!(run.stderr, synthlite::USAGE);
    assert_eq!(run.stderr.lines().count(), 18);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
}

#[test]
fn top_level_help_prints_usage_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--help", "-h"] {
        let run = common::run(dir.path(), &[flag], &[], Duration::from_secs(5));
        assert_eq!(run.code, 2, "{flag}: {}", run.stderr);
        assert_eq!(run.stderr, synthlite::USAGE, "{flag}");
        assert!(run.stdout.is_empty(), "{flag}: {}", run.stdout);
    }
}

#[test]
fn subcommand_help_lists_every_flag_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&[&str], &[&str]); 4] = [
        (
            &["generate", "--help"],
            &[
                "<INPUT>",
                ".txt",
                "--out",
                "--config",
                "--resume",
                "--dry-run",
                "--retry-failed",
                "--resume-parked-keys",
                "--detailed",
                "Decision-trace rows (see docs/decision-traces.md)",
                "--max-requests",
                "--max-rows",
                "--progress-interval",
            ],
        ),
        (&["generate", "-h"], &["--detailed", "--dry-run"]),
        (&["gate", "--help"], &["--out", "--config"]),
        (
            &["push", "--help"],
            &["--out", "--config", "--hf-repo", "--hf-republish"],
        ),
    ];
    for (args, expected) in cases {
        let run = common::run(dir.path(), args, &[], Duration::from_secs(5));
        assert_eq!(run.code, 0, "{args:?}: {}", run.stderr);
        assert!(run.stderr.is_empty(), "{args:?}: {}", run.stderr);
        for needle in expected {
            assert!(
                run.stdout.contains(needle),
                "{args:?} lacks {needle}:\n{}",
                run.stdout
            );
        }
        // Every option line carries a description, not just the flag name.
        for line in run
            .stdout
            .lines()
            .filter(|line| line.trim_start().starts_with("--"))
        {
            let described = line
                .trim()
                .split_once("  ")
                .is_some_and(|(_, help)| !help.trim().is_empty());
            assert!(described, "{args:?}: undocumented option: {line}");
        }
    }
    assert!(!dir.path().join("out").exists());
}

#[test]
fn version_flag_prints_the_crate_version_and_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    for flag in ["--version", "-V"] {
        let run = common::run(dir.path(), &[flag], &[], Duration::from_secs(5));
        assert_eq!(run.code, 0, "{flag}: {}", run.stderr);
        assert_eq!(
            run.stdout,
            format!("synthlite {}\n", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
        assert!(run.stderr.is_empty(), "{flag}: {}", run.stderr);
    }
}

#[test]
fn the_plan_line_names_the_version() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("versioned")),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr
            .trim_end()
            .ends_with(&format!(" version={}", env!("CARGO_PKG_VERSION"))),
        "{}",
        run.stderr
    );
}

#[test]
fn missing_model_exits_2_without_a_default() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("tasks.jsonl"), "not-json\n").unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:1"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert_eq!(run.stderr.trim(), "set OPENAI_MODEL or SYNTHLITE_MODEL");
    assert!(!dir.path().join("out").exists());
}

#[test]
fn dry_run_does_not_call_or_create_out() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("dry run seed prompt")),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:1"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("seeds=1"), "{}", run.stderr);
    assert!(run.stderr.contains("teacher-test"), "{}", run.stderr);
    assert!(run.stderr.contains("base=127.0.0.1:1 "), "{}", run.stderr);
    assert!(run.stderr.contains(" start "), "{}", run.stderr);
    assert!(!dir.path().join("out").exists());
}

#[test]
fn dry_run_accepts_taskgen_curriculum_coordinates() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = common::fixture_task();
    task["coordinates"]["curriculum"] = json!({
        "context": {
            "audience": "l0_self_service",
            "enterprise_size": "medium",
            "interaction": "single_turn",
            "lifecycle": "day0_discovery_design"
        },
        "objective": "Identify the resolver used by an encrypted DNS client",
        "acceptance": ["Distinguish transport encryption from answer trust"],
        "references": ["https://www.rfc-editor.org/rfc/rfc8484"]
    });
    fs::write(dir.path().join("tasks.jsonl"), format!("{task}\n")).unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("seeds=1"), "{}", run.stderr);
}

#[test]
fn dry_run_rejects_malformed_curriculum() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = common::fixture_task();
    task["coordinates"]["curriculum"] = json!({
        "context": {},
        "objective": "Identify the resolver",
        "acceptance": ["Check the encrypted DNS client"],
        "references": ["https://www.rfc-editor.org/rfc/rfc8484"]
    });
    fs::write(dir.path().join("tasks.jsonl"), format!("{task}\n")).unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr.contains("coordinates.curriculum.context"),
        "{}",
        run.stderr
    );
}

#[test]
fn hash_mismatch_on_default_out_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("hash mismatch seed")),
    )
    .unwrap();
    let out = dir.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(
        out.join("state.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": "synthlite.state.v1",
            "synthlite_version": "0.1.0",
            "generator_config_hash": "gen_deadbeef",
            "generator_config": {},
            "source_population_sha256": "abc",
            "taskgen_run_id": null,
            "seed_count": 1,
            "created_at": "2026-09-22T00:00:00Z",
            "generation_complete": false,
            "parked_keys": []
        }))
        .unwrap(),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", "http://127.0.0.1:1"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("pass a new --out"), "{}", run.stderr);
}

#[test]
fn resume_on_missing_directory_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("resume missing directory")),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--resume"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("--resume"), "{}", run.stderr);
    assert!(!dir.path().join("out").exists());
}

#[tokio::test]
async fn malformed_line_fails_before_any_http_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let body = format!(
        "{}\n{{not json}}\n",
        common::task_line("this line is valid and must not be sent")
    );
    fs::write(dir.path().join("tasks.jsonl"), body).unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", &server.uri()),
        ],
        Duration::from_secs(10),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("tasks.jsonl:2:"), "{}", run.stderr);
    server.verify().await;
    assert!(server.received_requests().await.unwrap().is_empty());
}

fn seed_file(dir: &std::path::Path, prompt: &str) {
    fs::write(
        dir.join("tasks.jsonl"),
        format!("{}\n", common::task_line(prompt)),
    )
    .unwrap();
}

#[test]
fn missing_positional_argument_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let run = common::run(
        dir.path(),
        &["generate", "--out", "x"],
        &[],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("INPUT"), "{}", run.stderr);
}

#[test]
fn base_url_with_query_exits_2_without_echo() {
    let dir = tempfile::tempdir().unwrap();
    seed_file(dir.path(), "query base url");
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            (
                "OPENAI_BASE_URL",
                "https://gw.example/v1?user=ops@corp.example",
            ),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("base_url"), "{}", run.stderr);
    assert!(!run.stderr.contains("corp.example"), "{}", run.stderr);
}

#[test]
fn base_url_without_scheme_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    seed_file(dir.path(), "schemeless base url");
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", "api.groq.com/openai/v1"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("base_url"), "{}", run.stderr);
}

#[test]
fn empty_base_url_env_falls_back_to_openai() {
    let dir = tempfile::tempdir().unwrap();
    seed_file(dir.path(), "empty base url");
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", ""),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr.contains("base=api.openai.com "),
        "{}",
        run.stderr
    );
}

#[test]
fn userinfo_is_not_displayed() {
    let dir = tempfile::tempdir().unwrap();
    seed_file(dir.path(), "userinfo base url");
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", "https://user:hunter2@gw.example:8443/v1"),
        ],
        Duration::from_secs(5),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr.contains("base=gw.example:8443 "),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("hunter2"), "{}", run.stderr);
}

#[test]
fn config_refusals_exit_2() {
    for (toml, expect) in [
        ("[generation]\nmax_output_tokens = 0\n", "max_output_tokens"),
        (
            "[gate]\nvalidation_per_mille = 1001\n",
            "validation_per_mille",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        seed_file(dir.path(), "config refusal");
        fs::write(dir.path().join("synthlite.toml"), toml).unwrap();
        let run = common::run(
            dir.path(),
            &["tasks.jsonl", "--dry-run"],
            &[
                ("OPENAI_API_KEY", "sk-test"),
                ("OPENAI_MODEL", "teacher-test"),
            ],
            Duration::from_secs(5),
        );
        assert_eq!(run.code, 2, "{toml}: {}", run.stderr);
        assert!(run.stderr.contains(expect), "{}", run.stderr);
    }
}

#[test]
fn pasted_secret_is_not_echoed() {
    for toml in [
        "[[key]]\nprovider = \"groq\"\nbase_url = \"https://api.groq.com/openai/v1\"\nmodel = \"m\"\napi_key_env = \"gsk_live-abc.def\"\n",
        "[[key]]\nprovider = \"groq\"\napi_key_env = gsk_live-abc.def\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        seed_file(dir.path(), "secret in config");
        fs::write(dir.path().join("synthlite.toml"), toml).unwrap();
        let run = common::run(dir.path(), &["tasks.jsonl", "--dry-run"], &[], Duration::from_secs(5));
        assert_eq!(run.code, 2, "{}", run.stderr);
        assert!(!run.stderr.contains("gsk_live"), "{}", run.stderr);
    }
}

#[tokio::test]
async fn openai_dated_snapshot_model_commits_the_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "teacher-test-2024-07-18",
            "choices": [{
                "message": {"role": "assistant", "content": common::long_reply("snapshot")},
                "finish_reason": "stop"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    seed_file(dir.path(), "dated snapshot");
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", &uri),
        ],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 1, "{rows}");
    server.verify().await;
}

fn candidate_line(task: &serde_json::Value, hard_failures: serde_json::Value) -> String {
    json!({
        "schema_version": "scogo.taskgen.candidate.v1",
        "candidate_id": "c-1",
        "sequence": 1,
        "wave": 1,
        "repair_of": null,
        "repair_count": 0,
        "prompt_sha256": "abc",
        "deterministic_checks": {"schema": "pass", "hard_failures": hard_failures, "warnings": []},
        "candidate": task,
    })
    .to_string()
}

fn dry_run(dir: &std::path::Path) -> common::Run {
    common::run(
        dir,
        &["tasks.jsonl", "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    )
}

#[test]
fn taskgen_candidate_records_are_unwrapped_and_hard_failures_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = common::fixture_task();
    first["prompt"] = json!("candidate seed one");
    let mut second = common::fixture_task();
    second["prompt"] = json!("candidate seed two");
    let mut failed = common::fixture_task();
    failed["prompt"] = json!("candidate that failed Taskgen checks");
    let lines = [
        candidate_line(&first, json!([])),
        candidate_line(&failed, json!(["schema"])),
        common::task_line("plain task beside candidates"),
        candidate_line(&second, json!([])),
    ];
    fs::write(dir.path().join("tasks.jsonl"), lines.join("\n") + "\n").unwrap();
    let run = dry_run(dir.path());
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("seeds=3"), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("preflight skipped 1 Taskgen candidates with deterministic hard failures"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_candidate_hashes_to_the_same_id_as_its_task() {
    // The same record as a candidate and as a plain task is one seed twice.
    let dir = tempfile::tempdir().unwrap();
    let task = common::fixture_task();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n{}\n", candidate_line(&task, json!([])), task),
    )
    .unwrap();
    let run = dry_run(dir.path());
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("tasks.jsonl:2: duplicate source_task_id"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_candidate_without_its_task_or_with_an_unknown_task_field_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut wrapper: serde_json::Value =
        serde_json::from_str(&candidate_line(&common::fixture_task(), json!([]))).unwrap();
    wrapper.as_object_mut().unwrap().remove("candidate");
    fs::write(dir.path().join("tasks.jsonl"), format!("{wrapper}\n")).unwrap();
    let run = dry_run(dir.path());
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("tasks.jsonl:1: scogo.taskgen.candidate.v1 record has no candidate object"),
        "{}",
        run.stderr
    );
    let mut task = common::fixture_task();
    task["reviewer_note"] = json!("not a task field");
    fs::write(
        dir.path().join("tasks.jsonl"),
        candidate_line(&task, json!([])) + "\n",
    )
    .unwrap();
    let run = dry_run(dir.path());
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("tasks.jsonl:1: unknown field reviewer_note"),
        "{}",
        run.stderr
    );
}

fn dry_run_input(dir: &std::path::Path, input: &str) -> common::Run {
    common::run(
        dir,
        &[input, "--dry-run"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
        ],
        Duration::from_secs(5),
    )
}

#[test]
fn dry_run_counts_prompt_records() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        json!({"prompt": "Why are BGP paths stale after a graceful restart?"}),
        json!({"prompt": "Summarize this incident for the on-call channel.", "id": "inc-42"}),
        json!({
            "prompt": "Draft a rollback plan for a failed firmware push.",
            "id": null,
            "metadata": {"team": "network", "priority": 2}
        }),
    ];
    let body: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    // Blank lines, CRLF endings, and a byte order mark are all fine.
    fs::write(
        dir.path().join("prompts.jsonl"),
        format!("\u{feff}{}\r\n\n{}\n   \n{}", body[0], body[1], body[2]),
    )
    .unwrap();
    let run = dry_run_input(dir.path(), "prompts.jsonl");
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("seeds=3"), "{}", run.stderr);
    assert!(!run.stderr.contains("preflight skipped"), "{}", run.stderr);
    assert!(!dir.path().join("out").exists());
}

#[test]
fn a_txt_file_is_one_trimmed_prompt_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let text = "  Why are BGP paths stale?  \n\n\t\nIs {\"prompt\": \"json\"} read as text?\r\nCheck the UPS alarms.\n";
    for name in ["prompts.txt", "PROMPTS.TXT"] {
        fs::write(dir.path().join(name), text).unwrap();
        let run = dry_run_input(dir.path(), name);
        assert_eq!(run.code, 0, "{name}: {}", run.stderr);
        assert!(run.stderr.contains("seeds=3"), "{name}: {}", run.stderr);
    }
    // The same text under another extension is JSONL, and refused as such.
    fs::write(dir.path().join("prompts.md"), text).unwrap();
    let run = dry_run_input(dir.path(), "prompts.md");
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr.contains("prompts.md:1: malformed JSON"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("use a .txt file"), "{}", run.stderr);
}

#[test]
fn a_duplicate_txt_line_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("prompts.txt"),
        "Check the UPS alarms.\nRestart nothing yet.\n  Check the UPS alarms.\n",
    )
    .unwrap();
    let run = dry_run_input(dir.path(), "prompts.txt");
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("prompts.txt:3: duplicate prompt (same prompt as line 1)"),
        "{}",
        run.stderr
    );
}

#[test]
fn an_unknown_prompt_record_field_is_refused_with_a_hint() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("prompts.jsonl"),
        format!(
            "{}\n",
            json!({"prompt": "Check the UPS alarms.", "team": "facilities"})
        ),
    )
    .unwrap();
    let run = dry_run_input(dir.path(), "prompts.jsonl");
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains(r#"prompts.jsonl:1: unknown field team (put extra fields under "metadata")"#),
        "{}",
        run.stderr
    );
}

#[test]
fn an_unsupported_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (line, expected) in [
        (
            json!({"schema_version": "scogo.taskgen.task.v1", "prompt": "old task"}),
            "prompts.jsonl:1: unsupported schema_version scogo.taskgen.task.v1",
        ),
        (
            json!({"schema_version": "synthlite.prompt.v1", "prompt": "tagged prompt"}),
            "prompts.jsonl:1: unsupported schema_version synthlite.prompt.v1",
        ),
        (
            json!({"schema_version": 2, "prompt": "numbered"}),
            "prompts.jsonl:1: unsupported schema_version 2",
        ),
    ] {
        fs::write(dir.path().join("prompts.jsonl"), format!("{line}\n")).unwrap();
        let run = dry_run_input(dir.path(), "prompts.jsonl");
        assert_eq!(run.code, 2, "{line}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{line}: {}", run.stderr);
    }
}

#[test]
fn a_duplicate_prompt_record_is_refused_whatever_its_id_or_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let lines = [
        json!({"prompt": "Check the UPS alarms.", "id": "a"}),
        json!({"prompt": "Something else."}),
        json!({"prompt": "Check the UPS alarms.", "id": "b", "metadata": {"copy": true}}),
    ];
    let body: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    fs::write(dir.path().join("prompts.jsonl"), body.join("\n") + "\n").unwrap();
    let run = dry_run_input(dir.path(), "prompts.jsonl");
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("prompts.jsonl:3: duplicate prompt (same prompt as line 1)"),
        "{}",
        run.stderr
    );
}

#[test]
fn malformed_prompt_records_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (line, expected) in [
        (
            json!({"id": "no-prompt"}),
            "prompts.jsonl:1: missing prompt",
        ),
        (
            json!({"prompt": 7}),
            "prompts.jsonl:1: prompt must be a string",
        ),
        (
            json!({"prompt": ""}),
            "prompts.jsonl:1: prompt must be non-empty",
        ),
        (
            json!({"prompt": " \n\t"}),
            "prompts.jsonl:1: prompt must be non-empty",
        ),
        (
            json!({"prompt": "x".repeat(20_001)}),
            "prompts.jsonl:1: prompt is longer than 20000 characters",
        ),
        (
            json!({"prompt": "ok", "id": 3}),
            "prompts.jsonl:1: id must be a string",
        ),
        (
            json!({"prompt": "ok", "metadata": ["a"]}),
            "prompts.jsonl:1: metadata must be an object",
        ),
        (
            json!(["prompt"]),
            "prompts.jsonl:1: each line must be a JSON object",
        ),
    ] {
        fs::write(dir.path().join("prompts.jsonl"), format!("{line}\n")).unwrap();
        let run = dry_run_input(dir.path(), "prompts.jsonl");
        assert_eq!(run.code, 2, "{expected}: {}", run.stderr);
        assert!(run.stderr.contains(expected), "{expected}: {}", run.stderr);
    }
    fs::write(
        dir.path().join("prompts.jsonl"),
        format!("{}\n", json!({"prompt": "x".repeat(20_000)})),
    )
    .unwrap();
    let run = dry_run_input(dir.path(), "prompts.jsonl");
    assert_eq!(run.code, 0, "{}", run.stderr);
}

#[test]
fn taskgen_and_prompt_lines_mix_in_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut candidate = common::fixture_task();
    candidate["prompt"] = json!("candidate beside prompt records");
    // The same text as a Taskgen task and as a prompt record is two seeds:
    // the two kinds never share an id.
    let lines = [
        common::task_line("Why are BGP paths stale?"),
        json!({"prompt": "Why are BGP paths stale?"}).to_string(),
        candidate_line(&candidate, json!([])),
        json!({"prompt": "Check the UPS alarms.", "metadata": {"site": "dc-1"}}).to_string(),
    ];
    fs::write(dir.path().join("prompts.jsonl"), lines.join("\n") + "\n").unwrap();
    let run = dry_run_input(dir.path(), "prompts.jsonl");
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("seeds=4"), "{}", run.stderr);
}
