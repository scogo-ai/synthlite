use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{self, LoadMode};
use crate::error::{Error, Result};
use crate::gate::{self, GateOpts};
use crate::hub::{self, HttpHub, Hub, HubFile, LfsUpload, UploadMode};
use crate::identity;
use crate::store;

pub struct PushOpts {
    pub out: PathBuf,
    pub config: Option<PathBuf>,
    pub hf_repo: Option<String>,
    pub hf_republish: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PublishReceipt {
    schema_version: String,
    repo_id: String,
    endpoint: String,
    commit_oid: String,
    /// None while the commit has landed but the tag has not; a rerun then
    /// only retries the tag.
    tag: Option<String>,
    files: BTreeMap<String, String>,
}

pub async fn run(opts: PushOpts) -> Result<()> {
    let token = hub_token()?;
    let repo = opts
        .hf_repo
        .clone()
        .filter(|repo| !repo.is_empty())
        .ok_or_else(|| Error::refuse("--hf-repo is required"))?;
    hub::split_repo(&repo)?;
    let endpoint = std::env::var("HF_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string());
    let endpoint = endpoint.trim_end_matches('/').to_string();
    let hub = HttpHub::new(&endpoint, token)?;
    push_with(&hub, &opts, &repo, &endpoint).await
}

pub async fn push_with(hub: &impl Hub, opts: &PushOpts, repo: &str, endpoint: &str) -> Result<()> {
    let _lock = store::lock_dir(&opts.out)?;
    prepare_local(opts, repo)?;
    let files = collect_files(&opts.out)?;
    let manifest_sha = files
        .iter()
        .find(|file| file.path == "manifest.json")
        .map(|file| file.sha256.clone())
        .ok_or_else(|| Error::refuse("manifest.json is missing"))?;
    let tag = format!("synthlite-{}", &manifest_sha[..12]);
    hub.ensure_private_dataset(repo).await?;
    if !opts.hf_republish {
        if let Some(receipt) = read_receipt(&opts.out)? {
            if receipt.repo_id == repo
                && receipt.endpoint.trim_end_matches('/') == endpoint.trim_end_matches('/')
                && receipt.files == file_digests(&files)
            {
                let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
                if hub.paths_present(repo, &paths).await? {
                    if let Some(tag) = &receipt.tag {
                        eprintln!(
                            "push {repo} unchanged commit={} tag={tag}",
                            receipt.commit_oid
                        );
                        return Ok(());
                    }
                    let oid = receipt.commit_oid.clone();
                    hub.tag(repo, &oid, &tag).await?;
                    write_receipt(
                        &opts.out,
                        &PublishReceipt {
                            tag: Some(tag.clone()),
                            ..receipt
                        },
                    )?;
                    eprintln!("push {repo} commit={oid} tag={tag}");
                    return Ok(());
                }
            }
        }
    }
    let modes = hub.preupload(repo, &files).await?;
    let lfs_files: Vec<LfsUpload> = files
        .iter()
        .zip(modes.iter())
        .filter_map(|(file, mode)| match mode {
            UploadMode::Lfs => Some(LfsUpload {
                oid: file.sha256.clone(),
                size: file.size,
                bytes: file.bytes.clone(),
            }),
            UploadMode::Regular => None,
        })
        .collect();
    hub.upload_lfs(repo, &lfs_files).await?;
    let train_rows = count_lines(&opts.out.join("data/train.jsonl"))?;
    let validation_rows = if opts.out.join("data/validation.jsonl").is_file() {
        count_lines(&opts.out.join("data/validation.jsonl"))?
    } else {
        0
    };
    let summary = format!(
        "synthlite {}: {train_rows} train, {validation_rows} validation",
        &manifest_sha[..12]
    );
    let ndjson = build_ndjson(&summary, &files, &modes)?;
    let oid = hub.commit(repo, ndjson).await?;
    let mut receipt = PublishReceipt {
        schema_version: "synthlite.publish.v1".into(),
        repo_id: repo.to_string(),
        endpoint: endpoint.trim_end_matches('/').to_string(),
        commit_oid: oid.clone(),
        tag: None,
        files: file_digests(&files),
    };
    write_receipt(&opts.out, &receipt)?;
    hub.tag(repo, &oid, &tag).await?;
    receipt.tag = Some(tag.clone());
    write_receipt(&opts.out, &receipt)?;
    eprintln!("push {repo} commit={oid} tag={tag}");
    Ok(())
}

fn write_receipt(out: &Path, receipt: &PublishReceipt) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(receipt)?;
    bytes.push(b'\n');
    store::write_atomic(&out.join("publish.json"), &bytes)
}

fn hub_token() -> Result<String> {
    if let Ok(token) = std::env::var("HF_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if let Ok(token) = std::env::var("HUGGING_FACE_HUB_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    Err(Error::refuse("HF_TOKEN is required"))
}

fn prepare_local(opts: &PushOpts, repo: &str) -> Result<()> {
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
            "torn tail in the output directory; resume generate before push",
        ));
    }
    let state = store::read_state(&opts.out)?.ok_or_else(|| {
        Error::refuse("generation_complete is false; pending work is not publishable")
    })?;
    if !state.generation_complete {
        return Err(Error::refuse(
            "generation_complete is false; pending work is not publishable",
        ));
    }
    let train = opts.out.join("data/train.jsonl");
    if !train.exists() {
        let name = repo.split_once('/').map(|(_, name)| name).unwrap_or(repo);
        gate::run_locked(GateOpts {
            out: opts.out.clone(),
            config: opts.config.clone(),
            pretty_name_fallback: name.to_string(),
        })?;
    }
    verify_receipt(&opts.out, opts.config.as_deref())?;
    Ok(())
}

fn verify_receipt(out: &Path, config: Option<&Path>) -> Result<()> {
    let gate_path = out.join("gate.json");
    if !gate_path.is_file() {
        return Err(Error::refuse("gate receipt is missing; run gate again"));
    }
    let receipt: Value = serde_json::from_slice(&fs::read(&gate_path)?)
        .map_err(|_| Error::refuse("gate receipt is stale; run gate again"))?;
    let loaded = config::load(config, LoadMode::Offline)?;
    let gate_hash = {
        let path = loaded.gate.exclude_prompt_hashes.as_deref();
        let hash = match path {
            Some(path) => {
                let bytes = fs::read(path).map_err(|err| {
                    Error::refuse(format!("exclude_prompt_hashes {}: {err}", path.display()))
                })?;
                Some(identity::sha256_hex(&bytes))
            }
            None => None,
        };
        let value = config::gate_config_value(&loaded.gate, hash.as_deref());
        identity::hash_json("", &value)?
    };
    let rows_bytes = if out.join("rows.jsonl").is_file() {
        fs::read(out.join("rows.jsonl"))?
    } else {
        Vec::new()
    };
    let train_bytes = fs::read(out.join("data/train.jsonl"))
        .map_err(|_| Error::refuse("data/train.jsonl is missing; run gate again"))?;
    let validation_path = out.join("data/validation.jsonl");
    let validation_sha = if validation_path.is_file() {
        Value::String(identity::sha256_hex(&fs::read(&validation_path)?))
    } else {
        Value::Null
    };
    let manifest_bytes = fs::read(out.join("manifest.json"))?;
    let readme_bytes = fs::read(out.join("README.md"))?;
    let checks = [
        (
            "rows_sha256",
            Value::String(identity::sha256_hex(&rows_bytes)),
        ),
        ("gate_config_hash", Value::String(gate_hash)),
        (
            "train_sha256",
            Value::String(identity::sha256_hex(&train_bytes)),
        ),
        ("validation_sha256", validation_sha),
        (
            "manifest_sha256",
            Value::String(identity::sha256_hex(&manifest_bytes)),
        ),
        (
            "readme_sha256",
            Value::String(identity::sha256_hex(&readme_bytes)),
        ),
    ];
    for (key, expected) in checks {
        if receipt.get(key) != Some(&expected) {
            return Err(Error::refuse("gate receipt is stale; run gate again"));
        }
    }
    Ok(())
}

fn collect_files(out: &Path) -> Result<Vec<HubFile>> {
    let mut paths = vec![out.join("data/train.jsonl")];
    if out.join("data/validation.jsonl").is_file() {
        paths.push(out.join("data/validation.jsonl"));
    }
    paths.push(out.join("README.md"));
    paths.push(out.join("manifest.json"));
    let mut files = Vec::new();
    for path in paths {
        let bytes = fs::read(&path)
            .map_err(|err| Error::refuse(format!("missing {}: {err}", path.display())))?;
        let rel = if path.ends_with("train.jsonl") {
            "data/train.jsonl".to_string()
        } else if path.ends_with("validation.jsonl") {
            "data/validation.jsonl".to_string()
        } else if path.ends_with("README.md") {
            "README.md".to_string()
        } else {
            "manifest.json".to_string()
        };
        files.push(HubFile {
            sha256: identity::sha256_hex(&bytes),
            size: bytes.len() as u64,
            sample_b64: hub::sample_b64(&bytes),
            bytes,
            path: rel,
        });
    }
    Ok(files)
}

fn file_digests(files: &[HubFile]) -> BTreeMap<String, String> {
    files
        .iter()
        .map(|file| (file.path.clone(), file.sha256.clone()))
        .collect()
}

fn build_ndjson(summary: &str, files: &[HubFile], modes: &[UploadMode]) -> Result<String> {
    let mut out = String::new();
    let header = json!({
        "key": "header",
        "value": {"summary": summary, "description": ""}
    });
    out.push_str(&serde_json::to_string(&header)?);
    out.push('\n');
    for (file, mode) in files.iter().zip(modes.iter()) {
        let line = match mode {
            UploadMode::Regular => json!({
                "key": "file",
                "value": {
                    "path": file.path,
                    "encoding": "base64",
                    "content": hub::content_b64(&file.bytes),
                }
            }),
            UploadMode::Lfs => json!({
                "key": "lfsFile",
                "value": {
                    "path": file.path,
                    "algo": "sha256",
                    "oid": file.sha256,
                    "size": file.size,
                }
            }),
        };
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

fn count_lines(path: &Path) -> Result<u64> {
    if !path.is_file() {
        return Ok(0);
    }
    let text = fs::read(path)?;
    Ok(text.iter().filter(|byte| **byte == b'\n').count() as u64)
}

fn read_receipt(out: &Path) -> Result<Option<PublishReceipt>> {
    let path = out.join("publish.json");
    if !path.is_file() {
        return Ok(None);
    }
    match serde_json::from_slice(&fs::read(path)?) {
        Ok(receipt) => Ok(Some(receipt)),
        Err(_) => Ok(None),
    }
}
