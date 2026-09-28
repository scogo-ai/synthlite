use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};

use crate::config::{KeyConfig, DEFAULT_MAX_OUTPUT_TOKENS, IMPLICIT_OUTPUT_BUDGETS};
use crate::timefmt::parse_http_date;
use crate::trace;

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

pub struct ChatSuccess {
    pub text: String,
    pub usage: Option<TokenUsage>,
    /// The `model` the provider reported (the first streamed chunk that has
    /// one, or the JSON body), if any. A router such as OpenRouter's
    /// `openrouter/free` names the model that actually served the request.
    pub served_model: Option<String>,
}

pub struct ChatResult {
    pub attempt: Attempt,
    pub usage: Option<TokenUsage>,
    /// A 2xx reply can consume tokens even when its usage report is missing.
    pub usage_expected: bool,
}

impl ChatResult {
    fn without_usage(attempt: Attempt, usage_expected: bool) -> Self {
        Self {
            attempt,
            usage: None,
            usage_expected,
        }
    }

    /// Classifies a complete reply. In `--detailed` runs an accepted reply
    /// must hold a decision trace: it is replaced by the rendered trace, or
    /// becomes a permanent `invalid_trace` (no reply text is kept).
    fn from_body(body: &Value, key: &KeyConfig, generation: &Value) -> Self {
        let mut attempt = classify_success(body, key);
        if generation.get("detailed") == Some(&Value::Bool(true)) {
            if let Attempt::Success(success) = &mut attempt {
                match trace::parse(&success.text) {
                    Some(parsed) => success.text = trace::render(&parsed),
                    None => {
                        attempt = Attempt::Permanent {
                            class: "invalid_trace",
                            http_status: Some(200),
                        }
                    }
                }
            }
        }
        Self {
            attempt,
            usage: usage_of(body.get("usage")),
            usage_expected: true,
        }
    }
}

pub enum Attempt {
    Success(ChatSuccess),
    Permanent {
        class: &'static str,
        http_status: Option<u16>,
    },
    /// `reason` is a short label for progress output: `timeout`, `connect`,
    /// `transport`, `bad_body`, or `http_<status>`.
    Retry {
        http_status: Option<u16>,
        reason: String,
    },
    RateLimit {
        retry_after: Option<Duration>,
    },
    Unauthorized {
        http_status: u16,
    },
    /// HTTP 400 while the implicit output budget was sent: the budget may be
    /// above this provider's output limit, so the caller steps it down.
    BudgetRejected,
}

pub fn max_output_tokens_explicit(generation: &Value) -> bool {
    generation
        .get("max_output_tokens")
        .is_some_and(|value| !value.is_null())
}

pub fn resolve_max_output_tokens(generation: &Value, override_budget: Option<u64>) -> u64 {
    if let Some(budget) = override_budget {
        return budget;
    }
    match generation.get("max_output_tokens") {
        Some(Value::Number(number)) => number
            .as_u64()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        _ => DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

/// The next implicit budget below `current`, if any.
pub fn next_lower_budget(current: u64) -> Option<u64> {
    IMPLICIT_OUTPUT_BUDGETS
        .into_iter()
        .find(|budget| *budget < current)
}

/// Hashed generation fields sent to the provider as-is. Every other hashed
/// field (`schema_version`, `system_message`, `max_output_tokens`,
/// `detailed`, `teacher_instruction`) is never a body key.
const SAMPLING_FIELDS: [&str; 7] = [
    "temperature",
    "top_p",
    "seed",
    "frequency_penalty",
    "presence_penalty",
    "stop",
    "reasoning_effort",
];

pub fn request_body(
    key: &KeyConfig,
    generation: &Value,
    prompt: &str,
    max_output_tokens_override: Option<u64>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), Value::String(key.model.clone()));
    body.insert("messages".into(), messages(generation, prompt));
    if let Some(object) = generation.as_object() {
        for field in SAMPLING_FIELDS {
            if let Some(value) = object.get(field).filter(|value| !value.is_null()) {
                body.insert(field.into(), value.clone());
            }
        }
        let max_tokens = resolve_max_output_tokens(generation, max_output_tokens_override);
        body.insert(
            key.max_tokens_field.clone(),
            Value::Number(serde_json::Number::from(max_tokens)),
        );
    }
    body.insert("stream".into(), Value::Bool(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    Value::Object(body)
}

pub fn messages(generation: &Value, prompt: &str) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = generation
        .get("system_message")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        // `--detailed`: the teacher contract rides on the outbound system
        // message only; `encode_row` stores `system_message` alone.
        let content = match generation
            .get("teacher_instruction")
            .and_then(Value::as_str)
        {
            Some(instruction) => format!("{system}\n\n{instruction}"),
            None => system.to_string(),
        };
        messages.push(json!({"role": "system", "content": content}));
    }
    messages.push(json!({"role": "user", "content": prompt}));
    Value::Array(messages)
}

pub fn classify_success(body: &Value, key: &KeyConfig) -> Attempt {
    let Some(choice) = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    else {
        return Attempt::Retry {
            http_status: Some(200),
            reason: "bad_body".into(),
        };
    };
    match choice.get("finish_reason") {
        None | Some(Value::Null) => {
            if key.require_finish_reason {
                return Attempt::Permanent {
                    class: "no_finish_reason",
                    http_status: Some(200),
                };
            }
        }
        Some(Value::String(reason)) if reason == "stop" => {}
        Some(Value::String(reason)) if reason == "length" => {
            return Attempt::Permanent {
                class: "truncated",
                http_status: Some(200),
            };
        }
        Some(Value::String(reason)) if reason == "content_filter" => {
            return Attempt::Permanent {
                class: "content_filter",
                http_status: Some(200),
            };
        }
        Some(_) => {
            return Attempt::Permanent {
                class: "unexpected_finish_reason",
                http_status: Some(200),
            };
        }
    }
    let message = choice.get("message");
    if message
        .and_then(|m| m.get("refusal"))
        .is_some_and(|refusal| !refusal.is_null())
    {
        return Attempt::Permanent {
            class: "refusal",
            http_status: Some(200),
        };
    }
    let text = match message.and_then(|m| m.get("content")) {
        Some(Value::String(content)) => content.trim().to_string(),
        _ => {
            return Attempt::Permanent {
                class: "empty_assistant",
                http_status: Some(200),
            };
        }
    };
    if text.is_empty() {
        return Attempt::Permanent {
            class: "empty_assistant",
            http_status: Some(200),
        };
    }
    let served_model = body.get("model").and_then(Value::as_str);
    if key.require_model_match {
        if let Some(model) = served_model {
            if !model_matches(&key.model, model) {
                return Attempt::Permanent {
                    class: "model_mismatch",
                    http_status: Some(200),
                };
            }
        }
    }
    Attempt::Success(ChatSuccess {
        text,
        usage: usage_of(body.get("usage")),
        served_model: served_model
            .filter(|model| !model.is_empty())
            .map(str::to_string),
    })
}

/// Exact match, or a dated snapshot of the requested alias:
/// `<requested>-YYYY-MM-DD` or `<requested>-NNNN`.
fn model_matches(requested: &str, returned: &str) -> bool {
    if returned == requested {
        return true;
    }
    let Some(suffix) = returned
        .strip_prefix(requested)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let digits =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_digit());
    match suffix.split('-').collect::<Vec<_>>().as_slice() {
        [short] => digits(short, 4),
        [year, month, day] => digits(year, 4) && digits(month, 2) && digits(day, 2),
        _ => false,
    }
}

fn usage_of(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value.filter(|v| v.is_object())?;
    Some(TokenUsage {
        prompt_tokens: json_i64(value.get("prompt_tokens")).filter(|n| *n >= 0)?,
        completion_tokens: json_i64(value.get("completion_tokens")).filter(|n| *n >= 0)?,
    })
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if let Some(number) = value.as_i64() {
        return Some(number);
    }
    value.as_u64().and_then(|n| i64::try_from(n).ok())
}

/// Streams one completion. `timeout_seconds` is an idle timeout: the request
/// fails only when no bytes arrive for that long, so a long reply that keeps
/// streaming is never cut. `live_tokens` is bumped once per content or
/// reasoning delta as it arrives, for progress output.
pub async fn chat(
    client: &reqwest::Client,
    key: &KeyConfig,
    generation: &Value,
    prompt: &str,
    max_output_tokens_override: Option<u64>,
    live_tokens: &AtomicU64,
) -> ChatResult {
    let url = format!("{}/chat/completions", key.base_url.trim_end_matches('/'));
    let idle = Duration::from_secs(key.timeout_seconds);
    let request = client
        .post(url)
        .bearer_auth(&key.api_key)
        .json(&request_body(
            key,
            generation,
            prompt,
            max_output_tokens_override,
        ))
        .send();
    let response = match tokio::time::timeout(idle, request).await {
        Err(_) => {
            return ChatResult::without_usage(
                Attempt::Retry {
                    http_status: None,
                    reason: "timeout".into(),
                },
                false,
            )
        }
        Ok(Err(err)) => {
            return ChatResult::without_usage(
                Attempt::Retry {
                    http_status: None,
                    reason: transport_reason(&err).into(),
                },
                false,
            )
        }
        Ok(Ok(response)) => response,
    };
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after);
        return ChatResult::without_usage(Attempt::RateLimit { retry_after }, false);
    }
    if status == 401 || status == 403 {
        return ChatResult::without_usage(
            Attempt::Unauthorized {
                http_status: status,
            },
            false,
        );
    }
    if matches!(status, 408 | 425 | 500 | 502 | 503 | 504) || (500..600).contains(&status) {
        return ChatResult::without_usage(
            Attempt::Retry {
                http_status: Some(status),
                reason: format!("http_{status}"),
            },
            false,
        );
    }
    if status == 400 && !max_output_tokens_explicit(generation) {
        return ChatResult::without_usage(Attempt::BudgetRejected, false);
    }
    if !(200..300).contains(&status) {
        return ChatResult::without_usage(
            Attempt::Permanent {
                class: "invalid_request",
                http_status: Some(status),
            },
            false,
        );
    }
    let is_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_stream {
        // A server that ignores `stream` answers with one JSON body.
        return match tokio::time::timeout(idle, response.json::<Value>()).await {
            Err(_) => ChatResult::without_usage(
                Attempt::Retry {
                    http_status: Some(status),
                    reason: "timeout".into(),
                },
                true,
            ),
            Ok(Err(_)) => ChatResult::without_usage(
                Attempt::Retry {
                    http_status: Some(status),
                    reason: "bad_body".into(),
                },
                true,
            ),
            Ok(Ok(body)) => ChatResult::from_body(&body, key, generation),
        };
    }
    let mut events = response.bytes_stream().eventsource();
    let mut reply = StreamedReply::default();
    let mut done = false;
    loop {
        let event = match tokio::time::timeout(idle, events.next()).await {
            Err(_) => {
                return ChatResult {
                    attempt: Attempt::Retry {
                        http_status: Some(status),
                        reason: "timeout".into(),
                    },
                    usage: usage_of(reply.usage.as_ref()),
                    usage_expected: true,
                }
            }
            Ok(None) => break,
            Ok(Some(Err(_))) => {
                return ChatResult {
                    attempt: Attempt::Retry {
                        http_status: Some(status),
                        reason: "stream_cut".into(),
                    },
                    usage: usage_of(reply.usage.as_ref()),
                    usage_expected: true,
                }
            }
            Ok(Some(Ok(event))) => event,
        };
        let data = event.data.trim();
        if data == "[DONE]" {
            done = true;
            break;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return ChatResult {
                attempt: Attempt::Retry {
                    http_status: Some(status),
                    reason: "bad_body".into(),
                },
                usage: usage_of(reply.usage.as_ref()),
                usage_expected: true,
            };
        };
        if chunk.get("error").is_some_and(|error| !error.is_null()) {
            return ChatResult {
                attempt: Attempt::Retry {
                    http_status: Some(status),
                    reason: "stream_error".into(),
                },
                usage: usage_of(reply.usage.as_ref()),
                usage_expected: true,
            };
        }
        reply.absorb(&chunk, live_tokens);
    }
    if !done && reply.finish_reason.is_none() {
        // The connection closed before the server said why it stopped.
        return ChatResult {
            attempt: Attempt::Retry {
                http_status: Some(status),
                reason: "stream_cut".into(),
            },
            usage: usage_of(reply.usage.as_ref()),
            usage_expected: true,
        };
    }
    ChatResult::from_body(&reply.into_body(), key, generation)
}

/// Deltas of one streamed completion, folded back into the shape of a
/// non-streamed response so `classify_success` applies unchanged.
#[derive(Default)]
struct StreamedReply {
    model: Option<String>,
    content: Option<String>,
    refusal: Option<String>,
    finish_reason: Option<Value>,
    usage: Option<Value>,
    saw_choice: bool,
}

impl StreamedReply {
    fn absorb(&mut self, chunk: &Value, live_tokens: &AtomicU64) {
        // Some providers (Azure's content-filter chunk) send "model":"" first;
        // keep the first chunk that names a model.
        if self.model.is_none() {
            self.model = chunk
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_string);
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        self.saw_choice = true;
        let delta = choice.get("delta");
        let text = |field: &str| {
            delta
                .and_then(|delta| delta.get(field))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        };
        if let Some(piece) = text("content") {
            self.content.get_or_insert_with(String::new).push_str(piece);
            live_tokens.fetch_add(1, Ordering::Relaxed);
        }
        if text("reasoning")
            .or_else(|| text("reasoning_content"))
            .is_some()
        {
            live_tokens.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(piece) = text("refusal") {
            self.refusal.get_or_insert_with(String::new).push_str(piece);
        }
        if let Some(reason) = choice
            .get("finish_reason")
            .filter(|reason| !reason.is_null())
        {
            self.finish_reason = Some(reason.clone());
        }
    }

    fn into_body(self) -> Value {
        let choices = if self.saw_choice {
            json!([{
                "message": {"content": self.content, "refusal": self.refusal},
                "finish_reason": self.finish_reason,
            }])
        } else {
            json!([])
        };
        json!({"model": self.model, "choices": choices, "usage": self.usage})
    }
}

fn transport_reason(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else {
        "transport"
    }
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = parse_http_date(value)?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::from_secs(0)),
    )
}

pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_model_in_the_first_chunk_does_not_hide_the_served_model() {
        let live = AtomicU64::new(0);
        let mut reply = StreamedReply::default();
        reply.absorb(
            &json!({"model": "", "choices": [], "prompt_filter_results": []}),
            &live,
        );
        reply.absorb(
            &json!({"model": "gpt-4o-mini-2024-07-18", "choices": [{"delta": {"content": "hi"}}]}),
            &live,
        );
        assert_eq!(reply.into_body()["model"], "gpt-4o-mini-2024-07-18");
    }
    use serde_json::json;

    fn key() -> KeyConfig {
        KeyConfig {
            id: "openai-1".into(),
            display_id: "openai-1".into(),
            provider: "openai".into(),
            api_key: "secret".into(),
            base_url: "https://example.test/v1".into(),
            base_url_display: "example.test".into(),
            model: "teacher".into(),
            max_concurrent: 4,
            requests_per_minute: None,
            timeout_seconds: 5,
            max_attempts: 2,
            rate_limit_cooldown_seconds: 0,
            headerless_429_limit: 3,
            max_tokens_field: "max_tokens".into(),
            require_model_match: true,
            require_finish_reason: true,
        }
    }

    #[test]
    fn length_is_not_a_row() {
        let body = json!({
            "model": "teacher",
            "choices": [{"message": {"content": "partial"}, "finish_reason": "length"}]
        });
        match classify_success(&body, &key()) {
            Attempt::Permanent { class, .. } => assert_eq!(class, "truncated"),
            _ => panic!("expected truncated"),
        }
    }

    #[test]
    fn request_omits_nulls_and_renames_max_tokens() {
        let generation = json!({
            "schema_version": "synthlite.generator-config.v1",
            "system_message": "be brief",
            "temperature": 1,
            "top_p": null,
            "max_output_tokens": 32,
            "seed": null,
            "frequency_penalty": null,
            "presence_penalty": null,
            "stop": null,
            "reasoning_effort": null
        });
        let body = request_body(&key(), &generation, "hello", None);
        assert!(body.get("top_p").is_none());
        assert!(body.get("system_message").is_none());
        assert_eq!(body.get("max_tokens").and_then(Value::as_i64), Some(32));
        assert_eq!(body["messages"][0]["content"], "be brief");
        assert_eq!(body["messages"][1]["content"], "hello");
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "max_tokens",
                "messages",
                "model",
                "stream",
                "stream_options",
                "temperature"
            ]
        );
    }

    fn detailed_generation() -> Value {
        json!({
            "schema_version": "synthlite.generator-config.v1",
            "detailed": true,
            "system_message": crate::trace::DEFAULT_SYSTEM_MESSAGE,
            "teacher_instruction": crate::trace::TEACHER_INSTRUCTION,
            "temperature": 0.2,
            "top_p": null,
            "max_output_tokens": null,
            "seed": null,
            "frequency_penalty": null,
            "presence_penalty": null,
            "stop": null,
            "reasoning_effort": "medium"
        })
    }

    #[test]
    fn detailed_body_carries_the_contract_only_in_the_system_message() {
        let body = request_body(&key(), &detailed_generation(), "hello", None);
        for field in [
            "detailed",
            "teacher_instruction",
            "system_message",
            "tools",
            "response_format",
            "n",
        ] {
            assert!(body.get(field).is_none(), "{field}");
        }
        assert_eq!(
            body["messages"][0]["content"],
            format!(
                "{}\n\n{}",
                crate::trace::DEFAULT_SYSTEM_MESSAGE,
                crate::trace::TEACHER_INSTRUCTION
            )
        );
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["reasoning_effort"], "medium");
    }

    #[test]
    fn detailed_success_is_rendered_or_invalid_trace() {
        let generation = detailed_generation();
        let object = r#"{"reasoning_steps":[{"kind":"evidence","content":"e"},{"kind":"hypothesis","content":"h"},{"kind":"action","content":"a"},{"kind":"verification","content":"v"},{"kind":"conclusion","content":"c"}],"final_answer":"do this"}"#;
        let body = |content: &str, finish: &str| {
            json!({
                "model": "teacher",
                "choices": [{"message": {"content": content}, "finish_reason": finish}]
            })
        };
        match ChatResult::from_body(&body(object, "stop"), &key(), &generation).attempt {
            Attempt::Success(success) => {
                assert!(success
                    .text
                    .starts_with("## Decision trace\n1. Evidence: e\n"))
            }
            _ => panic!("expected a rendered trace"),
        }
        assert!(matches!(
            ChatResult::from_body(&body("**Memo:** restart BGP.", "stop"), &key(), &generation)
                .attempt,
            Attempt::Permanent {
                class: "invalid_trace",
                http_status: Some(200)
            }
        ));
        assert!(matches!(
            ChatResult::from_body(&body(object, "length"), &key(), &generation).attempt,
            Attempt::Permanent {
                class: "truncated",
                ..
            }
        ));
    }

    #[test]
    fn implicit_budget_uses_default_max_tokens() {
        let generation = json!({
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
        });
        let body = request_body(&key(), &generation, "hello", None);
        assert_eq!(
            body.get("max_tokens").and_then(Value::as_u64),
            Some(DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }

    #[test]
    fn model_match_accepts_dated_snapshots_only() {
        assert!(model_matches("gpt-4o", "gpt-4o"));
        assert!(model_matches("gpt-4o", "gpt-4o-2024-08-06"));
        assert!(model_matches("gpt-4o-mini", "gpt-4o-mini-2024-07-18"));
        assert!(model_matches("gpt-4", "gpt-4-0613"));
        assert!(!model_matches("gpt-4o", "gpt-4o-mini"));
        assert!(!model_matches("gpt-4o", "gpt-4o-mini-2024-07-18"));
        assert!(!model_matches("gpt-4o", "gpt-4.1"));
        assert!(!model_matches("gpt-4o", "gpt-4o-12345"));
        assert!(!model_matches("gpt-4o-2024-08-06", "gpt-4o"));
    }

    #[test]
    fn dated_snapshot_response_is_a_row() {
        let mut key = key();
        key.model = "gpt-4o-mini".into();
        let body = json!({
            "model": "gpt-4o-mini-2024-07-18",
            "choices": [{"message": {"content": "answer"}, "finish_reason": "stop"}]
        });
        assert!(matches!(classify_success(&body, &key), Attempt::Success(_)));
        let body = json!({
            "model": "gpt-4o-mini-high",
            "choices": [{"message": {"content": "answer"}, "finish_reason": "stop"}]
        });
        assert!(matches!(
            classify_success(&body, &key),
            Attempt::Permanent {
                class: "model_mismatch",
                ..
            }
        ));
    }

    fn served_model(body: &Value, key: &KeyConfig) -> Option<String> {
        match classify_success(body, key) {
            Attempt::Success(success) => success.served_model,
            _ => panic!("expected a row"),
        }
    }

    #[test]
    fn a_router_reply_is_a_row_that_names_the_served_model() {
        // OpenRouter's free router: the reply names the model it picked.
        let mut key = key();
        key.model = "openrouter/free".into();
        key.require_model_match = false;
        let reply = |model: Value| {
            json!({
                "model": model,
                "choices": [{"message": {"content": "answer"}, "finish_reason": "stop"}]
            })
        };
        assert_eq!(
            served_model(
                &reply(json!("meta-llama/llama-3.3-70b-instruct:free")),
                &key
            )
            .as_deref(),
            Some("meta-llama/llama-3.3-70b-instruct:free")
        );
        assert_eq!(served_model(&reply(Value::Null), &key), None);
        assert_eq!(served_model(&reply(json!("")), &key), None);
        key.require_model_match = true;
        assert!(matches!(
            classify_success(
                &reply(json!("meta-llama/llama-3.3-70b-instruct:free")),
                &key
            ),
            Attempt::Permanent {
                class: "model_mismatch",
                ..
            }
        ));
    }

    #[test]
    fn a_stream_keeps_the_first_model_it_names() {
        let live = AtomicU64::new(0);
        let mut reply = StreamedReply::default();
        for chunk in [
            json!({"choices": [{"delta": {"role": "assistant"}}]}),
            json!({"model": "first-model", "choices": [{"delta": {"content": "an"}}]}),
            json!({"model": "second-model", "choices": [{"delta": {"content": "swer"}, "finish_reason": "stop"}]}),
        ] {
            reply.absorb(&chunk, &live);
        }
        let mut key = key();
        key.require_model_match = false;
        let body = reply.into_body();
        assert_eq!(served_model(&body, &key).as_deref(), Some("first-model"));
    }

    #[test]
    fn implicit_budget_ladder_steps_down() {
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 32_768);
        assert_eq!(next_lower_budget(32_768), Some(16_384));
        assert_eq!(next_lower_budget(16_384), Some(8_192));
        assert_eq!(next_lower_budget(8_192), Some(4_096));
        assert_eq!(next_lower_budget(4_096), None);
        assert_eq!(next_lower_budget(10_000), Some(8_192));
    }
}
