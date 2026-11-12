use crate::queue::{Destination, Manifest, Queue};
use anyhow::{bail, ensure, Context, Result};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}
// Never Debug/log tokens or upload session URLs.
#[derive(Serialize, Deserialize)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}
#[derive(Serialize, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    #[serde(rename = "displayName")]
    pub name: String,
}
const SCOPE: &str = "offline_access User.Read Files.ReadWrite";
fn authority(d: &Destination, suffix: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/{}",
        d.tenant_id, suffix
    )
}
#[derive(Debug)]
pub struct RetryDelay(pub u64);
impl std::fmt::Display for RetryDelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Microsoft requested a retry after {} seconds", self.0)
    }
}
impl std::error::Error for RetryDelay {}
async fn checked_json(r: reqwest::Response) -> Result<Value> {
    let status = r.status();
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
        let seconds = r
            .headers()
            .get("Retry-After")
            .and_then(|s| s.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60)
            .max(1);
        return Err(RetryDelay(seconds).into());
    }
    ensure!(status.is_success(), "Microsoft HTTP {}", status.as_u16());
    Ok(r.json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid Microsoft response"))?)
}
pub async fn begin(d: &Destination) -> Result<DeviceCode> {
    d.validate()?;
    let r = client()?
        .post(authority(d, "devicecode"))
        .form(&[("client_id", d.client_id.as_str()), ("scope", SCOPE)])
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Microsoft sign-in connection failed"))?;
    Ok(serde_json::from_value(checked_json(r).await?)?)
}
pub async fn poll(d: &Destination, code: &str) -> Result<Option<Token>> {
    let r = client()?
        .post(authority(d, "token"))
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", d.client_id.as_str()),
            ("device_code", code),
        ])
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Microsoft sign-in connection failed"))?;
    let status = r.status();
    let v: Value = r
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid sign-in response"))?;
    if status.is_success() {
        return Ok(Some(serde_json::from_value(v)?));
    }
    match v["error"].as_str() {
        Some("authorization_pending") => Ok(None),
        Some("slow_down") => bail!("slow_down"),
        Some("authorization_declined") => bail!("Sign-in declined"),
        Some("expired_token") => bail!("Sign-in expired"),
        _ => bail!("Microsoft sign-in rejected; check app registration/consent"),
    }
}
pub async fn refresh(d: &Destination, refresh: &str) -> Result<Token> {
    let r = client()?
        .post(authority(d, "token"))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", d.client_id.as_str()),
            ("refresh_token", refresh),
            ("scope", SCOPE),
        ])
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Microsoft token refresh connection failed"))?;
    Ok(serde_json::from_value(checked_json(r).await?)?)
}
pub async fn profile(access: &str) -> Result<Profile> {
    Ok(serde_json::from_value(
        checked_json(
            client()?
                .get("https://graph.microsoft.com/v1.0/me?$select=id,displayName")
                .bearer_auth(access)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Microsoft profile connection failed"))?,
        )
        .await?,
    )?)
}

pub struct Graph {
    client: Client,
    token: String,
}
impl Graph {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: client()?,
            token,
        })
    }
    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        self.client
            .get(format!("https://graph.microsoft.com/v1.0/{path}"))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("SharePoint connection failed"))
    }
    async fn post(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        self.client
            .post(format!("https://graph.microsoft.com/v1.0/{path}"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("SharePoint connection failed"))
    }
    pub async fn validate_destination(&self, d: &Destination) -> Result<()> {
        d.validate()?;
        let item = checked_json(
            self.get(&format!("drives/{}/items/{}", d.drive_id, d.folder_id))
                .await?,
        )
        .await?;
        ensure!(
            item["folder"].is_object(),
            "Upload destination is not a folder"
        );
        Ok(())
    }
    async fn folder(&self, m: &Manifest) -> Result<String> {
        let base = format!(
            "drives/{}/items/{}",
            m.destination.drive_id, m.destination.folder_id
        );
        let name = m.capture_id.to_string();
        let r = self
            .post(
                &format!("{base}/children"),
                json!({"name":name,"folder":{},"@microsoft.graph.conflictBehavior":"fail"}),
            )
            .await?;
        let item = if r.status() == StatusCode::CONFLICT {
            checked_json(self.get(&format!("{base}:/{name}")).await?).await?
        } else {
            checked_json(r).await?
        };
        ensure!(item["folder"].is_object(), "Capture path is not a folder");
        Ok(item["id"]
            .as_str()
            .context("Missing folder identity")?
            .into())
    }
    async fn existing_matches(&self, path: &str, bytes: u64, sha: &str) -> Result<bool> {
        let r = self.get(path).await?;
        if r.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        let item = checked_json(r).await?;
        ensure!(
            item["size"].as_u64() == Some(bytes),
            "Remote file conflict; review required"
        );
        // Download from Graph's trusted preauthenticated URL, never forward the bearer token.
        let download = item["@microsoft.graph.downloadUrl"]
            .as_str()
            .context("Missing remote verification URL")?;
        validate_upload_url(download)?;
        let mut r = self
            .client
            .get(download)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Remote verification connection failed"))?;
        ensure!(r.status().is_success(), "Remote verification failed");
        let mut h = Sha256::new();
        let mut n = 0;
        while let Some(chunk) = r
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Remote verification interrupted"))?
        {
            n += chunk.len() as u64;
            ensure!(n <= bytes, "Remote verification size mismatch");
            h.update(chunk);
        }
        ensure!(
            n == bytes && format!("{:x}", h.finalize()) == sha,
            "Remote file conflict; review required"
        );
        Ok(true)
    }
    pub async fn upload(&self, folder: &Path, recorder: &str) -> Result<()> {
        let m = Queue::manifest(folder)?;
        m.destination.validate()?;
        ensure!(
            m.recorder_id == recorder,
            "Sign in as the original recorder to upload this capture"
        );
        Queue::verify(folder, &m)?;
        let mut state = Queue::state(folder)?;
        if state.status == "Uploaded" {
            return Ok(());
        }
        state.status = "Uploading".into();
        state.error = None;
        state.attempts += 1;
        Queue::save_state(folder, &state)?;
        if state.remote_folder.is_none() {
            state.remote_folder = Some(self.folder(&m).await?);
            Queue::save_state(folder, &state)?;
        }
        let remote = state.remote_folder.clone().unwrap();
        let mut files = m.files.clone();
        let manifest_path = folder.join("manifest.json");
        files.push(crate::queue::PackageFile {
            name: "manifest.json".into(),
            bytes: manifest_path.metadata()?.len(),
            sha256: crate::queue::hash(&manifest_path)?,
        });
        for (i, file) in files.iter().enumerate().skip(state.file_index) {
            let path = format!(
                "drives/{}/items/{}:/{}",
                m.destination.drive_id, remote, file.name
            );
            if self
                .existing_matches(&path, file.bytes, &file.sha256)
                .await?
            {
                state.file_index = i + 1;
                state.upload_url = None;
                state.offset = 0;
                Queue::save_state(folder, &state)?;
                continue;
            }
            if file.bytes == 0 {
                bail!("Empty capture file requires review");
            }
            if let Some(url) = state.upload_url.clone() {
                validate_upload_url(&url)?;
                let r = self
                    .client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Upload status connection failed"))?;
                if r.status() == StatusCode::NOT_FOUND || r.status() == StatusCode::GONE {
                    state.upload_url = None;
                    state.offset = 0;
                } else {
                    state.offset = next_offset(&checked_json(r).await?, file.bytes)?;
                }
            }
            if state.upload_url.is_none() {
                let v = checked_json(
                    self.post(
                        &format!("{path}:/createUploadSession"),
                        json!({"item":{"@microsoft.graph.conflictBehavior":"fail"}}),
                    )
                    .await?,
                )
                .await?;
                let url = v["uploadUrl"]
                    .as_str()
                    .context("Missing upload URL")?
                    .to_string();
                validate_upload_url(&url)?;
                state.upload_url = Some(url);
                state.offset = 0;
            }
            Queue::save_state(folder, &state)?;
            let mut input = tokio::fs::File::open(folder.join(&file.name)).await?;
            let mut committed = false;
            while state.offset < file.bytes {
                input.seek(std::io::SeekFrom::Start(state.offset)).await?;
                // 10 MiB = 32 x 320 KiB, below Graph's 60 MiB limit.
                let n = (file.bytes - state.offset).min(10 * 1024 * 1024) as usize;
                let mut buf = vec![0; n];
                input.read_exact(&mut buf).await?;
                let r = self
                    .client
                    .put(state.upload_url.as_ref().unwrap())
                    .header("Content-Length", n)
                    .header(
                        "Content-Range",
                        format!(
                            "bytes {}-{}/{}",
                            state.offset,
                            state.offset + n as u64 - 1,
                            file.bytes
                        ),
                    )
                    .body(buf)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Upload interrupted; queued for retry"))?;
                let status = r.status();
                let v = checked_json(r).await?;
                if status == StatusCode::ACCEPTED {
                    let next = next_offset(&v, file.bytes)?;
                    ensure!(next > state.offset, "Upload made no progress");
                    state.offset = next;
                } else {
                    ensure!(
                        status == StatusCode::OK || status == StatusCode::CREATED,
                        "Unexpected upload result"
                    );
                    ensure!(
                        v["size"].as_u64() == Some(file.bytes),
                        "Committed upload size mismatch"
                    );
                    state.offset = file.bytes;
                    committed = true;
                }
                Queue::save_state(folder, &state)?;
            }
            ensure!(committed, "Upload commit unverified; retry required");
            state.file_index = i + 1;
            state.upload_url = None;
            state.offset = 0;
            Queue::save_state(folder, &state)?;
        }
        state.status = "Uploaded".into();
        state.error = None;
        Queue::save_state(folder, &state)?;
        Ok(())
    }
}
pub fn validate_upload_url(s: &str) -> Result<()> {
    let u = url::Url::parse(s)?;
    let h = u.host_str().unwrap_or("");
    ensure!(
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
            && ["sharepoint.com", "1drv.com", "sharepointonline.com"]
                .iter()
                .any(|d| h == *d || h.ends_with(&format!(".{d}"))),
        "Untrusted Microsoft transfer URL"
    );
    Ok(())
}
fn next_offset(v: &Value, size: u64) -> Result<u64> {
    let r = v["nextExpectedRanges"]
        .as_array()
        .context("Missing upload ranges")?;
    ensure!(r.len() == 1, "Nonsequential upload requires review");
    let start = r[0]
        .as_str()
        .context("Invalid upload range")?
        .split('-')
        .next()
        .unwrap()
        .parse::<u64>()?;
    ensure!(
        start < size && start % (320 * 1024) == 0,
        "Invalid upload offset"
    );
    Ok(start)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_upload_urls() {
        for u in [
            "http://x.sharepoint.com/a",
            "https://sharepoint.com.evil.test/a",
            "https://user@x.sharepoint.com/a",
            "https://127.0.0.1/a",
        ] {
            assert!(validate_upload_url(u).is_err());
        }
        assert!(validate_upload_url("https://tenant.sharepoint.com/upload?token=secret").is_ok());
    }
    #[test]
    fn validates_resume_offsets() {
        assert_eq!(
            next_offset(&json!({"nextExpectedRanges":["327680-"]}), 999999).unwrap(),
            327680
        );
        assert!(next_offset(&json!({"nextExpectedRanges":["1-"]}), 999999).is_err());
        assert!(next_offset(&json!({"nextExpectedRanges":["0-","100-"]}), 999999).is_err());
    }
}
