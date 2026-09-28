#![allow(dead_code)] // each test binary uses a different subset of these helpers
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_synthlite"))
}

pub fn fixture_task() -> Value {
    serde_json::from_str(include_str!("../fixtures/canonical/valid-task.json")).unwrap()
}

pub fn task_line(prompt: &str) -> String {
    let mut task = fixture_task();
    task["prompt"] = json!(prompt);
    serde_json::to_string(&task).unwrap()
}

pub fn long_reply(tag: &str) -> String {
    let unit = format!("Inspect the evidence for {tag} and write down a bounded next step. ");
    let mut text = String::new();
    while text.chars().count() < 240 {
        text.push_str(&unit);
    }
    text
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

const CLEARED: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_MODEL",
    "OPENAI_BASE_URL",
    "SYNTHLITE_MODEL",
    "SYNTHLITE_SYSTEM_PROMPT",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "HF_ENDPOINT",
];

pub fn run(cwd: &Path, args: &[&str], env: &[(&str, &str)], timeout: Duration) -> Run {
    let mut cmd = Command::new(bin());
    cmd.current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in CLEARED {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let stdout_thread = thread::spawn(move || read_to_string(&mut stdout));
    let stderr_thread = thread::spawn(move || read_to_string(&mut stderr));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let stdout = stdout_thread.join().unwrap_or_default();
                let stderr = stderr_thread.join().unwrap_or_default();
                panic!(
                    "timed out after {}s\nargs: {args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    timeout.as_secs()
                );
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(err) => panic!("wait failed: {err}"),
        }
    };
    Run {
        code: status.code().unwrap_or(-1),
        stdout: stdout_thread.join().unwrap(),
        stderr: stderr_thread.join().unwrap(),
    }
}

fn read_to_string(reader: &mut impl Read) -> String {
    let mut buf = String::new();
    reader.read_to_string(&mut buf).unwrap();
    buf
}

/// One scripted SSE response: `(delay_ms, data)` pairs, each written as
/// `data: <data>\n\n` after sleeping `delay_ms`. A `data` starting with `:`
/// is written as-is, as an SSE comment.
pub type SseScript = Vec<(u64, String)>;

/// A raw HTTP server that streams scripted SSE responses, one script per
/// connection in order (the last script repeats). wiremock can only delay a
/// whole response, so pacing and mid-stream stalls need this. Returns the
/// base URL and the request bodies received so far.
pub fn sse_server(
    scripts: Vec<SseScript>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = bodies.clone();
    thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let script = scripts[index.min(scripts.len() - 1)].clone();
            let seen = seen.clone();
            thread::spawn(move || {
                let body = read_http_request(&mut stream);
                seen.lock().unwrap().push(body);
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                for (delay, data) in script {
                    thread::sleep(Duration::from_millis(delay));
                    // A `:` line is an SSE comment, as OpenRouter sends while it
                    // picks a model.
                    let frame = if data.starts_with(':') {
                        format!("{data}\n\n")
                    } else {
                        format!("data: {data}\n\n")
                    };
                    if stream.write_all(frame.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                }
            });
        }
    });
    (base, bodies)
}

fn read_http_request(stream: &mut std::net::TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            return String::new();
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8_lossy(&buffer[header_end..]).into_owned()
}

/// SSE chunks for one streamed completion: reasoning deltas, content deltas,
/// a finish chunk, a usage chunk, and `[DONE]`, each `delay_ms` apart.
pub fn sse_completion(
    reasoning: &[&str],
    content: &[&str],
    finish_reason: &str,
    completion_tokens: u64,
    delay_ms: u64,
) -> SseScript {
    let mut script = Vec::new();
    let chunk = |delta: Value, finish: Value| {
        json!({
            "model": "teacher-test",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
        })
        .to_string()
    };
    script.push((delay_ms, chunk(json!({"role": "assistant"}), Value::Null)));
    for piece in reasoning {
        script.push((delay_ms, chunk(json!({"reasoning": piece}), Value::Null)));
    }
    for piece in content {
        script.push((delay_ms, chunk(json!({"content": piece}), Value::Null)));
    }
    script.push((delay_ms, chunk(json!({}), json!(finish_reason))));
    script.push((
        0,
        json!({
            "model": "teacher-test",
            "choices": [],
            "usage": {"prompt_tokens": 11, "completion_tokens": completion_tokens}
        })
        .to_string(),
    ));
    script.push((0, "[DONE]".to_string()));
    script
}

pub fn key_config(base_url: &str, extra: &str) -> String {
    format!(
        "[[key]]\nprovider = \"fake\"\nbase_url = \"{base_url}\"\nmodel = \"teacher-test\"\napi_key_env = \"FAKE_KEY\"\n{extra}\n"
    )
}
