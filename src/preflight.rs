//! Reads and validates the input file before any request is sent.
//!
//! A `.txt` file holds one prompt per line. Any other file is JSONL, where
//! each line is one of:
//!
//! - a prompt record, `{"prompt": ..., "id": ..., "metadata": {...}}`, with
//!   no `schema_version`;
//! - a Taskgen task (`scogo.taskgen.task.v2`);
//! - a Taskgen candidate wrapper (`scogo.taskgen.candidate.v1`).
//!
//! Kinds can be mixed in one file.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::path::Path;

use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::identity::{population_sha256, prompt_task_id, source_task_id};

const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Longest prompt, in characters, for every input kind.
const MAX_PROMPT_CHARS: usize = 20_000;
const TASK_SCHEMA: &str = "scogo.taskgen.task.v2";
/// Taskgen's `candidates.jsonl` wrapper, written right after generation and
/// before review. Its `candidate` object is an ordinary task v2 record.
const CANDIDATE_SCHEMA: &str = "scogo.taskgen.candidate.v1";

/// Every field a prompt record may have. Anything else belongs in `metadata`.
const PROMPT_KEYS: &[&str] = &["prompt", "id", "metadata"];

const TASK_KEYS: &[&str] = &[
    "schema_version",
    "prompt",
    "category",
    "domain",
    "subdomain",
    "difficulty",
    "coordinates",
    "language",
    "taskgen_model",
    "temperature",
];

const COORD_KEYS: &[&str] = &[
    "taxonomy_id",
    "category_id",
    "task_family",
    "environment",
    "platform_scope",
    "platforms",
    "incident_mechanism",
    "evidence_condition",
    "evidence_bundle",
    "action_risk",
    "presentation",
    "curriculum",
];

const CURRICULUM_KEYS: &[&str] = &["context", "objective", "acceptance", "references"];

/// Where a seed came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    /// A Taskgen task, given directly or unwrapped from a candidate.
    Taskgen,
    /// A plain prompt: a JSONL prompt record or a `.txt` line. `id` and
    /// `metadata` are the record's own fields, carried into the row as
    /// `metadata.input_id` and `metadata.input_metadata`.
    Prompt {
        id: Option<String>,
        metadata: Option<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct Seed {
    pub source_task_id: String,
    pub prompt: String,
    /// Taskgen taxonomy fields; `None` for a plain prompt.
    pub category: Option<String>,
    pub domain: Option<String>,
    pub subdomain: Option<String>,
    pub difficulty: Option<i64>,
    /// Taskgen coordinates; `{}` for a plain prompt.
    pub coordinates: Value,
    pub language: Option<String>,
    pub taskgen_model: Option<String>,
    pub origin: Origin,
}

pub struct Preflight {
    pub seeds: Vec<Seed>,
    /// The source record of each seed, in seed order: the Taskgen task, the
    /// prompt record as written, or `{"prompt": <line>}` for a `.txt` line.
    pub tasks: Vec<Value>,
    pub population_sha256: String,
    pub taskgen_run_id: Option<String>,
    /// Candidate lines skipped because Taskgen's deterministic checks failed
    /// them; Taskgen never accepts such a candidate.
    pub skipped_candidates: usize,
}

/// `.txt` (any case) means one prompt per line; everything else is JSONL.
pub fn is_text_input(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("txt"))
}

pub fn load(path: &Path) -> Result<Preflight> {
    let text_input = is_text_input(path);
    let file =
        File::open(path).map_err(|err| Error::refuse(format!("{}: {err}", path.display())))?;
    let mut reader = BufReader::new(file);
    let mut tasks = Vec::new();
    let mut seeds: Vec<Seed> = Vec::new();
    let mut first_seen: HashMap<String, usize> = HashMap::new();
    let mut skipped_candidates = 0usize;
    let mut line_no = 0usize;

    loop {
        line_no += 1;
        let mut buf = Vec::new();
        let got = read_limited(&mut reader, &mut buf, MAX_LINE_BYTES).map_err(|err| {
            if err.kind() == ErrorKind::InvalidData {
                Error::refuse(format!("{}:{line_no}: line exceeds 1 MiB", path.display()))
            } else {
                Error::from(err)
            }
        })?;
        if !got {
            break;
        }
        if line_no == 1 && buf.starts_with(b"\xEF\xBB\xBF") {
            // A byte order mark, as some editors write at the start of a file.
            buf.drain(..3);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let text = String::from_utf8(buf).map_err(|_| {
            Error::refuse(format!("{}:{line_no}: line is not utf-8", path.display()))
        })?;
        let parsed = if text_input {
            text_line(&text)
        } else {
            json_line(&text, &mut skipped_candidates)
        }
        .map_err(|message| Error::refuse(format!("{}:{line_no}: {message}", path.display())))?;
        let Some((seed, record)) = parsed else {
            continue;
        };
        match first_seen.entry(seed.source_task_id.clone()) {
            Entry::Vacant(entry) => {
                entry.insert(line_no);
            }
            Entry::Occupied(entry) => {
                let message = match seed.origin {
                    Origin::Taskgen => format!("duplicate source_task_id {}", seed.source_task_id),
                    Origin::Prompt { .. } => {
                        format!("duplicate prompt (same prompt as line {})", entry.get())
                    }
                };
                return Err(Error::refuse(format!(
                    "{}:{line_no}: {message}",
                    path.display()
                )));
            }
        }
        tasks.push(record);
        seeds.push(seed);
    }

    let entries: Vec<(String, &Value)> = seeds
        .iter()
        .zip(&tasks)
        .map(|(seed, task)| (seed.source_task_id.clone(), task))
        .collect();
    let population_sha256 = population_sha256(&entries)?;
    Ok(Preflight {
        seeds,
        tasks,
        population_sha256,
        taskgen_run_id: read_run_id(path),
        skipped_candidates,
    })
}

type Parsed = std::result::Result<Option<(Seed, Value)>, String>;

/// One `.txt` line: the trimmed line is the prompt. A line that is only
/// whitespace is skipped.
fn text_line(text: &str) -> Parsed {
    let prompt = text.trim();
    if prompt.is_empty() {
        return Ok(None);
    }
    check_prompt(prompt)?;
    let seed = prompt_seed(prompt.to_string(), None, None)?;
    Ok(Some((seed, json!({ "prompt": prompt }))))
}

/// One JSONL line, told apart by its `schema_version`. `Ok(None)` is a
/// Taskgen candidate that Taskgen itself failed.
fn json_line(text: &str, skipped_candidates: &mut usize) -> Parsed {
    let value: Value = serde_json::from_str(text).map_err(|err| {
        format!("malformed JSON: {err} (for one plain prompt per line, use a .txt file)")
    })?;
    let Some(object) = value.as_object() else {
        return Err("each line must be a JSON object".into());
    };
    match object.get("schema_version") {
        None => {
            let seed = prompt_record(object)?;
            Ok(Some((seed, value)))
        }
        Some(Value::String(schema)) if schema == TASK_SCHEMA => {
            let seed = validate_task(&value)?;
            Ok(Some((seed, value)))
        }
        Some(Value::String(schema)) if schema == CANDIDATE_SCHEMA => {
            let failed = value
                .pointer("/deterministic_checks/hard_failures")
                .and_then(Value::as_array)
                .is_some_and(|failures| !failures.is_empty());
            if failed {
                *skipped_candidates += 1;
                return Ok(None);
            }
            // The inner task is validated and hashed exactly as a
            // tasks.jsonl line, so both files give the same ids.
            let task = object
                .get("candidate")
                .cloned()
                .ok_or_else(|| format!("{CANDIDATE_SCHEMA} record has no candidate object"))?;
            let seed = validate_task(&task)?;
            Ok(Some((seed, task)))
        }
        Some(Value::String(schema)) => Err(format!("unsupported schema_version {schema}")),
        Some(other) => Err(format!("unsupported schema_version {other}")),
    }
}

/// A JSONL prompt record: `prompt` is required; `id` (a string) and
/// `metadata` (an object) are optional, and `null` counts as absent.
fn prompt_record(object: &Map<String, Value>) -> std::result::Result<Seed, String> {
    for key in object.keys() {
        if !PROMPT_KEYS.contains(&key.as_str()) {
            return Err(format!(
                "unknown field {key} (put extra fields under \"metadata\")"
            ));
        }
    }
    let prompt = match object.get("prompt") {
        Some(Value::String(prompt)) => prompt.clone(),
        Some(_) => return Err("prompt must be a string".into()),
        None => return Err("missing prompt".into()),
    };
    check_prompt(&prompt)?;
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => return Err("id must be a string".into()),
    };
    let metadata = match object.get("metadata") {
        None | Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => Some(value.clone()),
        Some(_) => return Err("metadata must be an object".into()),
    };
    prompt_seed(prompt, id, metadata)
}

fn check_prompt(prompt: &str) -> std::result::Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("prompt must be non-empty".into());
    }
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        return Err(format!(
            "prompt is longer than {MAX_PROMPT_CHARS} characters"
        ));
    }
    Ok(())
}

fn prompt_seed(
    prompt: String,
    id: Option<String>,
    metadata: Option<Value>,
) -> std::result::Result<Seed, String> {
    Ok(Seed {
        source_task_id: prompt_task_id(&prompt).map_err(|err| err.to_string())?,
        prompt,
        category: None,
        domain: None,
        subdomain: None,
        difficulty: None,
        coordinates: json!({}),
        language: None,
        taskgen_model: None,
        origin: Origin::Prompt { id, metadata },
    })
}

fn validate_task(task: &Value) -> std::result::Result<Seed, String> {
    let obj = task
        .as_object()
        .ok_or_else(|| "task must be a JSON object".to_string())?;
    for key in obj.keys() {
        if !TASK_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown field {key}"));
        }
    }
    let schema = req_str(obj, "schema_version")?;
    if schema != TASK_SCHEMA {
        return Err(format!("schema_version must be {TASK_SCHEMA}"));
    }
    let prompt = req_str(obj, "prompt")?;
    let prompt_len = prompt.chars().count();
    if !(1..=MAX_PROMPT_CHARS).contains(&prompt_len) {
        return Err("prompt length must be 1 through 20000".into());
    }
    let category = nonempty(obj, "category")?;
    let domain = nonempty(obj, "domain")?;
    let subdomain = nonempty(obj, "subdomain")?;
    let difficulty = match obj.get("difficulty").and_then(Value::as_number) {
        Some(number) if number.is_i64() => number
            .as_i64()
            .ok_or_else(|| "difficulty must be an integer".to_string())?,
        _ => return Err("difficulty must be an integer".into()),
    };
    if !(1..=10).contains(&difficulty) {
        return Err("difficulty must be from 1 through 10".into());
    }
    let coordinates = obj
        .get("coordinates")
        .ok_or_else(|| "missing coordinates".to_string())?;
    validate_coordinates(coordinates)?;
    let language = match obj.get("language") {
        None => None,
        Some(Value::String(value)) => {
            let len = value.chars().count();
            if !(2..=16).contains(&len) {
                return Err("language length must be 2 through 16".into());
            }
            Some(value.clone())
        }
        Some(_) => return Err("language must be a string".into()),
    };
    let taskgen_model = nonempty(obj, "taskgen_model")?;
    let temperature = obj
        .get("temperature")
        .and_then(Value::as_f64)
        .ok_or_else(|| "temperature must be a number".to_string())?;
    if !temperature.is_finite() || temperature < 0.0 {
        return Err("temperature must be greater than or equal to 0".into());
    }
    let source_task_id = source_task_id(task).map_err(|err| err.to_string())?;
    Ok(Seed {
        source_task_id,
        prompt,
        category: Some(category),
        domain: Some(domain),
        subdomain: Some(subdomain),
        difficulty: Some(difficulty),
        coordinates: coordinates.clone(),
        language,
        taskgen_model: Some(taskgen_model),
        origin: Origin::Taskgen,
    })
}

fn validate_coordinates(value: &Value) -> std::result::Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "coordinates must be an object".to_string())?;
    for key in obj.keys() {
        if !COORD_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown coordinates field {key}"));
        }
    }
    for key in COORD_KEYS {
        if key != &"platforms"
            && key != &"platform_scope"
            && key != &"curriculum"
            && !obj.contains_key(*key)
        {
            return Err(format!("missing coordinates.{key}"));
        }
    }
    for key in [
        "taxonomy_id",
        "category_id",
        "task_family",
        "environment",
        "incident_mechanism",
        "evidence_condition",
        "evidence_bundle",
        "action_risk",
        "presentation",
    ] {
        nonempty(obj, key)?;
    }
    let scope = nonempty(obj, "platform_scope")?;
    let expected = match scope.as_str() {
        "platform_neutral" => 0,
        "single_platform" => 1,
        "multi_platform" => 2,
        _ => return Err("coordinates.platform_scope is invalid".into()),
    };
    let platforms = obj
        .get("platforms")
        .and_then(Value::as_array)
        .ok_or_else(|| "coordinates.platforms must be an array".to_string())?;
    if platforms.len() != expected {
        return Err(format!(
            "coordinates.platforms must contain {expected} entries for {scope}"
        ));
    }
    let mut seen = HashSet::new();
    for platform in platforms {
        let name = platform
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "coordinates.platforms entries must be non-empty strings".to_string())?;
        if !seen.insert(name.to_string()) {
            return Err("coordinates.platforms entries must be unique".into());
        }
    }
    if let Some(curriculum) = obj.get("curriculum") {
        validate_curriculum(curriculum)?;
    }
    Ok(())
}

fn validate_curriculum(value: &Value) -> std::result::Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "coordinates.curriculum must be an object".to_string())?;
    for key in obj.keys() {
        if !CURRICULUM_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown coordinates.curriculum field {key}"));
        }
    }
    let context = obj
        .get("context")
        .and_then(Value::as_object)
        .filter(|context| !context.is_empty())
        .ok_or_else(|| "coordinates.curriculum.context must be a non-empty object".to_string())?;
    for (axis, value) in context {
        if axis.is_empty() || !value.as_str().is_some_and(|value| !value.is_empty()) {
            return Err("coordinates.curriculum.context entries must be non-empty strings".into());
        }
    }
    if !obj
        .get("objective")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return Err("coordinates.curriculum.objective must be a non-empty string".into());
    }
    for field in ["acceptance", "references"] {
        let values = obj
            .get(field)
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .ok_or_else(|| format!("coordinates.curriculum.{field} must be a non-empty array"))?;
        for value in values {
            let Some(text) = value.as_str().filter(|text| !text.is_empty()) else {
                return Err(format!(
                    "coordinates.curriculum.{field} entries must be non-empty strings"
                ));
            };
            if field == "references" && !text.starts_with("https://") {
                return Err("coordinates.curriculum.references entries must use https://".into());
            }
        }
    }
    Ok(())
}

fn req_str(obj: &serde_json::Map<String, Value>, key: &str) -> std::result::Result<String, String> {
    match obj.get(key) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing {key}")),
    }
}

fn nonempty(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> std::result::Result<String, String> {
    let value = req_str(obj, key)?;
    if value.is_empty() {
        return Err(format!("{key} must be non-empty"));
    }
    Ok(value)
}

fn read_limited<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<bool> {
    buf.clear();
    loop {
        let (chunk, newline, eof) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                (Vec::new(), false, true)
            } else if let Some(pos) = available.iter().position(|byte| *byte == b'\n') {
                (available[..=pos].to_vec(), true, false)
            } else {
                (available.to_vec(), false, false)
            }
        };
        if eof {
            return Ok(!buf.is_empty());
        }
        let content = if newline {
            chunk.len() - 1
        } else {
            chunk.len()
        };
        if buf.len() + content > limit {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "line too long"));
        }
        reader.consume(chunk.len());
        buf.extend_from_slice(&chunk);
        if newline {
            return Ok(true);
        }
    }
}

fn read_run_id(tasks_path: &Path) -> Option<String> {
    let path = tasks_path.parent()?.join("run.json");
    if !path.is_file() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("schema_version")?.as_str()? != "scogo.taskgen.run.v3" {
        return None;
    }
    let run_id = value.get("run_id")?.as_str()?.to_string();
    if run_id.is_empty() {
        None
    } else {
        Some(run_id)
    }
}
