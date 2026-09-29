use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};

use crate::config::{self, CardSettings, GateSettings, LoadMode};
use crate::error::{Error, Result};
use crate::identity::{self, norm_hash};
use crate::store::{self, State};
use crate::trace;

pub struct GateOpts {
    pub out: PathBuf,
    pub config: Option<PathBuf>,
    pub pretty_name_fallback: String,
}

pub fn run(opts: GateOpts) -> Result<()> {
    let _lock = store::lock_dir(&opts.out)?;
    run_locked(opts)
}

/// `gate` for a caller that already holds the `--out` lock (push).
pub fn run_locked(opts: GateOpts) -> Result<()> {
    if !opts.out.is_dir() {
        return Err(Error::refuse(format!(
            "{} does not exist",
            opts.out.display()
        )));
    }
    if store::has_torn_tail(&opts.out.join("rows.jsonl"))?
        || store::has_torn_tail(&opts.out.join("rows.errors.jsonl"))?
    {
        return Err(Error::refuse(
            "torn tail in the output directory; resume generate before gate",
        ));
    }
    let state = store::read_state(&opts.out)?.ok_or_else(|| {
        Error::refuse(format!(
            "state.json is missing in {}; generation is incomplete",
            opts.out.display()
        ))
    })?;
    if !state.generation_complete {
        return Err(Error::refuse(
            "generation_complete is false; pending or failed rows are not publishable",
        ));
    }
    // The mode comes from state.json only, never from the config file or a flag.
    let detailed = state.generator_config.get("detailed") == Some(&Value::Bool(true));
    let loaded = config::load(opts.config.as_deref(), LoadMode::Offline)?;
    let (holdout, exclude_hash) = load_holdout(loaded.gate.exclude_prompt_hashes.as_deref())?;
    let gate_value = config::gate_config_value(&loaded.gate, exclude_hash.as_deref());
    let gate_hash = identity::hash_json("", &gate_value)?;

    let (rows_bytes, lines) = read_row_lines(&opts.out.join("rows.jsonl"))?;
    let mut parsed = Vec::with_capacity(lines.len());
    for (index, bytes) in lines.iter().enumerate() {
        parsed.push(parse_row(bytes, index + 1, &state)?);
    }

    let mut rejected_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut rejected_lines = Vec::new();
    let mut survivors: Vec<usize> = Vec::new();
    for (index, row) in parsed.iter().enumerate() {
        let mut reasons = Vec::new();
        if holdout.contains(&norm_hash(&row.user)) {
            reasons.push("held_out");
        }
        let chars = row.assistant.chars().count();
        if chars < loaded.gate.min_assistant_chars {
            reasons.push("too_short");
        }
        if chars > loaded.gate.max_assistant_chars {
            reasons.push("too_long");
        }
        // A detailed reply opens with the trace, so the refusal window never
        // reaches the answer; check the final answer on its own as well.
        if is_refusal(&row.assistant, &loaded.gate)
            || (detailed
                && trace::final_answer(&row.assistant)
                    .is_some_and(|answer| is_refusal(answer, &loaded.gate)))
        {
            reasons.push("refusal");
        }
        if is_repetition(&row.assistant, loaded.gate.max_repeated_line) {
            reasons.push("repetition");
        }
        if reasons.is_empty() {
            survivors.push(index);
            continue;
        }
        for reason in &reasons {
            *rejected_counts.entry((*reason).to_string()).or_insert(0) += 1;
        }
        rejected_lines.push(rejection_line(row, &reasons, None, None)?);
    }

    let mut seen_prompt: HashMap<String, String> = HashMap::new();
    let mut seen_response: HashMap<String, String> = HashMap::new();
    let mut kept = Vec::new();
    for index in survivors {
        let row = &parsed[index];
        let prompt_key = norm_hash(&row.user);
        let response_key = norm_hash(&row.assistant);
        let mut reasons = Vec::new();
        let mut dup_prompt = None;
        let mut dup_response = None;
        if let Some(existing) = seen_prompt.get(&prompt_key) {
            reasons.push("duplicate_prompt");
            dup_prompt = Some(existing.clone());
        }
        if let Some(existing) = seen_response.get(&response_key) {
            reasons.push("duplicate_response");
            dup_response = Some(existing.clone());
        }
        if reasons.is_empty() {
            seen_prompt.insert(prompt_key, row.source_task_id.clone());
            seen_response.insert(response_key, row.source_task_id.clone());
            kept.push(index);
            continue;
        }
        for reason in &reasons {
            *rejected_counts.entry((*reason).to_string()).or_insert(0) += 1;
        }
        rejected_lines.push(rejection_line(row, &reasons, dup_prompt, dup_response)?);
    }

    let mut train = Vec::new();
    let mut validation = Vec::new();
    for index in &kept {
        let row = &parsed[*index];
        if is_validation(&row.source_task_id, loaded.gate.validation_per_mille)? {
            validation.extend_from_slice(&lines[*index]);
        } else {
            train.extend_from_slice(&lines[*index]);
        }
    }

    let kept_rows: Vec<&ParsedRow> = kept.iter().map(|index| &parsed[*index]).collect();
    let manifest = build_manifest(
        &state,
        &gate_hash,
        &gate_value,
        &parsed,
        &kept_rows,
        &rejected_counts,
        train_rows(&parsed, &kept, &loaded.gate)?,
        validation_rows(&parsed, &kept, &loaded.gate)?,
        loaded.gate.validation_per_mille,
    )?;
    let manifest_bytes = pretty(&manifest)?;
    let readme = render_card(
        &loaded.card,
        &opts.pretty_name_fallback,
        &state,
        &manifest,
        &rejected_counts,
        detailed,
    );
    let readme_bytes = readme.into_bytes();

    let data = opts.out.join("data");
    fs::create_dir_all(&data)?;
    store::write_atomic(&data.join("train.jsonl"), &train)?;
    let validation_path = data.join("validation.jsonl");
    let validation_sha = if validation.is_empty() {
        if validation_path.exists() {
            fs::remove_file(&validation_path)?;
        }
        Value::Null
    } else {
        store::write_atomic(&validation_path, &validation)?;
        Value::String(identity::sha256_hex(&validation))
    };
    store::write_atomic(&opts.out.join("manifest.json"), &manifest_bytes)?;
    store::write_atomic(&opts.out.join("README.md"), &readme_bytes)?;
    let rejected_bytes = rejected_lines.concat();
    store::write_atomic(&opts.out.join("rejected.jsonl"), &rejected_bytes)?;

    let receipt = json!({
        "rows_sha256": identity::sha256_hex(&rows_bytes),
        "gate_config_hash": gate_hash,
        "train_sha256": identity::sha256_hex(&train),
        "validation_sha256": validation_sha,
        "manifest_sha256": identity::sha256_hex(&manifest_bytes),
        "readme_sha256": identity::sha256_hex(&readme_bytes),
        "counts": {
            "committed": parsed.len(),
            "rejected": rejected_counts,
            "train": manifest.counts.train,
            "validation": manifest.counts.validation,
        }
    });
    store::write_atomic(&opts.out.join("gate.json"), &pretty(&receipt)?)?;
    eprintln!(
        "gate train={} validation={} rejected={}",
        manifest.counts.train,
        manifest.counts.validation,
        rejected_lines.len()
    );
    Ok(())
}

struct ParsedRow {
    source_task_id: String,
    variant_index: u64,
    generator_config_hash: String,
    user: String,
    assistant: String,
    provider: String,
    base_url: String,
    model: String,
    generated_at: String,
    usage: Option<(i64, i64)>,
}

/// Returns the raw bytes of `rows.jsonl` with its non-blank lines, so the
/// receipt hashes exactly the bytes that were gated.
fn read_row_lines(path: &std::path::Path) -> Result<(Vec<u8>, Vec<Vec<u8>>)> {
    if !path.is_file() {
        return Ok((Vec::new(), Vec::new()));
    }
    let data = fs::read(path)?;
    if data.is_empty() {
        return Ok((data, Vec::new()));
    }
    if data.last() != Some(&b'\n') {
        return Err(Error::refuse(
            "torn tail in rows.jsonl; resume generate before gate",
        ));
    }
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in data.iter().enumerate() {
        if *byte == b'\n' {
            let line = data[start..=index].to_vec();
            if line.iter().any(|b| !b.is_ascii_whitespace()) {
                lines.push(line);
            }
            start = index + 1;
        }
    }
    Ok((data, lines))
}

fn parse_row(bytes: &[u8], line_no: usize, state: &State) -> Result<ParsedRow> {
    let value: Value = serde_json::from_slice(bytes.trim_ascii_end())
        .map_err(|_| Error::refuse(format!("rows.jsonl:{line_no}: not a synthlite.sft.v1 row")))?;
    if value.get("schema_version").and_then(Value::as_str) != Some("synthlite.sft.v1") {
        return Err(Error::refuse(format!(
            "rows.jsonl:{line_no}: not a synthlite.sft.v1 row"
        )));
    }
    let source_task_id = req_string(&value, "source_task_id", line_no)?;
    let well_formed = source_task_id.strip_prefix("task_").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !well_formed {
        return Err(Error::refuse(format!(
            "rows.jsonl:{line_no}: invalid source_task_id"
        )));
    }
    let hash = req_string(&value, "generator_config_hash", line_no)?;
    if !state.accepts(&hash) {
        return Err(Error::refuse(format!(
            "rows.jsonl:{line_no}: generator_config_hash does not match state.json"
        )));
    }
    let variant_index = value
        .get("variant_index")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::refuse(format!("rows.jsonl:{line_no}: variant_index is missing")))?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::refuse(format!("rows.jsonl:{line_no}: messages is missing")))?;
    let turns = messages
        .iter()
        .map(|message| {
            let role = message.get("role").and_then(Value::as_str).ok_or_else(|| {
                Error::refuse(format!("rows.jsonl:{line_no}: message role is missing"))
            })?;
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| {
                    Error::refuse(format!(
                        "rows.jsonl:{line_no}: message content is missing or empty"
                    ))
                })?;
            Ok((role, content))
        })
        .collect::<Result<Vec<_>>>()?;
    let (user, assistant) = match turns.as_slice() {
        [("user", user), ("assistant", assistant)]
        | [("system", _), ("user", user), ("assistant", assistant)] => {
            (user.to_string(), assistant.to_string())
        }
        _ => {
            return Err(Error::refuse(format!(
                "rows.jsonl:{line_no}: messages must be [system?, user, assistant]"
            )));
        }
    };
    let metadata = value.get("metadata").cloned().unwrap_or(Value::Null);
    let usage = metadata.get("usage").and_then(|usage| {
        if usage.is_null() {
            return None;
        }
        Some((
            usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        ))
    });
    Ok(ParsedRow {
        source_task_id,
        variant_index,
        generator_config_hash: hash,
        user,
        assistant,
        provider: metadata
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        base_url: metadata
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: metadata
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        generated_at: metadata
            .get("generated_at")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        usage,
    })
}

fn req_string(value: &Value, key: &str, line_no: usize) -> Result<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::refuse(format!("rows.jsonl:{line_no}: missing {key}")))
}

fn load_holdout(path: Option<&std::path::Path>) -> Result<(HashSet<String>, Option<String>)> {
    let Some(path) = path else {
        return Ok((HashSet::new(), None));
    };
    let bytes = fs::read(path)
        .map_err(|err| Error::refuse(format!("exclude_prompt_hashes {}: {err}", path.display())))?;
    let digest = identity::sha256_hex(&bytes);
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::refuse("exclude_prompt_hashes is not utf-8"))?;
    let mut set = HashSet::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let hex = line.to_ascii_lowercase();
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::refuse(format!(
                "exclude_prompt_hashes:{}: expected 64 hex characters",
                index + 1
            )));
        }
        set.insert(hex);
    }
    Ok((set, Some(digest)))
}

fn is_refusal(text: &str, settings: &GateSettings) -> bool {
    let window: String = text.chars().take(settings.refusal_window_chars).collect();
    let folded = window.to_lowercase().replace('\u{2019}', "'");
    settings.refusal_phrases.iter().any(|phrase| {
        let phrase = phrase.to_lowercase().replace('\u{2019}', "'");
        !phrase.is_empty() && folded.contains(&phrase)
    })
}

fn is_repetition(text: &str, max_repeated: usize) -> bool {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.chars().count() < 20 {
            continue;
        }
        let count = counts.entry(trimmed).or_insert(0);
        *count += 1;
        if *count > max_repeated {
            return true;
        }
    }
    false
}

pub fn is_validation(source_task_id: &str, per_mille: u32) -> Result<bool> {
    if per_mille == 0 {
        return Ok(false);
    }
    let value = source_task_id
        .get(5..13)
        .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
        .ok_or_else(|| {
            Error::refuse(format!("source_task_id {source_task_id} is not splittable"))
        })?;
    Ok(value % 1000 < per_mille)
}

fn rejection_line(
    row: &ParsedRow,
    reasons: &[&str],
    duplicate_prompt_of: Option<String>,
    duplicate_response_of: Option<String>,
) -> Result<Vec<u8>> {
    let mut record = json!({
        "source_task_id": row.source_task_id,
        "variant_index": row.variant_index,
        "generator_config_hash": row.generator_config_hash,
        "reasons": reasons,
    });
    if let Some(id) = duplicate_prompt_of {
        record["duplicate_prompt_of"] = json!(id);
    }
    if let Some(id) = duplicate_response_of {
        record["duplicate_response_of"] = json!(id);
    }
    let mut bytes = serde_json::to_vec(&record)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn train_rows(rows: &[ParsedRow], kept: &[usize], settings: &GateSettings) -> Result<u64> {
    let mut count = 0u64;
    for index in kept {
        if !is_validation(&rows[*index].source_task_id, settings.validation_per_mille)? {
            count += 1;
        }
    }
    Ok(count)
}

fn validation_rows(rows: &[ParsedRow], kept: &[usize], settings: &GateSettings) -> Result<u64> {
    let mut count = 0u64;
    for index in kept {
        if is_validation(&rows[*index].source_task_id, settings.validation_per_mille)? {
            count += 1;
        }
    }
    Ok(count)
}

#[derive(Serialize)]
struct Manifest {
    schema_version: String,
    synthlite_version: String,
    generator_config_hash: String,
    generator_config: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    generator_config_history: Vec<store::ConfigEpoch>,
    gate_config_hash: String,
    gate_config: Value,
    source: ManifestSource,
    teachers: Vec<Teacher>,
    counts: ManifestCounts,
    split: ManifestSplit,
    generated_at: ManifestTimes,
    usage_totals: UsageTotals,
}

#[derive(Serialize)]
struct ManifestSource {
    taskgen_schema: String,
    source_population_sha256: String,
    taskgen_run_id: Option<String>,
    seed_count: u64,
}

#[derive(Serialize)]
struct Teacher {
    provider: String,
    base_url: String,
    model: String,
    rows: u64,
}

#[derive(Serialize)]
struct ManifestCounts {
    committed: u64,
    rejected: BTreeMap<String, usize>,
    train: u64,
    validation: u64,
}

#[derive(Serialize)]
struct ManifestSplit {
    method: String,
    validation_per_mille: u32,
}

#[derive(Serialize)]
struct ManifestTimes {
    first: Option<String>,
    last: Option<String>,
}

#[derive(Serialize)]
struct UsageTotals {
    prompt_tokens: i64,
    completion_tokens: i64,
    rows_with_usage: u64,
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    state: &State,
    gate_hash: &str,
    gate_value: &Value,
    committed: &[ParsedRow],
    kept: &[&ParsedRow],
    rejected: &BTreeMap<String, usize>,
    train: u64,
    validation: u64,
    per_mille: u32,
) -> Result<Manifest> {
    let mut teachers: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for row in kept {
        *teachers
            .entry((
                row.provider.clone(),
                row.base_url.clone(),
                row.model.clone(),
            ))
            .or_insert(0) += 1;
    }
    let mut times: Vec<&str> = committed
        .iter()
        .map(|row| row.generated_at.as_str())
        .filter(|stamp| !stamp.is_empty())
        .collect();
    times.sort_unstable();
    let mut prompt_tokens = 0i64;
    let mut completion_tokens = 0i64;
    let mut rows_with_usage = 0u64;
    for row in committed {
        if let Some((prompt, completion)) = row.usage {
            prompt_tokens += prompt;
            completion_tokens += completion;
            rows_with_usage += 1;
        }
    }
    Ok(Manifest {
        schema_version: "synthlite.manifest.v1".into(),
        synthlite_version: state.synthlite_version.clone(),
        generator_config_hash: state.generator_config_hash.clone(),
        generator_config: state.generator_config.clone(),
        generator_config_history: state.config_history.clone(),
        gate_config_hash: gate_hash.to_string(),
        gate_config: gate_value.clone(),
        source: ManifestSource {
            taskgen_schema: "scogo.taskgen.task.v2".into(),
            source_population_sha256: state.source_population_sha256.clone(),
            taskgen_run_id: state.taskgen_run_id.clone(),
            seed_count: state.seed_count,
        },
        teachers: teachers
            .into_iter()
            .map(|((provider, base_url, model), rows)| Teacher {
                provider,
                base_url,
                model,
                rows,
            })
            .collect(),
        counts: ManifestCounts {
            committed: committed.len() as u64,
            rejected: rejected.clone(),
            train,
            validation,
        },
        split: ManifestSplit {
            method: "source_task_id_hex_mod_1000".into(),
            validation_per_mille: per_mille,
        },
        generated_at: ManifestTimes {
            first: times.first().map(|s| (*s).to_string()),
            last: times.last().map(|s| (*s).to_string()),
        },
        usage_totals: UsageTotals {
            prompt_tokens,
            completion_tokens,
            rows_with_usage,
        },
    })
}

fn render_card(
    card: &CardSettings,
    fallback_name: &str,
    state: &State,
    manifest: &Manifest,
    rejected: &BTreeMap<String, usize>,
    detailed: bool,
) -> String {
    let pretty_name = card
        .pretty_name
        .clone()
        .unwrap_or_else(|| fallback_name.to_string());
    let mut front = String::from("---\n");
    front.push_str(&format!("pretty_name: {}\n", yaml_string(&pretty_name)));
    if let Some(license) = &card.license {
        front.push_str(&format!("license: {}\n", yaml_string(license)));
        if license == "other" {
            if let Some(name) = &card.license_name {
                front.push_str(&format!("license_name: {}\n", yaml_string(name)));
            }
            if let Some(link) = &card.license_link {
                front.push_str(&format!("license_link: {}\n", yaml_string(link)));
            }
        }
    }
    front.push_str("language:\n");
    for language in &card.language {
        front.push_str(&format!("  - {}\n", yaml_string(language)));
    }
    front.push_str("task_categories:\n  - text-generation\n");
    front.push_str("tags:\n  - synthetic\n  - sft\n  - synthlite\n");
    front.push_str(&format!(
        "size_categories:\n  - {}\n",
        size_category(manifest.counts.train as usize)
    ));
    front.push_str("configs:\n  - config_name: default\n    data_files:\n");
    front.push_str("      - split: train\n        path: data/train.jsonl\n");
    if manifest.counts.validation > 0 {
        front.push_str("      - split: validation\n        path: data/validation.jsonl\n");
    }
    front.push_str("---\n\n");
    front.push_str(&format!(
        "# {}\n\n",
        pretty_name.replace(char::is_control, " ")
    ));
    front.push_str(
        "Replies are unverified teacher generations. They were not judged for correctness.\n\n",
    );
    if detailed {
        front.push_str(DETAILED_CARD_PARAGRAPH);
    }
    front.push_str(&format!(
        "Train rows: {}.\n\nValidation rows: {}.\n\n",
        manifest.counts.train, manifest.counts.validation
    ));
    if manifest.teachers.is_empty() {
        front.push_str("Teachers: none.\n\n");
    } else {
        front.push_str("Teachers:\n\n");
        for teacher in &manifest.teachers {
            front.push_str(&format!(
                "- {} {} ({}): {} rows\n",
                teacher.provider, teacher.model, teacher.base_url, teacher.rows
            ));
        }
        front.push('\n');
    }
    let system_used = state
        .generator_config
        .get("system_message")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    front.push_str(if system_used {
        "System message: used.\n\n"
    } else {
        "System message: none.\n\n"
    });
    if rejected.is_empty() {
        front.push_str("Gate rejections: none.\n\n");
    } else {
        let parts: Vec<String> = rejected
            .iter()
            .map(|(reason, count)| format!("{reason} {count}"))
            .collect();
        front.push_str(&format!("Gate rejections: {}.\n\n", parts.join(", ")));
    }
    front.push_str(&format!(
        "Split: source_task_id hex [5..13] mod 1000 < {} is validation.\n\n",
        manifest.split.validation_per_mille
    ));
    front.push_str(&format!(
        "source_population_sha256: {}\n\n",
        state.source_population_sha256
    ));
    match &state.taskgen_run_id {
        Some(run_id) => front.push_str(&format!("taskgen_run_id: {run_id}\n\n")),
        None => front.push_str("taskgen_run_id: null\n\n"),
    }
    front.push_str("Lineage and counts: [manifest.json](manifest.json).\n");
    front
}

fn size_category(train_rows: usize) -> &'static str {
    if train_rows < 1_000 {
        "n<1K"
    } else if train_rows < 10_000 {
        "1K<n<10K"
    } else if train_rows < 100_000 {
        "10K<n<100K"
    } else {
        "100K<n<1M"
    }
}

/// Double-quoted YAML scalar. Every control character is escaped, because YAML
/// forbids most of them raw and folds raw line breaks.
fn yaml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn pretty(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

const DETAILED_CARD_PARAGRAPH: &str = "Generated with --detailed: each assistant reply is a decision trace (## Decision trace, numbered Evidence / Hypothesis / Action / Verification / Conclusion steps, ## Final answer) from one teacher call. Proposed checks are text. No tool was executed and no model judged the answer.\n\n";

#[cfg(test)]
mod tests {
    use super::yaml_string;

    #[test]
    fn yaml_string_escapes_every_control_character() {
        assert_eq!(yaml_string("a\\b\"c"), r#""a\\b\"c""#);
        assert_eq!(yaml_string("Ops\r\nSFT\t"), r#""Ops\r\nSFT\t""#);
        assert_eq!(
            yaml_string("a\u{0}b\u{7}c\u{1b}d\u{7f}e\u{85}"),
            r#""a\u0000b\u0007c\u001Bd\u007Fe\u0085""#
        );
        assert_eq!(yaml_string("caf\u{e9}"), "\"caf\u{e9}\"");
        let all: String = (0u32..0xa0).filter_map(char::from_u32).collect();
        assert!(!yaml_string(&all).chars().any(char::is_control));
    }
}
