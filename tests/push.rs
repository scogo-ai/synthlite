mod common;

use std::fs;
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, header, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "hf_test_token_should_not_leak";
const REPO: &str = "test-org/synthlite-set";

fn stop_body(text: &str) -> Value {
    json!({
        "model": "teacher-test",
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 6}
    })
}

#[tokio::test]
async fn push_creates_a_private_repo_and_the_second_push_is_a_noop() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("alpha-seed-one"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("alpha"))),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_string_contains("beta-seed-two"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply("beta"))),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        "[gate]\nvalidation_per_mille = 0\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("alpha-seed-one investigates the leak"),
            common::task_line("beta-seed-two checks the neighbor")
        ),
    )
    .unwrap();
    let uri = server.uri();
    let generated = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", &uri),
        ],
        Duration::from_secs(20),
    );
    assert_eq!(generated.code, 0, "{}", generated.stderr);
    assert!(generated.stderr.contains(" start "), "{}", generated.stderr);
    let after_generate = server.received_requests().await.unwrap();
    assert!(
        after_generate
            .iter()
            .all(|req| req.url.path() == "/chat/completions"),
        "generate called the hub"
    );

    Mock::given(method("GET"))
        .and(path(format!("/api/datasets/{REPO}")))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/datasets/{REPO}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"private": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"url": "ok"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/preupload/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {"path": "data/train.jsonl", "uploadMode": "regular"},
                {"path": "README.md", "uploadMode": "lfs"},
                {"path": "manifest.json", "uploadMode": "regular"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let upload = format!("{uri}/lfs-put");
    let verify_url = format!("{uri}/lfs-verify");
    Mock::given(method("POST"))
        .and(path(format!("/datasets/{REPO}.git/info/lfs/objects/batch")))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let oid = body["objects"][0]["oid"].as_str().unwrap().to_string();
            let size = body["objects"][0]["size"].as_u64().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "objects": [{
                    "oid": oid,
                    "size": size,
                    "actions": {
                        "upload": {"href": upload.clone(), "header": {}},
                        "verify": {"href": verify_url.clone(), "header": {}}
                    }
                }]
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/lfs-put"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/lfs-verify"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/commit/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "commitOid": "abc123deadbeef",
            "commitUrl": "http://example.test/commit/abc123deadbeef"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/tag/abc123deadbeef")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/paths-info/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"path": "data/train.jsonl", "type": "file", "size": 1},
            {"path": "README.md", "type": "file", "size": 1},
            {"path": "manifest.json", "type": "file", "size": 1}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let hub_env = [
        ("HF_TOKEN", TOKEN),
        ("HF_ENDPOINT", uri.as_str()),
        ("OPENAI_API_KEY", "sk-test"),
        ("OPENAI_MODEL", "teacher-test"),
    ];
    let pushed = common::run(
        dir.path(),
        &["push", "--hf-repo", REPO, "--out", "out"],
        &hub_env,
        Duration::from_secs(30),
    );
    assert_eq!(pushed.code, 0, "{}", pushed.stderr);
    assert!(
        pushed.stderr.contains("commit=abc123deadbeef"),
        "{}",
        pushed.stderr
    );
    assert!(dir.path().join("out/data/train.jsonl").is_file());
    assert!(!dir.path().join("out/data/validation.jsonl").exists());

    let again = common::run(
        dir.path(),
        &["push", "--hf-repo", REPO, "--out", "out"],
        &hub_env,
        Duration::from_secs(30),
    );
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert!(again.stderr.contains("unchanged"), "{}", again.stderr);
    server.verify().await;

    let requests = server.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|req| req.url.path() == "/api/repos/create")
        .unwrap();
    let create_body = String::from_utf8(create.body.clone()).unwrap();
    assert!(create_body.contains("\"private\":true"), "{create_body}");
    assert!(create_body.contains("test-org"), "{create_body}");
    let commit = requests
        .iter()
        .find(|req| req.url.path().ends_with("/commit/main"))
        .unwrap();
    let commit_body = String::from_utf8(commit.body.clone()).unwrap();
    assert!(commit_body.contains("data/train.jsonl"), "{commit_body}");
    assert!(commit_body.contains("README.md"));
    assert!(commit_body.contains("manifest.json"));
    assert!(commit_body.contains("\"key\":\"lfsFile\""), "{commit_body}");
    assert!(!commit_body.contains(TOKEN), "{commit_body}");
    assert!(!create_body.contains(TOKEN));
    for relative in [
        "out/README.md",
        "out/manifest.json",
        "out/data/train.jsonl",
        "out/publish.json",
    ] {
        let text = fs::read_to_string(dir.path().join(relative)).unwrap();
        assert!(!text.contains(TOKEN), "{relative} contains the token");
    }
    let put = requests
        .iter()
        .find(|req| req.url.path() == "/lfs-put")
        .unwrap();
    let put_body = String::from_utf8_lossy(&put.body);
    assert!(!put_body.contains(TOKEN));
    let auth = put
        .headers
        .get("authorization")
        .map(|value| value.to_str().unwrap_or(""))
        .unwrap_or("");
    assert!(!auth.contains(TOKEN));
}

#[tokio::test]
async fn push_refuses_when_generation_is_incomplete() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    fs::create_dir_all(&out).unwrap();
    fs::write(
        out.join("state.json"),
        serde_json::to_string(&json!({
            "schema_version": "synthlite.state.v1",
            "synthlite_version": "0.1.0",
            "generator_config_hash": "gen_test",
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
        &["push", "--hf-repo", REPO, "--out", "out"],
        &[("HF_TOKEN", TOKEN), ("HF_ENDPOINT", &server.uri())],
        Duration::from_secs(10),
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(run.stderr.contains("generation_complete"), "{}", run.stderr);
    assert!(server.received_requests().await.unwrap().is_empty());
}

async fn generated_dir(server: &MockServer) -> tempfile::TempDir {
    for tag in ["alpha-seed-one", "beta-seed-two"] {
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains(tag))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(stop_body(&common::long_reply(tag))),
            )
            .mount(server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("synthlite.toml"),
        "[gate]\nvalidation_per_mille = 0\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("tasks.jsonl"),
        format!(
            "{}\n{}\n",
            common::task_line("alpha-seed-one investigates the leak"),
            common::task_line("beta-seed-two checks the neighbor")
        ),
    )
    .unwrap();
    let uri = server.uri();
    let generated = common::run(
        dir.path(),
        &["tasks.jsonl", "--out", "out"],
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_MODEL", "teacher-test"),
            ("OPENAI_BASE_URL", &uri),
        ],
        Duration::from_secs(20),
    );
    assert_eq!(generated.code, 0, "{}", generated.stderr);
    server.reset().await;
    dir
}

/// Mounts an existing private repo plus the upload path. `upload_href`
/// overrides the LFS PUT target; `tag_status` is the tag response code.
async fn mount_hub(
    server: &MockServer,
    commit_oid: &str,
    tag_status: u16,
    upload_href: Option<String>,
) {
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path(format!("/api/datasets/{REPO}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"private": true})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/preupload/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "files": [
                {"path": "data/train.jsonl", "uploadMode": "lfs"},
                {"path": "README.md", "uploadMode": "regular"},
                {"path": "manifest.json", "uploadMode": "regular"}
            ]
        })))
        .mount(server)
        .await;
    let upload = upload_href.unwrap_or_else(|| format!("{uri}/lfs-put"));
    let verify_url = format!("{uri}/lfs-verify");
    Mock::given(method("POST"))
        .and(path(format!("/datasets/{REPO}.git/info/lfs/objects/batch")))
        .respond_with(move |request: &Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "objects": [{
                    "oid": body["objects"][0]["oid"],
                    "size": body["objects"][0]["size"],
                    "actions": {
                        "upload": {"href": upload.clone(), "header": {}},
                        "verify": {"href": verify_url.clone(), "header": {}}
                    }
                }]
            }))
        })
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/lfs-put"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/lfs-verify"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/commit/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"commitOid": commit_oid})))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(format!("^/api/datasets/{REPO}/tag/[^/]+$")))
        .respond_with(ResponseTemplate::new(tag_status))
        .mount(server)
        .await;
}

async fn mount_paths_present(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/paths-info/main")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"path": "data/train.jsonl", "type": "file"},
            {"path": "README.md", "type": "file"},
            {"path": "manifest.json", "type": "file"}
        ])))
        .mount(server)
        .await;
}

fn push(dir: &std::path::Path, uri: &str) -> common::Run {
    common::run(
        dir,
        &["push", "--hf-repo", REPO, "--out", "out"],
        &[("HF_TOKEN", TOKEN), ("HF_ENDPOINT", uri)],
        Duration::from_secs(30),
    )
}

async fn hits(server: &MockServer, method_name: &str, suffix: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|req| req.method.as_str() == method_name && req.url.path().ends_with(suffix))
        .count()
}

fn receipt(dir: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("out/publish.json")).unwrap()).unwrap()
}

#[tokio::test]
async fn push_recreates_a_deleted_repo_despite_a_matching_receipt() {
    let server = MockServer::start().await;
    let dir = generated_dir(&server).await;
    mount_hub(&server, "first0001", 200, None).await;
    let first = push(dir.path(), &server.uri());
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(receipt(dir.path())["commit_oid"], "first0001");

    server.reset().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/datasets/{REPO}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/repos/create"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"url": "ok"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/datasets/{REPO}/paths-info/main")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    mount_hub(&server, "second0002", 200, None).await;
    let again = push(dir.path(), &server.uri());
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(hits(&server, "POST", "/api/repos/create").await, 1);
    assert_eq!(hits(&server, "POST", "/commit/main").await, 1);
    assert_eq!(receipt(dir.path())["commit_oid"], "second0002");
}

#[tokio::test]
async fn push_after_a_failed_tag_only_retries_the_tag() {
    let server = MockServer::start().await;
    let dir = generated_dir(&server).await;
    mount_hub(&server, "landed0001", 502, None).await;
    let first = push(dir.path(), &server.uri());
    assert_eq!(first.code, 1, "{}", first.stderr);
    assert!(first.stderr.contains("tag"), "{}", first.stderr);
    let pending = receipt(dir.path());
    assert_eq!(pending["commit_oid"], "landed0001");
    assert!(pending["tag"].is_null(), "{pending}");

    server.reset().await;
    mount_hub(&server, "duplicate0002", 200, None).await;
    mount_paths_present(&server).await;
    let again = push(dir.path(), &server.uri());
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert!(
        again.stderr.contains("commit=landed0001"),
        "{}",
        again.stderr
    );
    assert_eq!(hits(&server, "POST", "/commit/main").await, 0);
    assert_eq!(hits(&server, "POST", "/preupload/main").await, 0);
    // The retried tag targets the landed commit, not whatever main is now.
    assert_eq!(hits(&server, "POST", "/tag/landed0001").await, 1);
    assert_eq!(hits(&server, "POST", "/tag/main").await, 0);
    let done = receipt(dir.path());
    assert_eq!(done["commit_oid"], "landed0001");
    assert!(
        done["tag"].as_str().unwrap().starts_with("synthlite-"),
        "{done}"
    );

    let third = push(dir.path(), &server.uri());
    assert_eq!(third.code, 0, "{}", third.stderr);
    assert!(third.stderr.contains("unchanged"), "{}", third.stderr);
    assert_eq!(hits(&server, "POST", "/tag/landed0001").await, 1);
}

#[tokio::test]
async fn push_ignores_stale_errors_for_seeds_dropped_from_the_input() {
    let server = MockServer::start().await;
    let dir = generated_dir(&server).await;
    let errors = dir.path().join("out/rows.errors.jsonl");
    let mut text = fs::read_to_string(&errors).unwrap_or_default();
    text.push_str("{\"source_task_id\":\"dropped-seed\",\"attempt\":1,\"class\":\"truncated\"}\n");
    fs::write(&errors, text).unwrap();
    mount_hub(&server, "clean0001", 200, None).await;
    let pushed = push(dir.path(), &server.uri());
    assert_eq!(pushed.code, 0, "{}", pushed.stderr);
    assert_eq!(receipt(dir.path())["commit_oid"], "clean0001");
}

#[tokio::test]
async fn transport_errors_do_not_print_presigned_urls() {
    let server = MockServer::start().await;
    let dir = generated_dir(&server).await;
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let href = format!("http://127.0.0.1:{port}/put?X-Amz-Signature=presignedsecret");
    mount_hub(&server, "never", 200, Some(href)).await;
    let pushed = push(dir.path(), &server.uri());
    assert_eq!(pushed.code, 1, "{}", pushed.stderr);
    assert!(
        !pushed.stderr.contains("presignedsecret"),
        "{}",
        pushed.stderr
    );
    assert!(!pushed.stderr.contains("X-Amz"), "{}", pushed.stderr);
    assert!(!pushed.stderr.contains(TOKEN), "{}", pushed.stderr);
}
