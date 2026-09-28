use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::identity::generator_config_hash;
use crate::trace;

/// Implicit output budgets, largest first. With `[generation].max_output_tokens`
/// unset (`null` in the hash), a key starts at the first and steps down when the
/// provider rejects a budget with HTTP 400. Hosted APIs bill tokens used, not
/// the cap, so starting high costs nothing and avoids regenerating long replies.
pub const IMPLICIT_OUTPUT_BUDGETS: [u64; 4] = [32_768, 16_384, 8_192, 4_096];
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = IMPLICIT_OUTPUT_BUDGETS[0];

#[derive(Clone)]
pub struct KeyConfig {
    pub id: String,
    /// `id` with the base URL reduced to `host[:port]`, for stderr.
    pub display_id: String,
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub base_url_display: String,
    pub model: String,
    pub max_concurrent: usize,
    pub requests_per_minute: Option<u32>,
    pub timeout_seconds: u64,
    pub max_attempts: u32,
    pub rate_limit_cooldown_seconds: u64,
    pub headerless_429_limit: u32,
    pub max_tokens_field: String,
    pub require_model_match: bool,
    pub require_finish_reason: bool,
}

impl std::fmt::Debug for KeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyConfig")
            .field("id", &self.id)
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("max_concurrent", &self.max_concurrent)
            .field("requests_per_minute", &self.requests_per_minute)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GateSettings {
    pub min_assistant_chars: usize,
    pub max_assistant_chars: usize,
    pub max_repeated_line: usize,
    pub refusal_window_chars: usize,
    pub refusal_phrases: Vec<String>,
    pub exclude_prompt_hashes: Option<PathBuf>,
    pub validation_per_mille: u32,
}

#[derive(Debug, Clone)]
pub struct CardSettings {
    pub pretty_name: Option<String>,
    pub license: Option<String>,
    pub license_name: Option<String>,
    pub license_link: Option<String>,
    pub language: Vec<String>,
}

pub struct Loaded {
    pub generation: Value,
    pub generator_config_hash: String,
    pub keys: Vec<KeyConfig>,
    pub gate: GateSettings,
    pub card: CardSettings,
}

pub enum LoadMode {
    /// `detailed` is the `--detailed` flag. It turns the mode on; the file
    /// key can too. `gate` and `push` load offline and read the mode from
    /// `state.json`.
    Generate {
        detailed: bool,
    },
    Offline,
}

pub fn load(config_flag: Option<&Path>, mode: LoadMode) -> Result<Loaded> {
    let path = resolve_config_path(config_flag)?;
    let table = match &path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|err| Error::refuse(format!("config {}: {err}", path.display())))?;
            text.parse::<toml::Value>().map_err(|err| {
                // The default Display quotes the offending source line, which may hold a secret.
                let line = err
                    .span()
                    .map(|span| format!(" (line {})", text[..span.start].matches('\n').count() + 1))
                    .unwrap_or_default();
                Error::refuse(format!(
                    "config {}: {}{line}",
                    path.display(),
                    err.message()
                ))
            })?
        }
        None => toml::Value::Table(toml::map::Map::new()),
    };
    let root = table
        .as_table()
        .ok_or_else(|| Error::refuse("config must be a table"))?;
    for key in root.keys() {
        if !matches!(key.as_str(), "generation" | "key" | "gate" | "card") {
            return Err(Error::refuse(format!("unknown config field {key}")));
        }
    }
    let generation = generation_from_file(root.get("generation"), &mode)?;
    let hash = generator_config_hash(&generation)?;
    let gate = gate_from_file(root.get("gate"))?;
    let card = card_from_file(root.get("card"))?;
    let keys = match mode {
        LoadMode::Generate { .. } => keys_from_file(root.get("key"))?,
        LoadMode::Offline => Vec::new(),
    };
    Ok(Loaded {
        generation,
        generator_config_hash: hash,
        keys,
        gate,
        card,
    })
}

pub fn resolve_config_path(flag: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = flag {
        if !path.is_file() {
            return Err(Error::refuse(format!(
                "config {} does not exist",
                path.display()
            )));
        }
        return Ok(Some(path.to_path_buf()));
    }
    let local = PathBuf::from("synthlite.toml");
    if local.is_file() {
        return Ok(Some(local));
    }
    Ok(None)
}

fn generation_from_file(value: Option<&toml::Value>, mode: &LoadMode) -> Result<Value> {
    generation_from(value, mode, std::env::var("SYNTHLITE_SYSTEM_PROMPT").ok())
}

/// `env_system` is `SYNTHLITE_SYSTEM_PROMPT`, passed in so tests do not
/// depend on the caller's environment.
fn generation_from(
    value: Option<&toml::Value>,
    mode: &LoadMode,
    env_system: Option<String>,
) -> Result<Value> {
    let mut detailed_file = false;
    let mut persona = None;
    let mut system_file = None;
    let mut temperature = Value::Null;
    let mut top_p = Value::Null;
    let mut max_output_tokens = Value::Null;
    let mut seed = Value::Null;
    let mut frequency_penalty = Value::Null;
    let mut presence_penalty = Value::Null;
    let mut stop = Value::Null;
    let mut reasoning_effort = Value::Null;
    if let Some(value) = value {
        let table = value
            .as_table()
            .ok_or_else(|| Error::refuse("[generation] must be a table"))?;
        for (key, item) in table {
            match key.as_str() {
                "detailed" => {
                    detailed_file = item
                        .as_bool()
                        .ok_or_else(|| Error::refuse("[generation].detailed must be a boolean"))?;
                }
                "persona" => {
                    // One line, no trailing period: it becomes "You are <persona>. ..."
                    let text = item
                        .as_str()
                        .map(|text| text.trim().trim_end_matches('.').trim_end())
                        .filter(|text| !text.is_empty() && !text.contains(['\n', '\r']))
                        .ok_or_else(|| {
                            Error::refuse(
                                "[generation].persona must be a non-empty, single-line string",
                            )
                        })?;
                    persona = Some(text.to_string());
                }
                "system_message" => {
                    system_file = Some(
                        item.as_str()
                            .ok_or_else(|| {
                                Error::refuse("[generation].system_message must be a string")
                            })?
                            .to_string(),
                    );
                }
                "temperature" => temperature = toml_number(item, "temperature")?,
                "top_p" => top_p = toml_number(item, "top_p")?,
                "frequency_penalty" => frequency_penalty = toml_number(item, "frequency_penalty")?,
                "presence_penalty" => presence_penalty = toml_number(item, "presence_penalty")?,
                "max_output_tokens" => max_output_tokens = toml_integer(item, "max_output_tokens")?,
                "seed" => seed = toml_integer(item, "seed")?,
                "stop" => stop = toml_stop(item)?,
                "reasoning_effort" => {
                    let text = item.as_str().ok_or_else(|| {
                        Error::refuse("[generation].reasoning_effort must be a string")
                    })?;
                    reasoning_effort = if text.trim().is_empty() {
                        Value::Null
                    } else {
                        Value::String(text.to_string())
                    };
                }
                other => {
                    return Err(Error::refuse(format!("unknown [generation] field {other}")));
                }
            }
        }
    }
    for (name, number) in [
        ("temperature", &temperature),
        ("top_p", &top_p),
        ("frequency_penalty", &frequency_penalty),
        ("presence_penalty", &presence_penalty),
    ] {
        if let Some(value) = number.as_f64() {
            if value < 0.0 {
                return Err(Error::refuse(format!("[generation].{name} must be >= 0")));
            }
        }
    }
    if max_output_tokens.as_i64().is_some_and(|value| value < 1) {
        return Err(Error::refuse("[generation].max_output_tokens must be >= 1"));
    }
    let detailed = detailed_file || matches!(mode, LoadMode::Generate { detailed: true });
    let mut system_message = resolve_system_message(env_system, system_file.as_deref());
    // gate and push only need [gate] and [card]; an exported
    // SYNTHLITE_SYSTEM_PROMPT or a flag-only --detailed must not stop them.
    let generating = matches!(mode, LoadMode::Generate { .. });
    if detailed {
        if system_message.is_some() && generating {
            return Err(Error::refuse(
                "--detailed uses a fixed system message; unset SYNTHLITE_SYSTEM_PROMPT and [generation].system_message, and use [generation].persona to name the assistant",
            ));
        }
        // The persona lives in the hashed system_message, so changing it
        // needs a new --out and an unset persona keeps earlier hashes.
        system_message = Some(trace::system_message(persona.as_deref()));
    } else if persona.is_some() && generating {
        return Err(Error::refuse("[generation].persona requires --detailed"));
    }
    let mut generation = json!({
        "schema_version": "synthlite.generator-config.v1",
        "system_message": system_message,
        "temperature": temperature,
        "top_p": top_p,
        "max_output_tokens": max_output_tokens,
        "seed": seed,
        "frequency_penalty": frequency_penalty,
        "presence_penalty": presence_penalty,
        "stop": stop,
        "reasoning_effort": reasoning_effort,
    });
    // Plain configs keep today's object, so their hash and out dirs resume.
    // `false` is never hashed.
    if detailed {
        generation["detailed"] = Value::Bool(true);
        generation["teacher_instruction"] = Value::String(trace::TEACHER_INSTRUCTION.into());
    }
    Ok(generation)
}

fn resolve_system_message(env_value: Option<String>, file_value: Option<&str>) -> Option<String> {
    match env_value {
        Some(value) if !value.trim().is_empty() => Some(value),
        _ => file_value.and_then(|value| {
            if value.trim().is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }),
    }
}

fn keys_from_file(value: Option<&toml::Value>) -> Result<Vec<KeyConfig>> {
    match value {
        None => Ok(vec![env_key()?]),
        Some(toml::Value::Array(items)) if items.is_empty() => Ok(vec![env_key()?]),
        Some(toml::Value::Array(items)) => {
            let mut keys: Vec<KeyConfig> = Vec::new();
            for item in items {
                let key = file_key(item)?;
                if keys.iter().any(|existing| existing.id == key.id) {
                    return Err(Error::refuse(format!("duplicate [[key]] {}", key.id)));
                }
                keys.push(key);
            }
            Ok(keys)
        }
        Some(_) => Err(Error::refuse("[[key]] must be a table array")),
    }
}

fn env_key() -> Result<KeyConfig> {
    let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        return Err(Error::refuse("OPENAI_API_KEY is required"));
    }
    let model = match std::env::var("SYNTHLITE_MODEL") {
        Ok(value) if !value.is_empty() => value,
        _ => match std::env::var("OPENAI_MODEL") {
            Ok(value) if !value.is_empty() => value,
            _ => {
                return Err(Error::refuse("set OPENAI_MODEL or SYNTHLITE_MODEL"));
            }
        },
    };
    let raw_base = match std::env::var("OPENAI_BASE_URL") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "https://api.openai.com/v1".to_string(),
    };
    finish_key(
        "openai".into(),
        "OPENAI_API_KEY",
        api_key,
        raw_base,
        model,
        Transport::default(),
    )
}

fn file_key(value: &toml::Value) -> Result<KeyConfig> {
    let table = value
        .as_table()
        .ok_or_else(|| Error::refuse("[[key]] entry must be a table"))?;
    let mut provider = None;
    let mut base_url = None;
    let mut model = None;
    let mut api_key_env = None;
    let mut transport = Transport::default();
    for (key, item) in table {
        match key.as_str() {
            "provider" => provider = Some(req_nonempty_str(item, "provider")?),
            "base_url" => base_url = Some(req_nonempty_str(item, "base_url")?),
            "model" => model = Some(req_nonempty_str(item, "model")?),
            "api_key_env" => api_key_env = Some(req_nonempty_str(item, "api_key_env")?),
            "max_concurrent" => transport.max_concurrent = req_u64(item, "max_concurrent")?,
            "requests_per_minute" => {
                transport.requests_per_minute = Some(req_u64(item, "requests_per_minute")?)
            }
            "timeout_seconds" => transport.timeout_seconds = req_u64(item, "timeout_seconds")?,
            "max_attempts" => transport.max_attempts = req_u64(item, "max_attempts")?,
            "rate_limit_cooldown_seconds" => {
                transport.rate_limit_cooldown_seconds =
                    req_u64(item, "rate_limit_cooldown_seconds")?
            }
            "headerless_429_limit" => {
                transport.headerless_429_limit = req_u64(item, "headerless_429_limit")?
            }
            "max_tokens_field" => {
                let field = req_nonempty_str(item, "max_tokens_field")?;
                if field != "max_tokens" && field != "max_completion_tokens" {
                    return Err(Error::refuse(
                        "max_tokens_field must be max_tokens or max_completion_tokens",
                    ));
                }
                transport.max_tokens_field = Some(field);
            }
            "require_model_match" => {
                transport.require_model_match = req_bool(item, "require_model_match")?
            }
            "require_finish_reason" => {
                transport.require_finish_reason = req_bool(item, "require_finish_reason")?
            }
            other => return Err(Error::refuse(format!("unknown [[key]] field {other}"))),
        }
    }
    let provider = provider.ok_or_else(|| Error::refuse("[[key]] provider is required"))?;
    let base_url = base_url.ok_or_else(|| Error::refuse("[[key]] base_url is required"))?;
    let model = model.ok_or_else(|| Error::refuse("[[key]] model is required"))?;
    let api_key_env =
        api_key_env.ok_or_else(|| Error::refuse("[[key]] api_key_env is required"))?;
    if !is_env_var_name(&api_key_env) {
        // Never echo the value: a pasted secret is the usual cause.
        return Err(Error::refuse(
            "[[key]] api_key_env must be an environment variable name ([A-Za-z_][A-Za-z0-9_]*), not a secret",
        ));
    }
    let api_key = std::env::var(&api_key_env).unwrap_or_default();
    if api_key.is_empty() {
        return Err(Error::refuse(format!("{api_key_env} is required")));
    }
    if transport.max_concurrent == 0
        || transport.timeout_seconds == 0
        || transport.max_attempts == 0
    {
        return Err(Error::refuse(
            "max_concurrent, timeout_seconds, and max_attempts must be >= 1",
        ));
    }
    if transport.headerless_429_limit == 0 {
        return Err(Error::refuse("headerless_429_limit must be >= 1"));
    }
    if transport.requests_per_minute == Some(0) {
        return Err(Error::refuse("requests_per_minute must be >= 1"));
    }
    finish_key(provider, &api_key_env, api_key, base_url, model, transport)
}

struct Transport {
    max_concurrent: u64,
    requests_per_minute: Option<u64>,
    timeout_seconds: u64,
    max_attempts: u64,
    rate_limit_cooldown_seconds: u64,
    headerless_429_limit: u64,
    /// `None` picks the default from the base URL host.
    max_tokens_field: Option<String>,
    require_model_match: bool,
    require_finish_reason: bool,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            max_concurrent: 16,
            requests_per_minute: None,
            timeout_seconds: 600,
            max_attempts: 5,
            rate_limit_cooldown_seconds: 60,
            headerless_429_limit: 3,
            max_tokens_field: None,
            require_model_match: true,
            require_finish_reason: true,
        }
    }
}

fn finish_key(
    provider: String,
    api_key_env: &str,
    api_key: String,
    raw_base: String,
    model: String,
    transport: Transport,
) -> Result<KeyConfig> {
    let base = parse_base_url(&raw_base)?;
    let max_tokens_field = transport.max_tokens_field.unwrap_or_else(|| {
        // OpenAI rejects `max_tokens` for reasoning models and accepts
        // `max_completion_tokens` for every chat model.
        if base.host == "api.openai.com" {
            "max_completion_tokens".into()
        } else {
            "max_tokens".into()
        }
    });
    Ok(KeyConfig {
        id: format!("{provider}/{api_key_env}/{model}@{}", base.url),
        display_id: format!("{provider}/{api_key_env}/{model}@{}", base.display),
        provider,
        api_key,
        base_url: base.url,
        base_url_display: base.display,
        model,
        max_concurrent: transport.max_concurrent as usize,
        requests_per_minute: transport.requests_per_minute.map(|n| n as u32),
        timeout_seconds: transport.timeout_seconds,
        max_attempts: transport.max_attempts as u32,
        rate_limit_cooldown_seconds: transport.rate_limit_cooldown_seconds,
        headerless_429_limit: transport.headerless_429_limit as u32,
        max_tokens_field,
        require_model_match: transport.require_model_match,
        require_finish_reason: transport.require_finish_reason,
    })
}

fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct BaseUrl {
    /// Request base without userinfo or trailing `/`.
    url: String,
    host: String,
    /// `host[:port]`.
    display: String,
}

/// Error messages never echo the raw value: it may carry credentials.
fn parse_base_url(raw: &str) -> Result<BaseUrl> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::refuse("base_url is required"));
    }
    let mut url = url::Url::parse(raw)
        .map_err(|err| Error::refuse(format!("base_url is not a valid URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::refuse("base_url must use http or https"));
    }
    let host = match url.host_str() {
        Some(host) if !host.is_empty() => host.to_string(),
        _ => return Err(Error::refuse("base_url must have a host")),
    };
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::refuse(
            "base_url must not contain a query string or fragment",
        ));
    }
    url.set_username("")
        .and_then(|()| url.set_password(None))
        .map_err(|()| Error::refuse("base_url is not a valid URL"))?;
    let display = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.clone(),
    };
    Ok(BaseUrl {
        url: url.as_str().trim_end_matches('/').to_string(),
        host,
        display,
    })
}

fn default_refusal_phrases() -> Vec<String> {
    [
        "i'm sorry, but i can't",
        "i can't help with",
        "i cannot help with",
        "i'm unable to help",
        "as an ai language model",
        "as an ai assistant",
        "as an ai,",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn gate_from_file(value: Option<&toml::Value>) -> Result<GateSettings> {
    let mut settings = GateSettings {
        min_assistant_chars: 200,
        max_assistant_chars: 40_000,
        max_repeated_line: 4,
        refusal_window_chars: 300,
        refusal_phrases: default_refusal_phrases(),
        exclude_prompt_hashes: None,
        validation_per_mille: 30,
    };
    let Some(value) = value else {
        return Ok(settings);
    };
    let table = value
        .as_table()
        .ok_or_else(|| Error::refuse("[gate] must be a table"))?;
    for (key, item) in table {
        match key.as_str() {
            "min_assistant_chars" => {
                settings.min_assistant_chars = req_u64(item, "min_assistant_chars")? as usize
            }
            "max_assistant_chars" => {
                settings.max_assistant_chars = req_u64(item, "max_assistant_chars")? as usize
            }
            "max_repeated_line" => {
                settings.max_repeated_line = req_u64(item, "max_repeated_line")? as usize
            }
            "refusal_window_chars" => {
                settings.refusal_window_chars = req_u64(item, "refusal_window_chars")? as usize
            }
            "validation_per_mille" => {
                let value = req_u64(item, "validation_per_mille")?;
                if value > 1000 {
                    return Err(Error::refuse(
                        "[gate].validation_per_mille must be between 0 and 1000",
                    ));
                }
                settings.validation_per_mille = value as u32;
            }
            "exclude_prompt_hashes" => {
                let path = req_nonempty_str(item, "exclude_prompt_hashes")?;
                settings.exclude_prompt_hashes = Some(PathBuf::from(path));
            }
            "refusal_phrases" => {
                let items = item.as_array().ok_or_else(|| {
                    Error::refuse("[gate].refusal_phrases must be an array of strings")
                })?;
                settings.refusal_phrases = items
                    .iter()
                    .map(|item| {
                        item.as_str().map(str::to_string).ok_or_else(|| {
                            Error::refuse("[gate].refusal_phrases must be an array of strings")
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            other => return Err(Error::refuse(format!("unknown [gate] field {other}"))),
        }
    }
    Ok(settings)
}

pub fn gate_config_value(settings: &GateSettings, exclude_hash: Option<&str>) -> Value {
    json!({
        "min_assistant_chars": settings.min_assistant_chars,
        "max_assistant_chars": settings.max_assistant_chars,
        "max_repeated_line": settings.max_repeated_line,
        "refusal_window_chars": settings.refusal_window_chars,
        "refusal_phrases": settings.refusal_phrases,
        "exclude_prompt_hashes": exclude_hash,
        "validation_per_mille": settings.validation_per_mille,
    })
}

fn card_from_file(value: Option<&toml::Value>) -> Result<CardSettings> {
    let mut card = CardSettings {
        pretty_name: None,
        license: None,
        license_name: None,
        license_link: None,
        language: vec!["en".into()],
    };
    let Some(value) = value else {
        return Ok(card);
    };
    let table = value
        .as_table()
        .ok_or_else(|| Error::refuse("[card] must be a table"))?;
    for (key, item) in table {
        match key.as_str() {
            "pretty_name" => card.pretty_name = Some(req_nonempty_str(item, "pretty_name")?),
            "license" => card.license = Some(req_nonempty_str(item, "license")?),
            "license_name" => card.license_name = Some(req_nonempty_str(item, "license_name")?),
            "license_link" => card.license_link = Some(req_nonempty_str(item, "license_link")?),
            "language" => {
                let items = item
                    .as_array()
                    .ok_or_else(|| Error::refuse("[card].language must be an array of strings"))?;
                card.language = items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| {
                                Error::refuse("[card].language must be an array of strings")
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            other => return Err(Error::refuse(format!("unknown [card] field {other}"))),
        }
    }
    Ok(card)
}

fn toml_number(value: &toml::Value, name: &str) -> Result<Value> {
    match value {
        toml::Value::Integer(number) => Ok(json!(number)),
        toml::Value::Float(number) => {
            if !number.is_finite() {
                return Err(Error::refuse(format!("[generation].{name} must be finite")));
            }
            let number = serde_json::Number::from_f64(*number)
                .ok_or_else(|| Error::refuse(format!("[generation].{name} must be finite")))?;
            Ok(Value::Number(number))
        }
        _ => Err(Error::refuse(format!(
            "[generation].{name} must be a number"
        ))),
    }
}

fn toml_integer(value: &toml::Value, name: &str) -> Result<Value> {
    match value {
        toml::Value::Integer(number) => Ok(json!(number)),
        _ => Err(Error::refuse(format!(
            "[generation].{name} must be an integer"
        ))),
    }
}

fn toml_stop(value: &toml::Value) -> Result<Value> {
    let items = value
        .as_array()
        .ok_or_else(|| Error::refuse("[generation].stop must be an array of strings"))?;
    let mut stop = Vec::new();
    for item in items {
        let text = item
            .as_str()
            .ok_or_else(|| Error::refuse("[generation].stop must be an array of strings"))?;
        stop.push(Value::String(text.to_string()));
    }
    Ok(Value::Array(stop))
}

fn req_nonempty_str(value: &toml::Value, name: &str) -> Result<String> {
    let text = value
        .as_str()
        .ok_or_else(|| Error::refuse(format!("{name} must be a string")))?;
    if text.is_empty() {
        return Err(Error::refuse(format!("{name} must be non-empty")));
    }
    Ok(text.to_string())
}

fn req_u64(value: &toml::Value, name: &str) -> Result<u64> {
    let number = value
        .as_integer()
        .ok_or_else(|| Error::refuse(format!("{name} must be an integer")))?;
    if number < 0 {
        return Err(Error::refuse(format!("{name} must be >= 0")));
    }
    Ok(number as u64)
}

fn req_bool(value: &toml::Value, name: &str) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| Error::refuse(format!("{name} must be a boolean")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal<T>(result: Result<T>) -> String {
        match result {
            Err(Error::Refuse(message)) => message,
            Err(other) => panic!("expected refusal, got {other}"),
            Ok(_) => panic!("expected refusal"),
        }
    }

    fn toml(text: &str) -> toml::Value {
        text.parse::<toml::Value>().unwrap()
    }

    fn key_for(base: &str, transport: Transport) -> Result<KeyConfig> {
        finish_key(
            "openai".into(),
            "K",
            "sk".into(),
            base.into(),
            "m".into(),
            transport,
        )
    }

    #[test]
    fn base_url_strips_userinfo_and_displays_host_port() {
        let base = parse_base_url("https://user:secret@api.openai.com/v1/").unwrap();
        assert_eq!(base.url, "https://api.openai.com/v1");
        assert_eq!(base.display, "api.openai.com");
        let base = parse_base_url("http://localhost:8000/v1").unwrap();
        assert_eq!(base.url, "http://localhost:8000/v1");
        assert_eq!(base.display, "localhost:8000");
    }

    #[test]
    fn at_sign_in_path_does_not_change_host() {
        let base = parse_base_url("https://gw.example/@team/v1").unwrap();
        assert_eq!(base.url, "https://gw.example/@team/v1");
        assert_eq!(base.display, "gw.example");
    }

    #[test]
    fn bad_base_url_is_refused_without_echo() {
        for raw in [
            "https://gw.example/v1?user=ops@corp.example",
            "https://gw.example/v1#frag",
            "api.groq.com/openai/v1",
            "ftp://gw.example/v1",
            "https://",
            "   ",
        ] {
            let message = refusal(parse_base_url(raw));
            assert!(!message.contains("corp.example"), "{message}");
            assert!(!message.contains("groq"), "{message}");
            assert!(!message.contains("gw.example"), "{message}");
        }
    }

    #[test]
    fn max_tokens_field_defaults_by_host() {
        let openai = key_for("https://api.openai.com/v1", Transport::default()).unwrap();
        assert_eq!(openai.max_tokens_field, "max_completion_tokens");
        let other = key_for("http://127.0.0.1:8000/v1", Transport::default()).unwrap();
        assert_eq!(other.max_tokens_field, "max_tokens");
        let overridden = key_for(
            "https://api.openai.com/v1",
            Transport {
                max_tokens_field: Some("max_tokens".into()),
                ..Transport::default()
            },
        )
        .unwrap();
        assert_eq!(overridden.max_tokens_field, "max_tokens");
    }

    #[test]
    fn non_positive_max_output_tokens_is_refused() {
        for value in ["0", "-5"] {
            let table = toml(&format!("max_output_tokens = {value}"));
            let message = refusal(generation_from_file(Some(&table), &LoadMode::Offline));
            assert!(message.contains("max_output_tokens"), "{message}");
        }
        let table = toml("max_output_tokens = 1");
        assert!(generation_from_file(Some(&table), &LoadMode::Offline).is_ok());
    }

    #[test]
    fn validation_per_mille_is_range_checked() {
        for value in ["1001", "4294967326"] {
            let table = toml(&format!("validation_per_mille = {value}"));
            let message = refusal(gate_from_file(Some(&table)));
            assert!(message.contains("validation_per_mille"), "{message}");
        }
        let table = toml("validation_per_mille = 1000");
        assert_eq!(
            gate_from_file(Some(&table)).unwrap().validation_per_mille,
            1000
        );
    }

    #[test]
    fn secret_shaped_api_key_env_is_refused_without_echo() {
        let table = toml(
            r#"
provider = "groq"
base_url = "https://api.groq.com/openai/v1"
model = "m"
api_key_env = "gsk_live-abc.def"
"#,
        );
        let message = refusal(file_key(&table));
        assert!(!message.contains("gsk_live"), "{message}");
        assert!(message.contains("api_key_env"), "{message}");
        assert!(is_env_var_name("GROQ_API_KEY"));
        assert!(is_env_var_name("_K1"));
        assert!(!is_env_var_name("1K"));
        assert!(!is_env_var_name("sk-abc"));
        assert!(!is_env_var_name(""));
    }

    #[test]
    fn file_key_id_is_stable_under_reordering_and_duplicates_refuse() {
        // Test-only variable with a unique name; no other test reads or writes it.
        std::env::set_var("SYNTHLITE_TEST_KEY_ID", "sk-test");
        let entry = |model: &str| {
            format!(
                "[[key]]\nprovider = \"groq\"\nbase_url = \"https://api.groq.com/openai/v1\"\nmodel = \"{model}\"\napi_key_env = \"SYNTHLITE_TEST_KEY_ID\"\n"
            )
        };
        let ids = |text: String| -> Vec<String> {
            let value = toml(&text);
            keys_from_file(value.get("key"))
                .unwrap()
                .into_iter()
                .map(|key| key.id)
                .collect()
        };
        let a_first = ids(format!("{}{}", entry("a"), entry("b")));
        let b_first = ids(format!("{}{}", entry("b"), entry("a")));
        assert_eq!(a_first[0], b_first[1]);
        assert_eq!(a_first[1], b_first[0]);
        assert_eq!(
            a_first[0],
            "groq/SYNTHLITE_TEST_KEY_ID/a@https://api.groq.com/openai/v1"
        );
        let key = key_for(
            "https://user:pw@gw.example:8443/team/v1",
            Transport::default(),
        )
        .unwrap();
        assert_eq!(key.display_id, "openai/K/m@gw.example:8443");
        let duplicate = toml(&format!("{}{}", entry("a"), entry("a")));
        let message = refusal(keys_from_file(duplicate.get("key")));
        assert!(message.contains("duplicate"), "{message}");
    }

    /// `generation_from_file` without the caller's environment.
    fn generation(value: &toml::Value, mode: &LoadMode) -> Result<Value> {
        generation_from(Some(value), mode, None)
    }

    #[test]
    fn plain_generation_hash_is_unchanged() {
        let plain = generation(&toml(""), &LoadMode::Offline).unwrap();
        assert!(plain.get("detailed").is_none());
        assert!(plain.get("teacher_instruction").is_none());
        assert_eq!(
            generator_config_hash(&plain).unwrap(),
            "gen_48ca1520349e09dc2f26f6797206185f1aac4acabf61806899ad03777c49e172"
        );
        let off = generation(&toml("detailed = false"), &LoadMode::Offline).unwrap();
        assert_eq!(off, plain);
    }

    #[test]
    fn detailed_hashes_the_flag_the_contract_and_the_fixed_system() {
        let file = generation(&toml("detailed = true"), &LoadMode::Offline).unwrap();
        assert_eq!(file["detailed"], true);
        assert_eq!(file["system_message"], trace::DEFAULT_SYSTEM_MESSAGE);
        assert_eq!(file["teacher_instruction"], trace::TEACHER_INSTRUCTION);
        let flag = generation(&toml(""), &LoadMode::Generate { detailed: true }).unwrap();
        assert_eq!(flag, file);
        let plain = generation(&toml(""), &LoadMode::Offline).unwrap();
        assert_ne!(
            generator_config_hash(&file).unwrap(),
            generator_config_hash(&plain).unwrap()
        );
        // Pinned: the default system message and the teacher instruction are
        // hashed, so editing either text would strand existing detailed runs.
        assert_eq!(
            generator_config_hash(&file).unwrap(),
            "gen_9f076ab23b6322272d9d4a0e21766485a63d59ee1a306f179c41b2913ac8381a"
        );
    }

    #[test]
    fn detailed_must_be_a_boolean() {
        for value in ["\"yes\"", "1"] {
            let table = toml(&format!("detailed = {value}"));
            let message = refusal(generation(&table, &LoadMode::Offline));
            assert!(
                message.contains("[generation].detailed must be a boolean"),
                "{message}"
            );
        }
    }

    #[test]
    fn detailed_generate_refuses_an_operator_system_message() {
        let generate = LoadMode::Generate { detailed: false };
        let table = toml("detailed = true\nsystem_message = \"Follow the user's requested role.\"");
        let message = refusal(generation(&table, &generate));
        assert!(
            message.contains("--detailed uses a fixed system message"),
            "{message}"
        );
        let offline = generation(&table, &LoadMode::Offline).unwrap();
        assert_eq!(offline["system_message"], trace::DEFAULT_SYSTEM_MESSAGE);
        let blank = toml("detailed = true\nsystem_message = \"  \"");
        assert!(generation(&blank, &generate).is_ok());
        // SYNTHLITE_SYSTEM_PROMPT is refused the same way; whitespace is unset.
        let detailed = toml("detailed = true");
        let env = |value: &str| generation_from(Some(&detailed), &generate, Some(value.into()));
        assert!(refusal(env("Follow the user's requested role.")).contains("fixed system message"));
        assert!(env("   ").is_ok());
    }

    #[test]
    fn persona_names_the_assistant_in_the_hashed_system_message() {
        let generate = LoadMode::Generate { detailed: true };
        let sia =
            toml("persona = \"Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations.\"");
        let branded = generation(&sia, &generate).unwrap();
        assert_eq!(
            branded["system_message"],
            "You are Sia by Scogo.AI, which delivers Autonomous Agentic IT Operations. Use supplied evidence, distinguish facts from hypotheses, prefer safe bounded actions, and include verification and recovery."
        );
        let default = generation(&toml(""), &generate).unwrap();
        assert_eq!(default["system_message"], trace::DEFAULT_SYSTEM_MESSAGE);
        assert_ne!(
            generator_config_hash(&branded).unwrap(),
            generator_config_hash(&default).unwrap()
        );
        assert!(branded.get("persona").is_none());
        for bad in [
            "persona = \"\"",
            "persona = \"  .\"",
            "persona = \"Sia\\nby Scogo\"",
            "persona = 3",
        ] {
            let message = refusal(generation(&toml(bad), &generate));
            assert!(
                message.contains("[generation].persona must be"),
                "{bad}: {message}"
            );
        }
        let plain = toml("persona = \"Sia\"");
        let message = refusal(generation(&plain, &LoadMode::Generate { detailed: false }));
        assert!(
            message.contains("[generation].persona requires --detailed"),
            "{message}"
        );
        assert!(generation(&plain, &LoadMode::Offline).is_ok());
    }
}
