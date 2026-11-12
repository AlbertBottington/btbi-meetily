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
    base: String,
}
impl Graph {
    pub fn new(token: String) -> Result<Self> {
        Ok(Self {
            client: client()?,
            token,
            base: "https://graph.microsoft.com/v1.0".into(),
        })
    }
    fn validate_transfer_url(&self, url: &str) -> Result<()> {
        // Only the test binary permits its own loopback mock server.
        #[cfg(test)]
        if self.base.starts_with("http://127.0.0.1:") && url.starts_with(&format!("{}/", self.base))
        {
            return Ok(());
        }
        validate_upload_url(url)
    }
    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        self.client
            .get(format!("{}/{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("SharePoint connection failed"))
    }
    async fn post(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        self.client
            .post(format!("{}/{path}", self.base))
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
        self.validate_transfer_url(download)?;
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
        let verified_folder = folder.to_path_buf();
        let verified_manifest = m.clone();
        tokio::task::spawn_blocking(move || Queue::verify(&verified_folder, &verified_manifest))
            .await??;
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
                self.validate_transfer_url(&url)?;
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
                self.validate_transfer_url(&url)?;
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

#[cfg(test)]
mod transport_tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[derive(Default)]
    struct Remote {
        offsets: HashMap<String, u64>,
        files: HashMap<String, Vec<u8>>,
        partial: HashMap<String, Vec<u8>>,
        throttled: bool,
        expire_next: bool,
        auth_leaked: bool,
        folders: usize,
    }
    async fn mock() -> (String, Arc<Mutex<Remote>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(Remote::default()));
        let shared = state.clone();
        let origin = base.clone();
        let job = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 65536];
                let header_end = loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break None;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(p) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                        break Some(p + 4);
                    }
                };
                let Some(end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < end + length {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let mut parts = headers.lines().next().unwrap().split_whitespace();
                let method = parts.next().unwrap();
                let path = parts.next().unwrap();
                let mut code = 200;
                let mut extra = String::new();
                let body;
                {
                    let mut remote = shared.lock().unwrap();
                    if path.starts_with("/transfer/") || path.starts_with("/download/") {
                        remote.auth_leaked |= headers.to_lowercase().contains("authorization:");
                    }
                    if path.ends_with("/children") {
                        remote.folders += 1;
                        code = 201;
                        body = json!({"id":"remote","folder":{}}).to_string().into_bytes();
                    } else if path.starts_with("/download/") {
                        body = remote.files[path.trim_start_matches("/download/")].clone();
                    } else if path.starts_with("/transfer/") {
                        let name = path.trim_start_matches("/transfer/").to_string();
                        if method == "GET" && remote.expire_next {
                            remote.expire_next = false;
                            remote.offsets.remove(&name);
                            remote.partial.remove(&name);
                            code = 404;
                            body = b"{}".to_vec();
                        } else if method == "GET" {
                            body=json!({"nextExpectedRanges":[format!("{}-",remote.offsets.get(&name).copied().unwrap_or(0))]}).to_string().into_bytes();
                        } else if name == "audio.dat"
                            && remote.offsets.get(&name).copied().unwrap_or(0) > 0
                            && !remote.throttled
                        {
                            remote.throttled = true;
                            code = 503;
                            extra = "Retry-After: 1\r\n".into();
                            body = b"{}".to_vec();
                        } else {
                            let range = headers
                                .lines()
                                .find(|l| l.to_lowercase().starts_with("content-range:"))
                                .unwrap();
                            let total = range.rsplit('/').next().unwrap().parse::<u64>().unwrap();
                            let offset = remote.offsets.get(&name).copied().unwrap_or(0);
                            assert!(range.contains(&format!("bytes {}-", offset)));
                            let data = remote.partial.entry(name.clone()).or_default();
                            data.extend_from_slice(&bytes[end..end + length]);
                            let next = data.len() as u64;
                            if next == total {
                                let data = remote.partial.remove(&name).unwrap();
                                remote.files.insert(name.clone(), data);
                                code = 201;
                                body = json!({"size":total}).to_string().into_bytes();
                            } else {
                                code = 202;
                                body = json!({"nextExpectedRanges":[format!("{next}-")]})
                                    .to_string()
                                    .into_bytes();
                            }
                            remote.offsets.insert(name, next);
                        }
                    } else if path.ends_with(":/createUploadSession") {
                        let name = path.split(":/").nth(1).unwrap().trim_end_matches(':');
                        body = json!({"uploadUrl":format!("{origin}/transfer/{name}")})
                            .to_string()
                            .into_bytes();
                    } else {
                        let name = path.split(":/").nth(1).unwrap();
                        if let Some(data) = remote.files.get(name) {
                            body=json!({"size":data.len(),"@microsoft.graph.downloadUrl":format!("{origin}/download/{name}")}).to_string().into_bytes();
                        } else {
                            code = 404;
                            body = b"{}".to_vec();
                        }
                    }
                }
                let response=format!("HTTP/1.1 {code} Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\n{extra}Connection: close\r\n\r\n",body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.write_all(&body).await.unwrap();
            }
        });
        (base, state, job)
    }
    async fn exercise_restart(expire: bool) {
        let (base, remote, server) = mock().await;
        let d = tempfile::tempdir().unwrap();
        let data = vec![42u8; 10 * 1024 * 1024 + 320 * 1024];
        std::fs::write(d.path().join("audio.dat"), &data).unwrap();
        let manifest = Manifest {
            version: 1,
            capture_id: uuid::Uuid::new_v4(),
            meeting_id: "synthetic".into(),
            recorder_id: "person".into(),
            recorder_name: "Synthetic".into(),
            destination: Destination {
                tenant_id: uuid::Uuid::new_v4().to_string(),
                client_id: uuid::Uuid::new_v4().to_string(),
                drive_id: "drive".into(),
                folder_id: "folder".into(),
            },
            files: vec![crate::queue::PackageFile {
                name: "audio.dat".into(),
                bytes: data.len() as u64,
                sha256: crate::queue::hash(&d.path().join("audio.dat")).unwrap(),
            }],
        };
        crate::queue::atomic_json(&d.path().join("manifest.json"), &manifest).unwrap();
        Queue::save_state(
            d.path(),
            &crate::queue::UploadState {
                status: "Pending".into(),
                ..Default::default()
            },
        )
        .unwrap();
        let graph = Graph {
            client: client().unwrap(),
            token: "synthetic-only".into(),
            base: base.clone(),
        };
        let error = graph.upload(d.path(), "person").await.unwrap_err();
        assert_eq!(error.downcast_ref::<RetryDelay>().unwrap().0, 1);
        assert_eq!(Queue::state(d.path()).unwrap().offset, 10 * 1024 * 1024);
        assert!(!remote.lock().unwrap().files.contains_key("manifest.json"));
        remote.lock().unwrap().expire_next = expire;
        // New Graph instance and on-disk state emulate process restart.
        let restarted = Graph {
            client: client().unwrap(),
            token: "synthetic-only".into(),
            base,
        };
        restarted.upload(d.path(), "person").await.unwrap();
        assert_eq!(Queue::state(d.path()).unwrap().status, "Uploaded");
        // Rewind only the local receipt: remote content must be hashed, not duplicated.
        let mut lost = Queue::state(d.path()).unwrap();
        lost.status = "Uploading".into();
        lost.file_index = 0;
        Queue::save_state(d.path(), &lost).unwrap();
        restarted.upload(d.path(), "person").await.unwrap();
        {
            let mut r = remote.lock().unwrap();
            assert_eq!(r.files["audio.dat"], data);
            assert_eq!(r.files.len(), 2);
            assert_eq!(r.folders, 1);
            assert!(!r.auth_leaked);
            r.files.get_mut("audio.dat").unwrap()[0] = 99;
        }
        Queue::save_state(d.path(), &lost).unwrap();
        assert!(restarted
            .upload(d.path(), "person")
            .await
            .unwrap_err()
            .to_string()
            .contains("Remote file conflict"));
        assert_ne!(Queue::state(d.path()).unwrap().status, "Uploaded");
        server.abort();
    }

    #[tokio::test]
    async fn resumes_after_throttle_and_reconciles_lost_commit_receipt() {
        exercise_restart(false).await;
    }
    #[tokio::test]
    async fn recreates_expired_session_without_duplicate_capture() {
        exercise_restart(true).await;
    }
}
