use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::error::{Error, Result};

/// Hub transport. `paths_present` uses POST `/paths-info`, which is the live
/// Hub API.
pub trait Hub: Send + Sync {
    fn ensure_private_dataset(
        &self,
        repo: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
    fn preupload(
        &self,
        repo: &str,
        files: &[HubFile],
    ) -> impl std::future::Future<Output = Result<Vec<UploadMode>>> + Send;
    fn upload_lfs(
        &self,
        repo: &str,
        files: &[LfsUpload],
    ) -> impl std::future::Future<Output = Result<()>> + Send;
    fn commit(
        &self,
        repo: &str,
        ndjson: String,
    ) -> impl std::future::Future<Output = Result<String>> + Send;
    /// Tags `revision` (a commit oid), not `main`, so a later commit by
    /// anyone else cannot receive the tag.
    fn tag(
        &self,
        repo: &str,
        revision: &str,
        tag: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;
    fn paths_present(
        &self,
        repo: &str,
        paths: &[String],
    ) -> impl std::future::Future<Output = Result<bool>> + Send;
}

#[derive(Clone)]
pub struct HubFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub size: u64,
    pub sample_b64: String,
}

pub struct LfsUpload {
    pub oid: String,
    pub size: u64,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadMode {
    Regular,
    Lfs,
}

pub struct HttpHub {
    client: reqwest::Client,
    endpoint: String,
    token: String,
}

impl HttpHub {
    pub fn new(endpoint: &str, token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            client,
            endpoint: endpoint.trim_end_matches('/').to_string(),
            token,
        })
    }

    fn auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.token)
    }

    fn status_error(&self, action: &str, status: reqwest::StatusCode) -> Error {
        Error::Unexpected(anyhow::anyhow!("hub {action} failed: {status}"))
    }

    fn on_endpoint(&self, href: &str) -> bool {
        match (
            reqwest::Url::parse(href),
            reqwest::Url::parse(&self.endpoint),
        ) {
            (Ok(href), Ok(endpoint)) => href.origin() == endpoint.origin(),
            _ => false,
        }
    }
}

/// 120 s base plus 1 s per 256 KiB (a ~2 Mbit/s floor), so a large
/// data/train.jsonl is not cut off by the 120 s default for Hub API calls.
fn lfs_put_timeout(len: usize) -> Duration {
    Duration::from_secs(120 + len as u64 / (256 * 1024))
}

impl Hub for HttpHub {
    async fn ensure_private_dataset(&self, repo: &str) -> Result<()> {
        match self.dataset_private(repo).await? {
            Some(true) => Ok(()),
            Some(false) => Err(Error::refuse(format!(
                "{repo} is public; refusing to upload"
            ))),
            None => self.create_private(repo).await,
        }
    }

    async fn preupload(&self, repo: &str, files: &[HubFile]) -> Result<Vec<UploadMode>> {
        let url = format!("{}/api/datasets/{repo}/preupload/main", self.endpoint);
        let payload = json!({
            "files": files.iter().map(|file| json!({
                "path": file.path,
                "size": file.size,
                "sample": file.sample_b64,
            })).collect::<Vec<_>>()
        });
        let response = self
            .auth(self.client.post(url))
            .json(&payload)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error("preupload", status));
        }
        let body: Value = response.json().await?;
        let listed = body
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unexpected(anyhow::anyhow!("hub preupload missing files")))?;
        let mut modes = Vec::with_capacity(files.len());
        for file in files {
            let mode = listed
                .iter()
                .find(|item| item.get("path").and_then(Value::as_str) == Some(file.path.as_str()))
                .and_then(|item| {
                    item.get("uploadMode")
                        .or_else(|| item.get("upload_mode"))
                        .and_then(Value::as_str)
                })
                .ok_or_else(|| {
                    Error::Unexpected(anyhow::anyhow!("hub preupload omitted {}", file.path))
                })?;
            modes.push(match mode {
                "regular" => UploadMode::Regular,
                "lfs" => UploadMode::Lfs,
                other => {
                    return Err(Error::Unexpected(anyhow::anyhow!(
                        "hub preupload mode {other} for {}",
                        file.path
                    )));
                }
            });
        }
        Ok(modes)
    }

    async fn upload_lfs(&self, repo: &str, files: &[LfsUpload]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let url = format!(
            "{}/datasets/{repo}.git/info/lfs/objects/batch",
            self.endpoint
        );
        let payload = json!({
            "operation": "upload",
            "transfers": ["basic"],
            "hash_algo": "sha256",
            "objects": files.iter().map(|file| json!({
                "oid": file.oid,
                "size": file.size,
            })).collect::<Vec<_>>()
        });
        let response = self
            .auth(self.client.post(&url))
            .header("Accept", "application/vnd.git-lfs+json")
            .header("Content-Type", "application/vnd.git-lfs+json")
            .json(&payload)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error("lfs batch", status));
        }
        let body: Value = response.json().await?;
        let objects = body
            .get("objects")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unexpected(anyhow::anyhow!("hub lfs batch missing objects")))?;
        for file in files {
            let object = objects
                .iter()
                .find(|item| item.get("oid").and_then(Value::as_str) == Some(file.oid.as_str()));
            let Some(object) = object else {
                return Err(Error::Unexpected(anyhow::anyhow!(
                    "hub lfs batch omitted {}",
                    file.oid
                )));
            };
            if let Some(error) = object.get("error") {
                if !error.is_null() {
                    return Err(Error::Unexpected(anyhow::anyhow!("hub lfs object error")));
                }
            }
            let actions = object.get("actions");
            let Some(upload) = actions.and_then(|a| a.get("upload")) else {
                continue;
            };
            let href = upload
                .get("href")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Unexpected(anyhow::anyhow!("hub lfs upload missing href")))?;
            self.put_action(href, upload.get("header"), &file.bytes)
                .await?;
            if let Some(verify) = actions.and_then(|a| a.get("verify")) {
                if let Some(href) = verify.get("href").and_then(Value::as_str) {
                    let verify_body = json!({"oid": file.oid, "size": file.size});
                    self.post_action(href, verify.get("header"), &verify_body)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn commit(&self, repo: &str, ndjson: String) -> Result<String> {
        let url = format!("{}/api/datasets/{repo}/commit/main", self.endpoint);
        let response = self
            .auth(self.client.post(url))
            .header("Content-Type", "application/x-ndjson")
            .body(ndjson)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error("commit", status));
        }
        let body: Value = response.json().await?;
        if let Some(oid) = body.get("commitOid").and_then(Value::as_str) {
            return Ok(oid.to_string());
        }
        if let Some(oid) = body.get("oid").and_then(Value::as_str) {
            return Ok(oid.to_string());
        }
        if let Some(url) = body
            .get("commitUrl")
            .or_else(|| body.get("commit_url"))
            .and_then(Value::as_str)
        {
            if let Some(oid) = url.rsplit('/').next() {
                if !oid.is_empty() {
                    return Ok(oid.to_string());
                }
            }
        }
        Err(Error::Unexpected(anyhow::anyhow!(
            "hub commit response missing commitOid"
        )))
    }

    async fn tag(&self, repo: &str, revision: &str, tag: &str) -> Result<()> {
        let url = format!("{}/api/datasets/{repo}/tag/{revision}", self.endpoint);
        let response = self
            .auth(self.client.post(url))
            .json(&json!({"tag": tag}))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() || status.as_u16() == 409 {
            return Ok(());
        }
        Err(self.status_error("tag", status))
    }

    async fn paths_present(&self, repo: &str, paths: &[String]) -> Result<bool> {
        let url = format!("{}/api/datasets/{repo}/paths-info/main", self.endpoint);
        let response = self
            .auth(self.client.post(url))
            .json(&json!({"paths": paths}))
            .send()
            .await?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Ok(false);
        }
        if !status.is_success() {
            return Err(self.status_error("paths-info", status));
        }
        let body: Value = response.json().await?;
        let listed = if let Some(array) = body.as_array() {
            array
        } else if let Some(array) = body.get("paths").and_then(Value::as_array) {
            array
        } else {
            return Ok(false);
        };
        Ok(paths.iter().all(|path| {
            listed.iter().any(|item| {
                item.get("path").and_then(Value::as_str) == Some(path.as_str())
                    && item.get("type").and_then(Value::as_str) != Some("missing")
            })
        }))
    }
}

impl HttpHub {
    async fn dataset_private(&self, repo: &str) -> Result<Option<bool>> {
        let url = format!("{}/api/datasets/{repo}", self.endpoint);
        let response = self.auth(self.client.get(url)).send().await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error("dataset lookup", status));
        }
        let body: Value = response.json().await?;
        let private = body
            .get("private")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                Error::Unexpected(anyhow::anyhow!("hub dataset response missing private"))
            })?;
        Ok(Some(private))
    }

    async fn create_private(&self, repo: &str) -> Result<()> {
        let (organization, name) = split_repo(repo)?;
        let url = format!("{}/api/repos/create", self.endpoint);
        let response = self
            .auth(self.client.post(url))
            .json(&json!({
                "type": "dataset",
                "name": name,
                "organization": organization,
                "private": true,
            }))
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        if status.as_u16() == 409 {
            return match self.dataset_private(repo).await? {
                Some(true) => Ok(()),
                Some(false) => Err(Error::refuse(format!(
                    "{repo} is public; refusing to upload"
                ))),
                None => Err(Error::Unexpected(anyhow::anyhow!(
                    "hub create conflict for {repo}"
                ))),
            };
        }
        Err(self.status_error("create", status))
    }

    async fn put_action(&self, href: &str, headers: Option<&Value>, bytes: &[u8]) -> Result<()> {
        let mut request = self.client.put(href).timeout(lfs_put_timeout(bytes.len()));
        request = apply_action_headers(request, headers);
        let response = request.body(bytes.to_vec()).send().await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(self.status_error("lfs upload", response.status()))
    }

    /// The verify endpoint lives on the Hub and needs repo write auth. The
    /// token is added only for a same-origin href whose action headers do not
    /// already authorize it.
    async fn post_action(&self, href: &str, headers: Option<&Value>, body: &Value) -> Result<()> {
        let mut request = self.client.post(href).json(body);
        let has_auth = headers.and_then(Value::as_object).is_some_and(|object| {
            object
                .keys()
                .any(|key| key.eq_ignore_ascii_case("authorization"))
        });
        if !has_auth && self.on_endpoint(href) {
            request = self.auth(request);
        }
        request = apply_action_headers(request, headers);
        let response = request.send().await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(self.status_error("lfs verify", response.status()))
    }
}

fn apply_action_headers(
    mut request: reqwest::RequestBuilder,
    headers: Option<&Value>,
) -> reqwest::RequestBuilder {
    if let Some(object) = headers.and_then(Value::as_object) {
        for (key, value) in object {
            if let Some(text) = value.as_str() {
                request = request.header(key.as_str(), text);
            }
        }
    }
    request
}

pub fn split_repo(repo: &str) -> Result<(&str, &str)> {
    let Some((organization, name)) = repo.split_once('/') else {
        return Err(Error::refuse("--hf-repo must be owner/name"));
    };
    if !repo_part_ok(organization) || !repo_part_ok(name) || repo.matches('/').count() != 1 {
        return Err(Error::refuse("--hf-repo must be owner/name"));
    }
    Ok((organization, name))
}

fn repo_part_ok(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

pub fn sample_b64(bytes: &[u8]) -> String {
    let end = bytes.len().min(512);
    base64::engine::general_purpose::STANDARD.encode(&bytes[..end])
}

pub fn content_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lfs_put_timeout_scales_with_body_size() {
        assert_eq!(lfs_put_timeout(0), Duration::from_secs(120));
        assert_eq!(lfs_put_timeout(1 << 30), Duration::from_secs(120 + 4096));
    }

    #[test]
    fn only_same_origin_hrefs_are_on_the_endpoint() {
        let hub = HttpHub::new("https://huggingface.co", "t".into()).unwrap();
        assert!(hub.on_endpoint("https://huggingface.co/datasets/o/n.git/info/lfs/objects/verify"));
        assert!(!hub.on_endpoint("https://huggingface.co.evil.test/verify"));
        assert!(!hub.on_endpoint("http://huggingface.co/verify"));
        assert!(!hub.on_endpoint("https://s3.amazonaws.com/bucket?X-Amz-Signature=x"));
        assert!(!hub.on_endpoint("not a url"));
    }
}
