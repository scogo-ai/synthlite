use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::identity::sha256_hex;

pub const STATE_SCHEMA: &str = "synthlite.state.v1";
pub const LOCK_FILE: &str = ".synthlite.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParkedKey {
    pub key: String,
    pub reason: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub schema_version: String,
    pub synthlite_version: String,
    pub generator_config_hash: String,
    pub generator_config: Value,
    pub source_population_sha256: String,
    pub taskgen_run_id: Option<String>,
    #[serde(default)]
    pub seed_count: u64,
    pub created_at: String,
    pub generation_complete: bool,
    #[serde(default)]
    pub parked_keys: Vec<ParkedKey>,
    /// Earlier `[generation]` configs of this run, oldest first. Rows keep the
    /// hash they were written under; a resume under a new config moves the
    /// previous one here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_history: Vec<ConfigEpoch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEpoch {
    pub generator_config_hash: String,
    pub generator_config: Value,
    pub superseded_at: String,
}

impl State {
    /// True when rows written under `hash` belong to this run.
    pub fn accepts(&self, hash: &str) -> bool {
        self.generator_config_hash == hash
            || self
                .config_history
                .iter()
                .any(|epoch| epoch.generator_config_hash == hash)
    }

    /// Makes `hash` the current config. The previous one is kept in
    /// `config_history`. Returns the changed fields, empty when nothing changed.
    pub fn adopt_config(&mut self, hash: &str, config: &Value, at: &str) -> Vec<String> {
        if self.generator_config_hash == hash {
            self.generator_config = config.clone();
            return Vec::new();
        }
        let changes = config_changes(&self.generator_config, config);
        self.config_history
            .retain(|epoch| epoch.generator_config_hash != hash);
        self.config_history.push(ConfigEpoch {
            generator_config_hash: std::mem::replace(
                &mut self.generator_config_hash,
                hash.to_string(),
            ),
            generator_config: std::mem::replace(&mut self.generator_config, config.clone()),
            superseded_at: at.to_string(),
        });
        changes
    }
}

/// One `field: old -> new` entry per top-level key that differs. Long strings
/// (the system message) are shown as changed, not quoted.
fn config_changes(old: &Value, new: &Value) -> Vec<String> {
    let show = |value: Option<&Value>| match value {
        Some(Value::String(text)) if text.len() > 40 => "<text>".to_string(),
        Some(value) => value.to_string(),
        None => "null".to_string(),
    };
    let mut keys: Vec<&String> = Vec::new();
    for value in [old, new] {
        if let Some(map) = value.as_object() {
            for key in map.keys() {
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
    }
    keys.sort();
    keys.into_iter()
        .filter(|key| old.get(key.as_str()) != new.get(key.as_str()))
        .map(|key| {
            format!(
                "{key}: {} -> {}",
                show(old.get(key.as_str())),
                show(new.get(key.as_str()))
            )
        })
        .collect()
}

pub struct Scan {
    pub committed: BTreeMap<String, String>,
    pub failed_attempts: BTreeMap<String, u32>,
}

impl Scan {
    pub fn failed_ids(&self) -> HashSet<String> {
        self.failed_attempts.keys().cloned().collect()
    }
}

/// Takes an exclusive advisory lock on `DIR/.synthlite.lock`. The lock is
/// released when the returned file is dropped (or the process exits).
pub fn lock_dir(dir: &Path) -> Result<File> {
    if !dir.is_dir() {
        return Err(Error::refuse(format!("{} does not exist", dir.display())));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(Error::refuse(format!(
            "another synthlite process is using {}",
            dir.display()
        ))),
        Err(fs::TryLockError::Error(err)) => Err(err.into()),
    }
}

pub fn is_synthlite_run(dir: &Path) -> bool {
    dir.join("state.json").is_file() || dir.join("rows.jsonl").is_file()
}

/// Checks that an out dir can be resumed under the current config. A different
/// `[generation]` config is allowed: it returns `true`, and the caller records
/// the switch with `State::adopt_config`. Only a `--detailed` mode switch
/// refuses, because it changes the shape of every row. `detailed` is the
/// current mode.
pub fn assert_resumable(dir: &Path, hash: &str, detailed: bool) -> Result<bool> {
    let state = read_state(dir)?;
    let mut changed = false;
    if let Some(state) = &state {
        if state.schema_version != STATE_SCHEMA {
            return Err(Error::refuse(format!(
                "{} state.json is not {STATE_SCHEMA}; pass a new --out",
                dir.display()
            )));
        }
        if state.generator_config_hash != hash {
            mode_check(dir, state, detailed)?;
            changed = true;
        }
    }
    for (index, line) in complete_lines(&dir.join("rows.jsonl"))?
        .into_iter()
        .enumerate()
    {
        let value: Value = serde_json::from_slice(line.trim_end()).map_err(|_| {
            Error::refuse(format!(
                "{}:{}: unreadable row",
                dir.join("rows.jsonl").display(),
                index + 1
            ))
        })?;
        let row_hash = value
            .get("generator_config_hash")
            .and_then(Value::as_str)
            .unwrap_or("");
        let known = match &state {
            Some(state) => row_hash == hash || state.accepts(row_hash),
            None => row_hash == hash,
        };
        if !known {
            return Err(hash_mismatch(dir));
        }
    }
    Ok(changed)
}

pub fn hash_mismatch(dir: &Path) -> Error {
    Error::refuse(format!(
        "generator_config_hash does not match {}; pass a new --out",
        dir.display()
    ))
}

fn mode_check(dir: &Path, state: &State, detailed: bool) -> Result<()> {
    let was_detailed = state.generator_config.get("detailed") == Some(&Value::Bool(true));
    match (was_detailed, detailed) {
        (true, false) => Err(Error::refuse(format!(
            "{} was generated with --detailed; rerun with --detailed or pass a new --out",
            dir.display()
        ))),
        (false, true) => Err(Error::refuse(format!(
            "{} was generated without --detailed; remove --detailed and [generation].detailed, or pass a new --out",
            dir.display()
        ))),
        _ => Ok(()),
    }
}

pub fn read_state(dir: &Path) -> Result<Option<State>> {
    let path = dir.join("state.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path)?;
    serde_json::from_slice(&bytes).map(Some).map_err(|_| {
        Error::refuse(format!(
            "{} is not readable; pass a new --out",
            path.display()
        ))
    })
}

pub fn write_state(dir: &Path, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    let mut bytes = bytes;
    bytes.push(b'\n');
    write_atomic(&dir.join("state.json"), &bytes)
}

pub fn repair_outputs(dir: &Path) -> Result<()> {
    repair_torn(&dir.join("rows.jsonl"), &dir.join("rows.partial"))?;
    repair_torn(
        &dir.join("rows.errors.jsonl"),
        &dir.join("rows.errors.partial"),
    )?;
    Ok(())
}

pub fn has_torn_tail(path: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)?;
    Ok(byte[0] != b'\n')
}

pub fn scan(dir: &Path) -> Result<Scan> {
    let mut committed = BTreeMap::new();
    for (index, line) in complete_lines(&dir.join("rows.jsonl"))?
        .into_iter()
        .enumerate()
    {
        let value: Value = serde_json::from_slice(line.trim_end()).map_err(|_| {
            Error::refuse(format!(
                "{}:{}: unreadable row",
                dir.join("rows.jsonl").display(),
                index + 1
            ))
        })?;
        let id = value
            .get("source_task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::refuse(format!(
                    "{}:{}: missing source_task_id",
                    dir.join("rows.jsonl").display(),
                    index + 1
                ))
            })?
            .to_string();
        let hash = value
            .get("generator_config_hash")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        committed.insert(id, hash);
    }
    let mut failed_attempts = BTreeMap::new();
    for (index, line) in complete_lines(&dir.join("rows.errors.jsonl"))?
        .into_iter()
        .enumerate()
    {
        let value: Value = serde_json::from_slice(line.trim_end()).map_err(|_| {
            Error::refuse(format!(
                "{}:{}: unreadable error",
                dir.join("rows.errors.jsonl").display(),
                index + 1
            ))
        })?;
        let id = value
            .get("source_task_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if id.is_empty() || committed.contains_key(&id) {
            continue;
        }
        let attempt = value.get("attempt").and_then(Value::as_u64).unwrap_or(1) as u32;
        failed_attempts
            .entry(id)
            .and_modify(|current: &mut u32| *current = (*current).max(attempt))
            .or_insert(attempt);
    }
    Ok(Scan {
        committed,
        failed_attempts,
    })
}

pub struct Appender {
    file: File,
    path: PathBuf,
}

impl Appender {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?;
        if !existed {
            if let Some(parent) = path.parent() {
                fsync_dir(parent)?;
            }
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn append_line(&mut self, line: &[u8]) -> Result<()> {
        let start = self.file.metadata()?.len();
        if let Err(err) = self.file.write_all(line) {
            truncate_to(&self.path, start)?;
            return Err(io_write(err, &self.path));
        }
        if let Err(err) = self.file.sync_all() {
            if is_enospc(&err) {
                truncate_to(&self.path, start)?;
            }
            return Err(io_write(err, &self.path));
        }
        Ok(())
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| Error::refuse(format!("missing file name {}", path.display())))?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp = parent.join(tmp_name);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(parent)?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(sha256_hex(b""));
    }
    Ok(sha256_hex(&fs::read(path)?))
}

pub fn create_out_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    fsync_dir(parent.unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

pub fn fsync_dir(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    file.sync_all()?;
    Ok(())
}

fn repair_torn(path: &Path, partial: &Path) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let data = fs::read(path)?;
    if data.is_empty() || data.last() == Some(&b'\n') {
        return Ok(false);
    }
    let cut = data
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    // Quarantine first (appended, one fragment per line), so a crash before
    // the truncate can only duplicate a fragment, never lose it.
    {
        let mut file = OpenOptions::new().create(true).append(true).open(partial)?;
        file.write_all(&data[cut..])?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    if let Some(parent) = path.parent() {
        fsync_dir(parent)?;
    }
    truncate_to(path, cut as u64)?;
    Ok(true)
}

fn complete_lines(path: &Path) -> Result<Vec<Vec<u8>>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let data = fs::read(path)?;
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let end = if data.last() == Some(&b'\n') {
        data.len()
    } else {
        data.iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(0)
    };
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (index, byte) in data[..end].iter().enumerate() {
        if *byte == b'\n' {
            let line = data[start..=index].to_vec();
            if line.iter().any(|b| !b.is_ascii_whitespace()) {
                lines.push(line);
            }
            start = index + 1;
        }
    }
    Ok(lines)
}

fn truncate_to(path: &Path, len: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

fn io_write(err: std::io::Error, path: &Path) -> Error {
    if is_enospc(&err) {
        Error::Unexpected(anyhow::anyhow!(
            "disk full writing {}; kept the last complete line",
            path.display()
        ))
    } else {
        Error::Unexpected(anyhow::anyhow!("writing {}: {err}", path.display()))
    }
}

fn is_enospc(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::StorageFull || err.raw_os_error() == Some(28)
}

trait TrimEnd {
    fn trim_end(&self) -> &[u8];
}

impl TrimEnd for Vec<u8> {
    fn trim_end(&self) -> &[u8] {
        if self.last() == Some(&b'\n') {
            let end = if self.len() >= 2 && self[self.len() - 2] == b'\r' {
                self.len() - 2
            } else {
                self.len() - 1
            };
            &self[..end]
        } else {
            self
        }
    }
}
