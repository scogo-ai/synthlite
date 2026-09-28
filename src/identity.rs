use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::Result;

/// Schema tag hashed into the id of a plain prompt record, so a prompt never
/// shares an id with a Taskgen task.
pub const PROMPT_IDENTITY_SCHEMA: &str = "synthlite.prompt.v1";

/// The identity object of a Taskgen task record, with the same
/// `serde_json::to_vec` bytes Taskgen hashes, so a seed keeps its Taskgen id.
/// `language`, `taskgen_model`, and `temperature` are not part of the hash.
pub fn identity_value(task: &Value) -> Value {
    json!({
        "schema_version": task.get("schema_version").cloned().unwrap_or(Value::Null),
        "prompt": task.get("prompt").cloned().unwrap_or(Value::Null),
        "category": task.get("category").cloned().unwrap_or(Value::Null),
        "domain": task.get("domain").cloned().unwrap_or(Value::Null),
        "subdomain": task.get("subdomain").cloned().unwrap_or(Value::Null),
        "difficulty": task.get("difficulty").cloned().unwrap_or(Value::Null),
        "coordinates": task.get("coordinates").cloned().unwrap_or_else(|| json!({})),
    })
}

/// `source_task_id` of a Taskgen task record.
pub fn source_task_id(task: &Value) -> Result<String> {
    hash_json("task_", &identity_value(task))
}

/// `source_task_id` of a plain prompt (a JSONL prompt record or a `.txt`
/// line). Only the prompt text is hashed: a record's `id` and `metadata`
/// never change it.
pub fn prompt_task_id(prompt: &str) -> Result<String> {
    hash_json(
        "task_",
        &json!({
            "schema_version": PROMPT_IDENTITY_SCHEMA,
            "prompt": prompt,
        }),
    )
}

/// Population digest of Taskgen task records.
pub fn source_population_sha256(tasks: &[Value]) -> Result<String> {
    let mut entries = Vec::with_capacity(tasks.len());
    for task in tasks {
        entries.push((source_task_id(task)?, task));
    }
    population_sha256(&entries)
}

/// Population digest of `(source_task_id, source record)` pairs in any
/// order. For Taskgen tasks it equals [`source_population_sha256`].
pub fn population_sha256(entries: &[(String, &Value)]) -> Result<String> {
    let mut rows = Vec::with_capacity(entries.len());
    for (id, task) in entries {
        rows.push(json!({
            "source_task_id": id,
            "source_task": task,
        }));
    }
    rows.sort_by(|left, right| {
        left.get("source_task_id")
            .and_then(Value::as_str)
            .cmp(&right.get("source_task_id").and_then(Value::as_str))
    });
    let population = json!({
        "schema_version": "scogo.private-hf-source-population.v1",
        "rows": rows,
    });
    hash_json("", &population)
}

pub fn generator_config_hash(config: &Value) -> Result<String> {
    hash_json("gen_", config)
}

pub fn hash_json(prefix: &str, value: &Value) -> Result<String> {
    let bytes = serde_json::to_vec(value)?;
    Ok(format!("{prefix}{}", sha256_hex(&bytes)))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Taskgen's `normalize_prompt`: Unicode lowercase, split on whitespace, and
/// join with one space.
pub fn normalize_prompt(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn norm_hash(text: &str) -> String {
    sha256_hex(normalize_prompt(text).as_bytes())
}
