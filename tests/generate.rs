mod common;

use std::fs;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use synthlite::config::DEFAULT_MAX_OUTPUT_TOKENS;

fn env(uri: &str) -> Vec<(&str, &str)> {
    vec![
        ("OPENAI_API_KEY", "sk-test"),
        ("OPENAI_MODEL", "teacher-test"),
        ("OPENAI_BASE_URL", uri),
    ]
}

fn stop_body(text: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "model": "teacher-test",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 22}
    })
}

#[tokio::test]
async fn success_then_resume_skips_the_committed_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body("a complete teacher reply")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("resume-skip seed")),
    )
    .unwrap();
    let uri = server.uri();
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(first.stderr.contains(" start "), "{}", first.stderr);
    assert!(first.stderr.contains("teacher-test"), "{}", first.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 1);
    assert!(rows.contains("a complete teacher reply"));
    assert!(!rows.contains("sk-test"));

    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert!(second.stderr.contains(" resume "), "{}", second.stderr);
    assert_eq!(
        fs::read(dir.path().join("out/rows.jsonl")).unwrap(),
        rows.as_bytes()
    );
    let dry = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--dry-run"],
        &env(&uri),
        Duration::from_secs(10),
    );
    assert_eq!(dry.code, 0, "{}", dry.stderr);
    assert!(dry.stderr.contains(" resume "), "{}", dry.stderr);
    server.verify().await;
}

#[tokio::test]
async fn torn_tail_is_repaired_and_that_seed_is_regenerated() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body("regenerated teacher reply")),
        )
        .expect(3)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("torn seed alpha"),
            common::task_line("torn seed beta")
        ),
    )
    .unwrap();
    let uri = server.uri();
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let original = fs::read(dir.path().join("out/rows.jsonl")).unwrap();
    let newline = original.iter().position(|byte| *byte == b'\n').unwrap();
    let first_line = original[..=newline].to_vec();
    let mut torn = first_line.clone();
    torn.extend_from_slice(br#"{"schema_version":"synthlite.sft.v1","source_task_id":"partial"#);
    fs::write(dir.path().join("out/rows.jsonl"), &torn).unwrap();

    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    let repaired = fs::read(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(repaired.starts_with(&first_line));
    assert_eq!(*repaired.last().unwrap(), b'\n');
    assert_eq!(repaired.iter().filter(|byte| **byte == b'\n').count(), 2);
    let partial = fs::read_to_string(dir.path().join("out/rows.partial")).unwrap();
    assert!(partial.starts_with(r#"{"schema_version""#));
    assert_eq!(partial.lines().count(), 1, "{partial}");
    assert!(partial.ends_with('\n'));

    // A second torn tail is appended to the quarantine, not written over the first.
    let mut torn = fs::read(dir.path().join("out/rows.jsonl")).unwrap();
    torn.extend_from_slice(b"SECOND_TORN_FRAGMENT");
    fs::write(dir.path().join("out/rows.jsonl"), &torn).unwrap();
    let third = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(third.code, 0, "{}", third.stderr);
    let partial = fs::read_to_string(dir.path().join("out/rows.partial")).unwrap();
    assert_eq!(partial.lines().count(), 2, "{partial}");
    assert!(partial.starts_with(r#"{"schema_version""#), "{partial}");
    assert!(partial.ends_with("SECOND_TORN_FRAGMENT\n"), "{partial}");
    server.verify().await;
}

#[tokio::test]
async fn failed_row_is_not_retried_until_retry_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "teacher-test",
            "choices": [{
                "message": {"role": "assistant", "content": "cut off"},
                "finish_reason": "length"
            }]
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_body("retried teacher reply")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let prompt = "FAILED_PROMPT_BODY_SHOULD_NOT_LEAK";
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line(prompt)),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        "[generation]\nmax_output_tokens = 512\n",
    )
    .unwrap();
    let uri = server.uri();
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 4, "{}", first.stderr);
    assert!(
        first
            .stderr
            .contains("1 work items failed; rerun with --retry-failed"),
        "{}",
        first.stderr
    );
    assert!(!dir.path().join("out/rows.jsonl").exists());
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert!(errors.contains("truncated"), "{errors}");
    assert!(!errors.contains(prompt), "{errors}");
    assert!(!errors.contains("sk-test"), "{errors}");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 4, "{}", second.stderr);
    assert!(
        second.stderr.contains("rerun with --retry-failed"),
        "{}",
        second.stderr
    );
    assert!(!dir.path().join("out/rows.jsonl").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let third = common::run(
        dir.path(),
        &["generate", "--retry-failed", "tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(third.code, 0, "{}", third.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(rows.contains("retried teacher reply"), "{rows}");
    assert!(!rows.contains("\"finish_reason\""));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    server.verify().await;
}

#[tokio::test]
async fn implicit_budget_sends_default_max_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(format!(
            "\"max_tokens\":{}",
            DEFAULT_MAX_OUTPUT_TOKENS
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body("smoke-sized teacher reply")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("implicit budget smoke")),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(rows.contains("smoke-sized teacher reply"), "{rows}");
    let row: Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    assert_eq!(
        row["metadata"]["max_output_tokens"],
        DEFAULT_MAX_OUTPUT_TOKENS
    );
    server.verify().await;
}

#[tokio::test]
async fn length_at_the_default_budget_is_truncated_without_regenerating() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains(format!(
            "\"max_tokens\":{}",
            DEFAULT_MAX_OUTPUT_TOKENS
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(length_body()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("very long answer")),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    let error: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(error["error_class"], "truncated");
    assert_eq!(error["max_output_tokens"], 32_768);
    assert!(
        !dir.path().join("out/rows.jsonl").exists()
            || fs::read_to_string(dir.path().join("out/rows.jsonl"))
                .unwrap()
                .is_empty()
    );
    server.verify().await;
}

#[tokio::test]
async fn a_rejected_budget_steps_down_and_the_key_remembers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"max_tokens\":32768"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"max_tokens\":16384"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("capped"))),
        )
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("capped one"),
            common::task_line("capped two")
        ),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_concurrent = 1"),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let seed_steps = stderr_lines(&run.stderr, "budget task_");
    assert_eq!(seed_steps.len(), 1, "{}", run.stderr);
    assert!(
        seed_steps[0].contains("max_output_tokens=32768->16384 reason=http_400"),
        "{}",
        run.stderr
    );
    let key_steps = stderr_lines(&run.stderr, "budget openai/");
    assert_eq!(key_steps.len(), 1, "{}", run.stderr);
    assert!(
        key_steps[0].contains("max_output_tokens=32768->16384"),
        "{}",
        run.stderr
    );
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    for line in rows.lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["metadata"]["max_output_tokens"], 16_384);
    }
    assert!(
        stderr_lines(&run.stderr, "done ")[0].contains("budget_retry=1"),
        "{}",
        run.stderr
    );
    server.verify().await;
}

#[tokio::test]
async fn a_400_at_every_budget_is_an_invalid_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400))
        .expect(4)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("always bad")),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert_eq!(
        stderr_lines(&run.stderr, "budget task_").len(),
        3,
        "{}",
        run.stderr
    );
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    let error: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(error["error_class"], "invalid_request");
    assert_eq!(error["http_status"], 400);
    assert_eq!(error["max_output_tokens"], 4096);
    server.verify().await;
}

#[tokio::test]
async fn an_explicit_budget_is_never_stepped_down() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"max_tokens\":20000"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("explicit")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        format!(
            "[generation]\nmax_output_tokens = 20000\n{}",
            key_toml(&server.uri(), "")
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(
        stderr_lines(&run.stderr, "budget ").is_empty(),
        "{}",
        run.stderr
    );
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert!(
        errors.contains("\"error_class\":\"invalid_request\""),
        "{errors}"
    );
    server.verify().await;
}

#[tokio::test]
async fn a_429_burst_halves_concurrency_once_and_does_not_park() {
    let server = MockServer::start().await;
    // The first four requests arrive together and all get a header-less 429.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).set_delay(Duration::from_millis(200)))
        .up_to_n_times(4)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("after burst"))),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let tasks: String = (0..4)
        .map(|i| format!("{}\n", common::task_line(&format!("burst seed {i}"))))
        .collect();
    fs::write(dir.path().join("tasks.jsonl"), tasks).unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(
            &server.uri(),
            "max_concurrent = 4\nrate_limit_cooldown_seconds = 1\nheaderless_429_limit = 2",
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(30),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(!run.stderr.contains("parked"), "{}", run.stderr);
    let down: Vec<&str> = stderr_lines(&run.stderr, "throttle ")
        .into_iter()
        .filter(|line| line.contains("reason=429"))
        .collect();
    assert_eq!(down.len(), 1, "{}", run.stderr);
    assert!(down[0].contains("max_concurrent=4->2"), "{}", run.stderr);
    assert_eq!(
        fs::read_to_string(dir.path().join("out/rows.jsonl"))
            .unwrap()
            .lines()
            .count(),
        4
    );
}

#[tokio::test]
async fn concurrency_grows_back_after_clean_responses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("recovering"))),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let tasks: String = (0..8)
        .map(|i| format!("{}\n", common::task_line(&format!("recover seed {i}"))))
        .collect();
    fs::write(dir.path().join("tasks.jsonl"), tasks).unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_concurrent = 2"),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(30),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let throttles = stderr_lines(&run.stderr, "throttle ");
    assert!(
        throttles
            .iter()
            .any(|l| l.contains("max_concurrent=2->1 reason=429")),
        "{}",
        run.stderr
    );
    assert!(
        throttles.iter().any(|l| l.ends_with("max_concurrent=1->2")),
        "{}",
        run.stderr
    );
}

#[tokio::test]
async fn explicit_small_budget_truncation_stays_permanent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("\"max_tokens\":256"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model": "teacher-test",
            "choices": [{
                "message": {"role": "assistant", "content": "cut off"},
                "finish_reason": "length"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("explicit cap")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        "[generation]\nmax_output_tokens = 256\n",
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(!dir.path().join("out/rows.jsonl").exists());
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert!(errors.contains("truncated"), "{errors}");
    let record: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(record["max_output_tokens"], 256);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn headerless_429_parks_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(429))
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("park this seed")),
    )
    .unwrap();
    let toml = format!(
        r#"
[[key]]
provider = "openai"
base_url = "{}"
model = "teacher-test"
api_key_env = "OPENAI_API_KEY"
rate_limit_cooldown_seconds = 0
headerless_429_limit = 2
max_concurrent = 1
timeout_seconds = 5
"#,
        server.uri()
    );
    fs::write(dir.path().join("synthlite.toml"), toml).unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stderr.contains("parked"), "{}", run.stderr);
    assert!(!dir.path().join("out/rows.jsonl").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(dir.path().join("out/state.json")).unwrap()).unwrap();
    assert_eq!(state["generation_complete"], false);
    assert_eq!(state["parked_keys"][0]["reason"], "headerless_429");
    server.verify().await;
}

fn key_toml(uri: &str, extra: &str) -> String {
    format!(
        r#"
[[key]]
provider = "openai"
base_url = "{uri}"
model = "teacher-test"
api_key_env = "OPENAI_API_KEY"
timeout_seconds = 5
{extra}
"#
    )
}

fn length_body() -> Value {
    json!({
        "model": "teacher-test",
        "choices": [{
            "message": {"role": "assistant", "content": "cut off"},
            "finish_reason": "length"
        }]
    })
}

fn read_state(dir: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("out/state.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn retry_failed_gets_a_fresh_retry_budget() {
    let server = MockServer::start().await;
    // Run 1 spends both attempts on 503s; run 2 sees one more 503, then succeeds.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(3)
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_body("fresh budget reply")))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("retry budget seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_attempts = 2\nmax_concurrent = 1"),
    )
    .unwrap();
    let key_env = [("OPENAI_API_KEY", "sk-test")];
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 4, "{}", first.stderr);
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    let record: Value = serde_json::from_str(errors.lines().last().unwrap()).unwrap();
    assert_eq!(record["error_class"], "retries_exhausted");
    assert_eq!(record["attempt"], 2);

    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--retry-failed"],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(rows.contains("fresh budget reply"), "{rows}");
    assert_eq!(read_state(dir.path())["generation_complete"], true);
    server.verify().await;
}

#[tokio::test]
async fn retry_failed_exhaustion_keeps_counting_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(4)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("retry count seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_attempts = 2\nmax_concurrent = 1"),
    )
    .unwrap();
    let key_env = [("OPENAI_API_KEY", "sk-test")];
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 4, "{}", first.stderr);
    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--retry-failed"],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 4, "{}", second.stderr);
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    let attempts: Vec<u64> = errors
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["attempt"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(attempts, vec![2, 4]);
    server.verify().await;
}

#[tokio::test]
async fn non_429_response_resets_the_headerless_429_count() {
    let server = MockServer::start().await;
    for (priority, status) in [(1u8, 429u16), (2, 503), (3, 429)] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(status))
            .up_to_n_times(1)
            .expect(1)
            .with_priority(priority)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_body("not parked reply")))
        .expect(1)
        .with_priority(4)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("interleaved 429 seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(
            &server.uri(),
            "rate_limit_cooldown_seconds = 0\nheaderless_429_limit = 2\nmax_concurrent = 1",
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("OPENAI_API_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(!run.stderr.contains("parked"), "{}", run.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(rows.contains("not parked reply"), "{rows}");
    server.verify().await;
}

#[tokio::test]
async fn unauthorized_parks_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string("UNAUTHORIZED_BODY_SHOULD_NOT_LEAK"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("unauthorized seed")),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    assert!(run.stderr.contains("all keys are parked"), "{}", run.stderr);
    let host = uri.trim_start_matches("http://");
    assert!(
        run.stderr.contains(&format!(
            "parked openai/OPENAI_API_KEY/teacher-test@{host} unauthorized"
        )),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("http://"), "{}", run.stderr);
    assert!(!run.stderr.contains("sk-test"), "{}", run.stderr);
    assert!(!run.stderr.contains("UNAUTHORIZED_BODY"), "{}", run.stderr);
    let state = read_state(dir.path());
    assert_eq!(state["generation_complete"], false);
    assert_eq!(state["parked_keys"][0]["reason"], "unauthorized");
    assert!(!dir.path().join("out/rows.jsonl").exists());
    server.verify().await;
}

#[tokio::test]
async fn max_requests_and_max_rows_stop_with_exit_3() {
    for cap in ["--max-requests", "--max-rows"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(stop_body("capped reply")))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("tasks.jsonl"),
            format!(
                "{}\n{}\n",
                common::task_line("capped seed one"),
                common::task_line("capped seed two")
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("synthlite.toml"),
            key_toml(&server.uri(), "max_concurrent = 1"),
        )
        .unwrap();
        let run = common::run(
            dir.path(),
            &["tasks.jsonl", "--out", "out", cap, "1"],
            &[("OPENAI_API_KEY", "sk-test")],
            Duration::from_secs(20),
        );
        assert_eq!(run.code, 3, "{cap}: {}", run.stderr);
        assert!(
            run.stderr
                .contains("stopped by --max-requests or --max-rows"),
            "{cap}: {}",
            run.stderr
        );
        let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 1, "{cap}");
        assert_eq!(read_state(dir.path())["generation_complete"], false);
        server.verify().await;
    }
}

#[tokio::test]
async fn torn_tail_in_errors_file_is_quarantined() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(length_body()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("torn errors seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        "[generation]\nmax_output_tokens = 256\n",
    )
    .unwrap();
    let uri = server.uri();
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 4, "{}", first.stderr);
    let errors_path = dir.path().join("out/rows.errors.jsonl");
    let original = fs::read(&errors_path).unwrap();
    let mut torn = original.clone();
    torn.extend_from_slice(br#"{"source_task_id":"torn-error"#);
    fs::write(&errors_path, &torn).unwrap();

    let gate = common::run(
        dir.path(),
        &["gate", "--out", "out"],
        &[],
        Duration::from_secs(10),
    );
    assert_eq!(gate.code, 2, "{}", gate.stderr);
    assert!(gate.stderr.contains("torn tail"), "{}", gate.stderr);

    let second = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 4, "{}", second.stderr);
    assert_eq!(fs::read(&errors_path).unwrap(), original);
    let partial = fs::read_to_string(dir.path().join("out/rows.errors.partial")).unwrap();
    assert_eq!(partial, "{\"source_task_id\":\"torn-error\n");
    server.verify().await;
}

#[tokio::test]
async fn a_locked_out_dir_refuses_every_command() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("locked seed")),
    )
    .unwrap();
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(out.join(".synthlite.lock"))
        .unwrap();
    lock.try_lock().unwrap();

    let uri = server.uri();
    let generate = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(generate.code, 2, "{}", generate.stderr);
    assert!(
        generate
            .stderr
            .contains("another synthlite process is using"),
        "{}",
        generate.stderr
    );
    assert!(!out.join("state.json").exists());

    let gate = common::run(
        dir.path(),
        &["gate", "--out", "out"],
        &[],
        Duration::from_secs(10),
    );
    assert_eq!(gate.code, 2, "{}", gate.stderr);
    assert!(
        gate.stderr.contains("another synthlite process is using"),
        "{}",
        gate.stderr
    );

    let push = common::run(
        dir.path(),
        &["push", "--hf-repo", "test-org/locked", "--out", "out"],
        &[("HF_TOKEN", "hf_test"), ("HF_ENDPOINT", &uri)],
        Duration::from_secs(10),
    );
    assert_eq!(push.code, 2, "{}", push.stderr);
    assert!(
        push.stderr.contains("another synthlite process is using"),
        "{}",
        push.stderr
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    drop(lock);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_body("unlocked reply")))
        .expect(1)
        .mount(&server)
        .await;
    let unlocked = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(unlocked.code, 0, "{}", unlocked.stderr);
    server.verify().await;
}

fn stderr_lines<'a>(stderr: &'a str, prefix: &str) -> Vec<&'a str> {
    stderr
        .lines()
        .filter(|line| line.starts_with(prefix))
        .collect()
}

fn progress_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| {
            line.split_once(' ')
                .is_some_and(|(_, rest)| rest.starts_with("progress "))
        })
        .collect()
}

#[tokio::test]
async fn slow_run_prints_progress_heartbeats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(stop_body(&common::long_reply("slow")))
                .set_delay(Duration::from_millis(2500)),
        )
        .expect(2)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("slow seed one"),
            common::task_line("slow seed two")
        ),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--progress-interval", "1"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let beats = progress_lines(&run.stderr);
    assert!(beats.len() >= 2, "{}", run.stderr);
    assert!(beats[0].contains("committed=0/2"), "{}", run.stderr);
    assert!(beats[0].contains("in_flight=2"), "{}", run.stderr);
    assert!(beats[0].contains("elapsed="), "{}", run.stderr);
    let done = stderr_lines(&run.stderr, "done ");
    assert_eq!(done.len(), 1, "{}", run.stderr);
    assert!(done[0].contains("committed=2"), "{}", run.stderr);
    assert!(done[0].contains("budget_retry=0"), "{}", run.stderr);
    assert!(
        done[0].contains("input_tokens=22 output_tokens=44"),
        "{}",
        run.stderr
    );
    assert!(done[0].contains("elapsed="), "{}", run.stderr);
    assert!(!run.stderr.contains("slow seed"), "{}", run.stderr);
    server.verify().await;
}

#[tokio::test]
async fn progress_interval_zero_disables_heartbeats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(stop_body(&common::long_reply("quiet")))
                .set_delay(Duration::from_millis(1500)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("quiet seed")),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--progress-interval", "0"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(progress_lines(&run.stderr).is_empty(), "{}", run.stderr);
    assert_eq!(
        stderr_lines(&run.stderr, "done ").len(),
        1,
        "{}",
        run.stderr
    );
    server.verify().await;
}

#[tokio::test]
async fn retries_and_failures_are_reported_as_they_happen() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("flaky seed"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("flaky seed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("flaky"))),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("long seed"))
        .respond_with(ResponseTemplate::new(200).set_body_json(length_body()))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("flaky seed"),
            common::task_line("long seed")
        ),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(30),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);

    let retries = stderr_lines(&run.stderr, "retry task_");
    assert_eq!(retries.len(), 1, "{}", run.stderr);
    assert!(retries[0].contains("try=1/5"), "{}", run.stderr);
    assert!(retries[0].contains("reason=http_503"), "{}", run.stderr);
    assert!(retries[0].contains("backoff="), "{}", run.stderr);

    let failures = stderr_lines(&run.stderr, "failed task_");
    assert_eq!(failures.len(), 1, "{}", run.stderr);
    assert!(failures[0].contains("class=truncated"), "{}", run.stderr);
    assert!(failures[0].contains("status=200"), "{}", run.stderr);
    assert!(
        failures[0].contains("max_output_tokens=32768"),
        "{}",
        run.stderr
    );

    let done = stderr_lines(&run.stderr, "done ");
    assert!(done[0].contains("committed=1"), "{}", run.stderr);
    assert!(done[0].contains("failed=1"), "{}", run.stderr);
    assert!(done[0].contains("retry=1"), "{}", run.stderr);
    assert!(done[0].contains("budget_retry=0"), "{}", run.stderr);
    assert!(done[0].contains("requests=3"), "{}", run.stderr);

    for secret in ["sk-test", "flaky seed", "long seed", "cut off"] {
        assert!(
            !run.stderr.contains(secret),
            "{secret} leaked: {}",
            run.stderr
        );
    }
    server.verify().await;
}

#[tokio::test]
async fn a_timed_out_request_is_reported_as_a_timeout_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(stop_body(&common::long_reply("late")))
                .set_delay(Duration::from_secs(3)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("on time"))),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("timeout seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        format!(
            "[[key]]\nprovider = \"fake\"\nbase_url = \"{}\"\nmodel = \"teacher-test\"\napi_key_env = \"FAKE_KEY\"\ntimeout_seconds = 1\n",
            server.uri()
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(30),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let retries = stderr_lines(&run.stderr, "retry task_");
    assert_eq!(retries.len(), 1, "{}", run.stderr);
    assert!(retries[0].contains("reason=timeout"), "{}", run.stderr);
}

fn one_seed(dir: &std::path::Path, prompt: &str, config: &str) {
    fs::write(
        dir.join("tasks.jsonl"),
        format!("{}\n", common::task_line(prompt)),
    )
    .unwrap();
    fs::write(dir.join("synthlite.toml"), config).unwrap();
}

#[test]
fn streamed_reply_is_assembled_without_reasoning() {
    let reply = common::long_reply("streamed");
    let (head, tail) = reply.split_at(reply.len() / 2);
    let (base, bodies) = common::sse_server(vec![common::sse_completion(
        &["thinking about ", "the evidence"],
        &[head, tail],
        "stop",
        321,
        0,
    )]);
    let dir = tempfile::tempdir().unwrap();
    one_seed(dir.path(), "stream seed", &common::key_config(&base, ""));
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let row: Value = serde_json::from_str(
        fs::read_to_string(dir.path().join("out/rows.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(row["messages"][1]["content"], reply.trim());
    assert!(!row.to_string().contains("thinking about"));
    assert_eq!(row["metadata"]["usage"]["completion_tokens"], 321);
    assert_eq!(row["metadata"]["served_model"], "teacher-test");
    let request: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(request["stream"], true);
    assert_eq!(request["stream_options"]["include_usage"], true);
}

#[test]
fn streamed_length_stop_never_commits_partial_text() {
    let (base, bodies) = common::sse_server(vec![common::sse_completion(
        &["long"],
        &["half an answer"],
        "length",
        32_768,
        0,
    )]);
    let dir = tempfile::tempdir().unwrap();
    one_seed(
        dir.path(),
        "stream length seed",
        &common::key_config(&base, ""),
    );
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert_eq!(bodies.lock().unwrap().len(), 1);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap_or_default();
    assert!(!rows.contains("half an answer"));
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert!(errors.contains("\"error_class\":\"truncated\""), "{errors}");
    let done = stderr_lines(&run.stderr, "done ");
    assert!(
        done[0].contains("input_tokens=11 output_tokens=32768"),
        "{}",
        run.stderr
    );
}

#[test]
fn slow_stream_shows_live_tokens_and_outlives_the_idle_timeout() {
    // 20 chunks, 150 ms apart: 3 s in total, far past timeout_seconds = 1,
    // but never idle for a whole second.
    let pieces: Vec<String> = (0..20)
        .map(|i| format!("step {i} of a careful answer that keeps going. "))
        .collect();
    let content: Vec<&str> = pieces.iter().map(String::as_str).collect();
    let (base, _) =
        common::sse_server(vec![common::sse_completion(&[], &content, "stop", 20, 150)]);
    let dir = tempfile::tempdir().unwrap();
    one_seed(
        dir.path(),
        "slow stream seed",
        &common::key_config(&base, "timeout_seconds = 1"),
    );
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--progress-interval", "1"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        stderr_lines(&run.stderr, "retry ").is_empty(),
        "{}",
        run.stderr
    );
    let beats: Vec<&str> = progress_lines(&run.stderr)
        .into_iter()
        .filter(|line| line.contains("committed=0/1"))
        .collect();
    assert!(
        !beats.is_empty(),
        "no progress before commit:\n{}",
        run.stderr
    );
    assert!(
        beats[0].contains("tok_s=? input_tokens=0 output_tokens=0"),
        "{}",
        run.stderr
    );
    let done = stderr_lines(&run.stderr, "done ");
    assert!(
        done[0].contains("input_tokens=11 output_tokens=20"),
        "{}",
        run.stderr
    );
}

#[test]
fn a_stalled_stream_times_out_and_is_retried() {
    let reply = common::long_reply("second try");
    let (base, bodies) = common::sse_server(vec![
        // One chunk, then silence well past the 1 s idle timeout.
        vec![
            (0, json!({"model": "teacher-test", "choices": [{"index": 0, "delta": {"content": "stalled "}, "finish_reason": null}]}).to_string()),
            (4000, "[DONE]".to_string()),
        ],
        common::sse_completion(&[], &[&reply], "stop", 40, 0),
    ]);
    let dir = tempfile::tempdir().unwrap();
    one_seed(
        dir.path(),
        "stall seed",
        &common::key_config(&base, "timeout_seconds = 1"),
    );
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let retries = stderr_lines(&run.stderr, "retry task_");
    assert_eq!(retries.len(), 1, "{}", run.stderr);
    assert!(retries[0].contains("reason=timeout"), "{}", run.stderr);
    let done = stderr_lines(&run.stderr, "done ");
    assert!(
        done[0].contains("input_tokens=11 output_tokens=40 usage_missing=1"),
        "{}",
        run.stderr
    );
    assert_eq!(bodies.lock().unwrap().len(), 2);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert!(!rows.contains("stalled"));
}

#[test]
fn a_stream_cut_before_finish_is_retried() {
    let reply = common::long_reply("after cut");
    let (base, _) = common::sse_server(vec![
        vec![(0, json!({"model": "teacher-test", "choices": [{"index": 0, "delta": {"content": "cut "}, "finish_reason": null}]}).to_string())],
        common::sse_completion(&[], &[&reply], "stop", 40, 0),
    ]);
    let dir = tempfile::tempdir().unwrap();
    one_seed(dir.path(), "cut seed", &common::key_config(&base, ""));
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let retries = stderr_lines(&run.stderr, "retry task_");
    assert_eq!(retries.len(), 1, "{}", run.stderr);
    assert!(retries[0].contains("reason=stream_cut"), "{}", run.stderr);
}

const DEFAULT_SYSTEM: &str = "You are a careful IT operations assistant. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery.";
const TRACE: &str = r#"{"reasoning_steps":[{"kind":"evidence","content":"The prompt reports stale BGP paths."},{"kind":"hypothesis","content":"Graceful restart did not finish."},{"kind":"action","content":"Request show bgp neighbor detail from Border Router B."},{"kind":"verification","content":"Confirm end-of-RIB arrived and stale paths cleared."},{"kind":"conclusion","content":"The session is up but recovery is incomplete."}],"final_answer":"Treat this as incomplete graceful-restart recovery and verify end-of-RIB before any reset."}"#;

async fn mock_reply(text: &str, expect: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(stop_body(text)))
        .expect(expect)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn detailed_commits_a_rendered_trace() {
    let server = mock_reply(TRACE, 1).await;
    let dir = tempfile::tempdir().unwrap();
    one_seed(dir.path(), "Why are BGP paths stale?", "");
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--detailed"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(request.get("detailed").is_none(), "{request}");
    assert!(request.get("teacher_instruction").is_none(), "{request}");
    let outbound_system = request["messages"][0]["content"].as_str().unwrap();
    assert!(
        outbound_system.starts_with(DEFAULT_SYSTEM),
        "{outbound_system}"
    );
    assert!(
        outbound_system.contains("One JSON object and no other text"),
        "{outbound_system}"
    );

    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 1);
    assert!(
        !rows.contains("One JSON object and no other text"),
        "{rows}"
    );
    let row: Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    assert_eq!(
        row["messages"][0],
        json!({"role": "system", "content": DEFAULT_SYSTEM})
    );
    assert_eq!(row["messages"][1]["content"], "Why are BGP paths stale?");
    let assistant = row["messages"][2]["content"].as_str().unwrap();
    assert!(
        assistant.starts_with("## Decision trace\n1. Evidence: The prompt reports"),
        "{assistant}"
    );
    assert!(
        assistant.ends_with(
            "\n\n## Final answer\nTreat this as incomplete graceful-restart recovery and verify end-of-RIB before any reset."
        ),
        "{assistant}"
    );
    assert!(row["metadata"].get("trace").is_none(), "{row}");
    assert_eq!(read_state(dir.path())["generator_config"]["detailed"], true);
    server.verify().await;
}

#[tokio::test]
async fn a_memo_is_one_permanent_invalid_trace() {
    let server = mock_reply(
        "**Most likely hypothesis:** graceful restart did not finish. memo-marker-7f3",
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    one_seed(
        dir.path(),
        "Why are BGP paths stale?",
        "[generation]\ndetailed = true\n",
    );
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(
        run.stderr.contains("class=invalid_trace status=200"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("memo-marker-7f3"), "{}", run.stderr);
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert_eq!(errors.lines().count(), 1, "{errors}");
    assert!(!errors.contains("memo-marker-7f3"), "{errors}");
    let error: Value = serde_json::from_str(errors.lines().next().unwrap()).unwrap();
    assert_eq!(error["error_class"], "invalid_trace");
    assert_eq!(error["http_status"], 200);
    assert!(!dir.path().join("out/rows.jsonl").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn detailed_refuses_an_operator_system_prompt() {
    let server = mock_reply(TRACE, 0).await;
    let dir = tempfile::tempdir().unwrap();
    one_seed(dir.path(), "Why are BGP paths stale?", "");
    let uri = server.uri();
    let mut vars = env(&uri);
    vars.push((
        "SYNTHLITE_SYSTEM_PROMPT",
        "Follow the user's requested role.",
    ));
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--detailed"],
        &vars,
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("--detailed uses a fixed system message"),
        "{}",
        run.stderr
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    server.verify().await;
}

#[tokio::test]
async fn switching_mode_on_an_out_dir_names_the_mode() {
    // A plain out dir rerun with --detailed.
    let plain_server = mock_reply("a complete plain teacher reply", 1).await;
    let plain = tempfile::tempdir().unwrap();
    one_seed(plain.path(), "Why are BGP paths stale?", "");
    let uri = plain_server.uri();
    let first = common::run(
        plain.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let second = common::run(
        plain.path(),
        &["tasks.jsonl", "--out", "out", "--detailed"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 2, "{}", second.stderr);
    assert!(
        second
            .stderr
            .contains("was generated without --detailed; remove --detailed and [generation].detailed, or pass a new --out"),
        "{}",
        second.stderr
    );
    assert_eq!(plain_server.received_requests().await.unwrap().len(), 1);
    plain_server.verify().await;

    // A detailed out dir rerun without --detailed.
    let detailed_server = mock_reply(TRACE, 1).await;
    let detailed = tempfile::tempdir().unwrap();
    one_seed(detailed.path(), "Why are BGP paths stale?", "");
    let uri = detailed_server.uri();
    let first = common::run(
        detailed.path(),
        &["tasks.jsonl", "--out", "out", "--detailed"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    let second = common::run(
        detailed.path(),
        &["tasks.jsonl", "--out", "out"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 2, "{}", second.stderr);
    assert!(
        second
            .stderr
            .contains("was generated with --detailed; rerun with --detailed or pass a new --out"),
        "{}",
        second.stderr
    );
    assert_eq!(detailed_server.received_requests().await.unwrap().len(), 1);
    detailed_server.verify().await;
}

#[test]
fn streamed_detailed_replies_are_rendered_or_invalid_trace() {
    // Real providers stream; the trace step must run on the folded stream too.
    let (head, tail) = TRACE.split_at(TRACE.len() / 2);
    let (base, bodies) = common::sse_server(vec![
        common::sse_completion(
            &["private reasoning"],
            &["```json\n", head, tail, "\n```"],
            "stop",
            40,
            0,
        ),
        common::sse_completion(
            &[],
            &["**Most likely hypothesis:** memo-marker-9c1"],
            "stop",
            12,
            0,
        ),
    ]);
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("stream seed one"),
            common::task_line("stream seed two")
        ),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        format!(
            "[generation]\ndetailed = true\n\n{}",
            common::key_config(&base, "max_concurrent = 1")
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[("FAKE_KEY", "sk-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(
        run.stderr.contains("class=invalid_trace status=200"),
        "{}",
        run.stderr
    );
    assert!(!run.stderr.contains("memo-marker-9c1"), "{}", run.stderr);
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    assert_eq!(rows.lines().count(), 1, "{rows}");
    assert!(!rows.contains("private reasoning"));
    assert!(!rows.contains("reasoning_steps"));
    let row: Value = serde_json::from_str(rows.lines().next().unwrap()).unwrap();
    assert_eq!(row["messages"][0]["content"], DEFAULT_SYSTEM);
    let text = row["messages"][2]["content"].as_str().unwrap();
    assert!(
        text.starts_with("## Decision trace\n1. Evidence: The prompt reports stale BGP paths.\n"),
        "{text}"
    );
    assert!(text.ends_with("\n\n## Final answer\nTreat this as incomplete graceful-restart recovery and verify end-of-RIB before any reset."));
    let errors = fs::read_to_string(dir.path().join("out/rows.errors.jsonl")).unwrap();
    assert_eq!(errors.lines().count(), 1, "{errors}");
    assert!(
        errors.contains("\"error_class\":\"invalid_trace\""),
        "{errors}"
    );
    assert!(!errors.contains("memo-marker-9c1"));
    let request: Value = serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(request["stream"], true);
    assert!(request.get("detailed").is_none());
    assert!(request.get("teacher_instruction").is_none());
}

fn rows(dir: &std::path::Path) -> Vec<Value> {
    fs::read_to_string(dir.join("out/rows.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn metadata_keys(row: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = row["metadata"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// Metadata keys of a Taskgen row: the 0.3 set plus `served_model`.
const TASKGEN_ROW_KEYS: [&str; 14] = [
    "base_url",
    "category",
    "coordinates",
    "difficulty",
    "domain",
    "generated_at",
    "language",
    "max_output_tokens",
    "model",
    "provider",
    "served_model",
    "subdomain",
    "taskgen_model",
    "usage",
];

const SERVED: &str = "meta-llama/llama-3.3-70b-instruct:free";

/// An OpenRouter-shaped stream: a processing comment, then chunks that name
/// the free model the router picked rather than `openrouter/free`.
fn openrouter_stream(reply: &str) -> common::SseScript {
    let mut script = vec![(0, ": OPENROUTER PROCESSING".to_string())];
    let (head, tail) = reply.split_at(reply.len() / 2);
    script.extend(
        common::sse_completion(&[], &[head, tail], "stop", 50, 0)
            .into_iter()
            .map(|(delay, data)| {
                (
                    delay,
                    data.replace("\"teacher-test\"", &format!("\"{SERVED}\"")),
                )
            }),
    );
    script
}

#[test]
fn prompt_records_through_openrouter_free_record_the_served_model() {
    let first = common::long_reply("openrouter one");
    let second = common::long_reply("openrouter two");
    let (base, bodies) =
        common::sse_server(vec![openrouter_stream(&first), openrouter_stream(&second)]);
    let dir = tempfile::tempdir().unwrap();
    let records = [
        json!({
            "prompt": "Why are BGP paths stale after a graceful restart?",
            "id": "inc-42",
            "metadata": {"team": "network", "priority": 2}
        }),
        json!({"prompt": "Check the UPS alarms before the maintenance window."}),
    ];
    let lines: Vec<String> = records.iter().map(Value::to_string).collect();
    fs::write(dir.path().join("prompts.jsonl"), lines.join("\n") + "\n").unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        format!(
            "[[key]]\nprovider = \"openrouter\"\nbase_url = \"{base}\"\nmodel = \"openrouter/free\"\napi_key_env = \"OPENROUTER_API_KEY\"\nrequire_model_match = false\nrequests_per_minute = 20\nmax_concurrent = 4\n"
        ),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["prompts.jsonl"],
        &[("OPENROUTER_API_KEY", "sk-or-test")],
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(
        run.stderr.contains("model=openrouter/free "),
        "{}",
        run.stderr
    );

    let rows = rows(dir.path());
    assert_eq!(rows.len(), 2);
    let with_id = rows
        .iter()
        .find(|row| row["metadata"]["input_id"] == "inc-42")
        .unwrap();
    let bare = rows
        .iter()
        .find(|row| row["metadata"]["input_id"].is_null())
        .unwrap();
    for (row, record) in [(with_id, &records[0]), (bare, &records[1])] {
        let prompt = record["prompt"].as_str().unwrap();
        assert_eq!(
            row["source_task_id"],
            synthlite::identity::prompt_task_id(prompt).unwrap()
        );
        assert_eq!(
            row["messages"][0],
            json!({"role": "user", "content": prompt})
        );
        let metadata = &row["metadata"];
        for field in [
            "category",
            "domain",
            "subdomain",
            "difficulty",
            "taskgen_model",
            "language",
        ] {
            assert!(metadata[field].is_null(), "{field}: {row}");
        }
        assert_eq!(metadata["coordinates"], json!({}));
        assert_eq!(metadata["provider"], "openrouter");
        assert_eq!(metadata["model"], "openrouter/free");
        assert_eq!(metadata["served_model"], SERVED);
        assert_eq!(metadata["usage"]["completion_tokens"], 50);
        let mut expected: Vec<&str> = TASKGEN_ROW_KEYS.to_vec();
        expected.extend(["input_id", "input_metadata"]);
        expected.sort_unstable();
        assert_eq!(metadata_keys(row), expected);
    }
    assert_eq!(
        with_id["metadata"]["input_metadata"],
        json!({"team": "network", "priority": 2})
    );
    assert!(bare["metadata"]["input_metadata"].is_null());
    let replies: Vec<&str> = rows
        .iter()
        .map(|row| row["messages"][1]["content"].as_str().unwrap())
        .collect();
    assert!(replies.contains(&first.trim()) && replies.contains(&second.trim()));
    for body in bodies.lock().unwrap().iter() {
        let request: Value = serde_json::from_str(body).unwrap();
        assert_eq!(request["model"], "openrouter/free");
    }

    // The rows gate like any other run.
    let gate = common::run(dir.path(), &["gate"], &[], Duration::from_secs(10));
    assert_eq!(gate.code, 0, "{}", gate.stderr);
    let train = fs::read_to_string(dir.path().join("out/data/train.jsonl")).unwrap();
    let validation =
        fs::read_to_string(dir.path().join("out/data/validation.jsonl")).unwrap_or_default();
    assert_eq!(train.lines().count() + validation.lines().count(), 2);
}

#[tokio::test]
async fn a_txt_line_and_its_prompt_record_are_the_same_seed() {
    let server = mock_reply(&common::long_reply("txt"), 2).await;
    let uri = server.uri();
    let txt = tempfile::tempdir().unwrap();
    fs::write(
        txt.path().join("prompts.txt"),
        "\n   Check the UPS alarms before the maintenance window.  \n\n",
    )
    .unwrap();
    let run = common::run(
        txt.path(),
        &["prompts.txt"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let jsonl = tempfile::tempdir().unwrap();
    fs::write(
        jsonl.path().join("prompts.jsonl"),
        format!(
            "{}\n",
            json!({"prompt": "Check the UPS alarms before the maintenance window."})
        ),
    )
    .unwrap();
    let run = common::run(
        jsonl.path(),
        &["prompts.jsonl"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    let from_txt = rows(txt.path());
    let from_jsonl = rows(jsonl.path());
    assert_eq!(from_txt.len(), 1);
    let row = &from_txt[0];
    assert_eq!(
        row["messages"][0]["content"],
        "Check the UPS alarms before the maintenance window."
    );
    assert!(row["metadata"]["input_id"].is_null(), "{row}");
    assert!(row["metadata"]["input_metadata"].is_null(), "{row}");
    assert!(row["metadata"]["category"].is_null(), "{row}");
    // A JSON (non-streamed) reply names its model in the body.
    assert_eq!(row["metadata"]["served_model"], "teacher-test");
    assert_eq!(row["source_task_id"], from_jsonl[0]["source_task_id"]);
    assert_eq!(
        read_state(txt.path())["source_population_sha256"],
        read_state(jsonl.path())["source_population_sha256"]
    );
    server.verify().await;
}

#[tokio::test]
async fn taskgen_rows_keep_their_metadata_and_gain_served_model() {
    let server = MockServer::start().await;
    let mut body = stop_body(&common::long_reply("taskgen"));
    body["model"] = json!("teacher-test-2024-07-18");
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let mut task = common::fixture_task();
    task["prompt"] = json!("Why are BGP paths stale?");
    task["language"] = json!("en");
    fs::write(dir.path().join("tasks.jsonl"), format!("{task}\n")).unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);

    let rows = rows(dir.path());
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(metadata_keys(row), TASKGEN_ROW_KEYS);
    let metadata = &row["metadata"];
    for field in [
        "category",
        "domain",
        "subdomain",
        "difficulty",
        "coordinates",
        "language",
        "taskgen_model",
    ] {
        assert_eq!(metadata[field], task[field], "{field}");
    }
    assert_eq!(metadata["model"], "teacher-test");
    assert_eq!(metadata["served_model"], "teacher-test-2024-07-18");
    assert_eq!(
        row["source_task_id"],
        synthlite::identity::source_task_id(&task).unwrap()
    );
    assert_eq!(
        read_state(dir.path())["source_population_sha256"],
        synthlite::identity::source_population_sha256(&[task]).unwrap()
    );
    server.verify().await;
}

#[tokio::test]
async fn detailed_prompt_records_render_a_trace() {
    let server = mock_reply(TRACE, 1).await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("prompts.jsonl"),
        format!(
            "{}\n",
            json!({"prompt": "Why are BGP paths stale?", "id": "bgp-1", "metadata": {"site": "dc-1"}})
        ),
    )
    .unwrap();
    let uri = server.uri();
    let run = common::run(
        dir.path(),
        &["prompts.jsonl", "--detailed"],
        &env(&uri),
        Duration::from_secs(20),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let rows = rows(dir.path());
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        row["messages"][0],
        json!({"role": "system", "content": DEFAULT_SYSTEM})
    );
    assert_eq!(row["messages"][1]["content"], "Why are BGP paths stale?");
    let assistant = row["messages"][2]["content"].as_str().unwrap();
    assert!(
        assistant
            .starts_with("## Decision trace\n1. Evidence: The prompt reports stale BGP paths.\n"),
        "{assistant}"
    );
    assert!(assistant.contains("\n\n## Final answer\n"), "{assistant}");
    assert_eq!(row["metadata"]["input_id"], "bgp-1");
    assert_eq!(row["metadata"]["input_metadata"], json!({"site": "dc-1"}));
    assert_eq!(row["metadata"]["served_model"], "teacher-test");
    assert_eq!(read_state(dir.path())["generator_config"]["detailed"], true);
    server.verify().await;
}

#[tokio::test]
async fn resume_under_a_changed_generation_config_keeps_history_and_gates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("first seed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("one"))),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("second seed"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("second seed"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("two"))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("first seed"),
            common::task_line("second seed")
        ),
    )
    .unwrap();
    let toml = |cap: u32| {
        format!(
            "[generation]\nmax_output_tokens = {cap}\n{}",
            key_toml(&server.uri(), "max_attempts = 2\nmax_concurrent = 1")
        )
    };
    fs::write(dir.path().join("a.toml"), toml(4096)).unwrap();
    fs::write(dir.path().join("b.toml"), toml(8192)).unwrap();
    let key_env = [("OPENAI_API_KEY", "sk-test")];
    let first = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--config", "a.toml"],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(first.code, 4, "{}", first.stderr);
    let old_hash = read_state(dir.path())["generator_config_hash"].clone();

    let second = common::run(
        dir.path(),
        &[
            "tasks.jsonl",
            "--out",
            "out",
            "--config",
            "b.toml",
            "--retry-failed",
        ],
        &key_env,
        Duration::from_secs(20),
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert!(second.stderr.contains("config_change"), "{}", second.stderr);
    assert!(
        second.stderr.contains("max_output_tokens: 4096 -> 8192"),
        "{}",
        second.stderr
    );
    let state = read_state(dir.path());
    assert_eq!(state["generation_complete"], true);
    assert_ne!(state["generator_config_hash"], old_hash);
    assert_eq!(
        state["config_history"][0]["generator_config_hash"],
        old_hash
    );
    let rows = fs::read_to_string(dir.path().join("out/rows.jsonl")).unwrap();
    let hashes: std::collections::HashSet<_> = rows
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["generator_config_hash"].to_string()
        })
        .collect();
    assert_eq!(
        hashes.len(),
        2,
        "each row keeps the hash it was written under"
    );

    let gate = common::run(
        dir.path(),
        &["gate", "--out", "out"],
        &[],
        Duration::from_secs(10),
    );
    assert_eq!(gate.code, 0, "{}", gate.stderr);
    server.verify().await;
}

#[tokio::test]
async fn retry_until_finish_completes_after_transient_failures() {
    let server = MockServer::start().await;
    // Two attempts per pass: passes 1 and 2 fail, the third pass succeeds.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(4)
        .expect(4)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(stop_body(&common::long_reply("until finish"))),
        )
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("until finish seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_attempts = 2\nmax_concurrent = 1"),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--retry-until-finish"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("SYNTHLITE_RETRY_ROUND_PAUSE_SECS", "0"),
        ],
        Duration::from_secs(60),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stderr.contains("retry_round 1/3"), "{}", run.stderr);
    assert!(run.stderr.contains("retry_round 2/3"), "{}", run.stderr);
    assert!(!run.stderr.contains("failure_summary"), "{}", run.stderr);
    assert_eq!(read_state(dir.path())["generation_complete"], true);
    server.verify().await;
}

#[tokio::test]
async fn retry_until_finish_stops_after_three_rounds_and_says_why() {
    let server = MockServer::start().await;
    // One initial pass plus three retry rounds, two attempts each.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(503))
        .expect(8)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!("{}\n", common::task_line("never finishes seed")),
    )
    .unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        key_toml(&server.uri(), "max_attempts = 2\nmax_concurrent = 1"),
    )
    .unwrap();
    let run = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out", "--retry-until-finish"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("SYNTHLITE_RETRY_ROUND_PAUSE_SECS", "0"),
        ],
        Duration::from_secs(60),
    );
    assert_eq!(run.code, 4, "{}", run.stderr);
    assert!(run.stderr.contains("retry_round 3/3"), "{}", run.stderr);
    assert!(!run.stderr.contains("retry_round 4/3"), "{}", run.stderr);
    assert!(
        run.stderr.contains("failure_summary still_failed=1"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("failure class=retries_exhausted status=503"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr.contains("rerun with --retry-failed"),
        "{}",
        run.stderr
    );
    server.verify().await;
}
