//! BTBI upload orchestration. Tokens live only in the OS credential store.
use anyhow::{ensure, Context, Result};
use btbi_core::{
    graph::{self, Profile},
    queue::{self, Destination, Queue},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Manager, Runtime};
use tokio::sync::Mutex;

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static OPERATION: Mutex<()> = Mutex::const_new(());
static SIGNING_IN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static NOTICE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub destination: Destination,
    pub profile: Option<Profile>,
}
fn root() -> Result<&'static PathBuf> {
    ROOT.get().context("Upload storage not initialized")
}
fn settings() -> Result<Settings> {
    Ok(serde_json::from_reader(std::fs::File::open(
        root()?.join("settings.json"),
    )?)?)
}
fn save(s: &Settings) -> Result<()> {
    queue::atomic_json(&root()?.join("settings.json"), s)
}
fn credentials(s: &Settings) -> Result<keyring::Entry> {
    let p = s.profile.as_ref().context("Sign in to Microsoft")?;
    Ok(keyring::Entry::new(
        "BTBI Meetily",
        &format!(
            "{}:{}:{}",
            s.destination.tenant_id, s.destination.client_id, p.id
        ),
    )?)
}
fn notice(s: &str) {
    if let Ok(mut n) = NOTICE.lock() {
        *n = s.to_string();
    }
}
fn message(e: anyhow::Error) -> String {
    e.to_string()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn initialize<R: Runtime>(app: &AppHandle<R>) -> Result<()> {
    let path = app.path().app_data_dir()?.join("btbi");
    std::fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    }
    ROOT.set(path)
        .map_err(|_| anyhow::anyhow!("Uploader already initialized"))?;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            // Recover opted-in, finalized meetings saved before a crash or failed snapshot.
            if let Some(state) = app.try_state::<crate::state::AppState>() {
                let rows: Result<Vec<(String, String)>, _> = sqlx::query_as(
                    "SELECT id, folder_path FROM meetings WHERE folder_path IS NOT NULL",
                )
                .fetch_all(state.db_manager.pool())
                .await;
                if let Ok(rows) = rows {
                    for (id, path) in rows {
                        let folder = Path::new(&path);
                        if folder.join("capture-ready.json").is_file()
                            && folder.join("recorder.json").is_file()
                        {
                            if let Err(e) = enqueue(folder, &id, Value::Null).await {
                                notice(&format!("Saved meeting needs upload attention: {}", e));
                            }
                        }
                    }
                }
            }
            if let Err(e) = run_queue().await {
                notice(&e.to_string());
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    Ok(())
}
pub fn stamp_recorder(folder: Option<PathBuf>) -> Result<()> {
    let Ok(s) = settings() else {
        return Ok(());
    };
    if s.profile.is_none() {
        return Ok(());
    }
    let profile = s
        .profile
        .context("Sign in before recording for automatic upload")?;
    let folder = folder.context("Enable Save recordings for SharePoint audio upload")?;
    queue::atomic_json(
        &folder.join("recorder.json"),
        &json!({"profile":profile,"destination":s.destination,"captured_at":now()}),
    )
}
pub async fn enqueue(folder: &Path, meeting_id: &str, _transcript: Value) -> Result<()> {
    let _guard = OPERATION.lock().await;
    if !folder.join("recorder.json").exists() {
        return Ok(());
    }
    let folder = folder.to_path_buf();
    let meeting_id = meeting_id.to_string();
    let queue_root = root()?.join("queue");
    tokio::task::spawn_blocking(move || -> Result<()> {
        let v: Value = serde_json::from_reader(std::fs::File::open(folder.join("recorder.json"))?)?;
        let p: Profile = serde_json::from_value(v["profile"].clone())?;
        let dest: Destination = serde_json::from_value(v["destination"].clone())?;
        let transcript: Value = serde_json::from_reader(std::fs::File::open(
            folder.join("canonical-transcript.json"),
        )?)?;
        Queue::new(queue_root)?.enqueue(&folder, &meeting_id, &p.id, &p.name, dest, &transcript)?;
        Ok(())
    })
    .await??;
    Ok(())
}
async fn access(s: &Settings) -> Result<String> {
    let entry = credentials(s)?;
    let refresh = entry
        .get_password()
        .map_err(|_| anyhow::anyhow!("Microsoft sign-in required"))?;
    let token = graph::refresh(&s.destination, &refresh).await?;
    if let Some(r) = token.refresh_token {
        entry
            .set_password(&r)
            .map_err(|_| anyhow::anyhow!("Could not protect Microsoft token"))?;
    }
    let profile = graph::profile(&token.access_token).await?;
    ensure!(
        Some(&profile.id) == s.profile.as_ref().map(|p| &p.id),
        "Microsoft account changed; sign in again"
    );
    Ok(token.access_token)
}
async fn run_queue() -> Result<()> {
    let Ok(_guard) = OPERATION.try_lock() else {
        return Ok(());
    };
    let Ok(s) = settings() else {
        return Ok(());
    };
    if !s.enabled {
        return Ok(());
    }
    let q = Queue::new(root()?.join("queue"))?;
    let mut graph = None;
    for folder in q.entries()? {
        let mut state = Queue::state(&folder)?;
        if state.status == "Uploaded" {
            continue;
        }
        let retry: Value = std::fs::read(folder.join("retry.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({}));
        if retry["after"].as_u64().unwrap_or(0) > now() {
            continue;
        }
        let manifest = Queue::manifest(&folder)?;
        if manifest.destination != s.destination
            || Some(&manifest.recorder_id) != s.profile.as_ref().map(|p| &p.id)
        {
            state.status = "Needs attention".into();
            state.error = Some("Original recorder and destination required".into());
            Queue::save_state(&folder, &state)?;
            continue;
        }
        if graph.is_none() {
            graph = Some(graph::Graph::new(access(&s).await?)?);
        }
        if let Err(e) = graph
            .as_ref()
            .unwrap()
            .upload(&folder, &manifest.recorder_id)
            .await
        {
            state = Queue::state(&folder)?;
            state.status = "Needs attention".into();
            state.error = Some(e.to_string());
            Queue::save_state(&folder, &state)?;
            let delay = e
                .downcast_ref::<graph::RetryDelay>()
                .map(|d| d.0)
                .unwrap_or_else(|| {
                    30u64
                        .saturating_mul(2u64.pow(state.attempts.min(7)))
                        .min(3600)
                });
            queue::atomic_json(&folder.join("retry.json"), &json!({"after":now()+delay}))?;
        }
    }
    Ok(())
}
#[tauri::command]
pub async fn btbi_status() -> Result<Value, String> {
    let q = Queue::new(root().map_err(message)?.join("queue")).map_err(message)?;
    let mut entries = Vec::new();
    for folder in q.entries().map_err(message)? {
        let m = Queue::manifest(&folder).map_err(message)?;
        let s = Queue::state(&folder).map_err(message)?;
        entries.push(json!({"capture_id":m.capture_id,"meeting_id":m.meeting_id,"status":s.status,"error":s.error}));
    }
    Ok(
        json!({"settings":settings().ok(),"entries":entries,"signing_in":SIGNING_IN.load(std::sync::atomic::Ordering::SeqCst),"notice":NOTICE.lock().map(|s|s.clone()).unwrap_or_default()}),
    )
}
#[tauri::command]
pub async fn btbi_configure(destination: Destination, enabled: bool) -> Result<(), String> {
    let _guard = OPERATION.lock().await;
    if crate::audio::recording_commands::is_recording().await {
        return Err("Stop recording before changing upload settings".into());
    }
    destination.validate().map_err(message)?;
    let old = settings().ok();
    let profile = old
        .filter(|s| s.destination == destination)
        .and_then(|s| s.profile);
    if enabled && profile.is_none() {
        return Err("Save settings and sign in before enabling uploads".into());
    }
    save(&Settings {
        enabled,
        destination,
        profile,
    })
    .map_err(message)
}
#[tauri::command]
pub async fn btbi_begin_signin() -> Result<Value, String> {
    if crate::audio::recording_commands::is_recording().await {
        return Err("Stop recording before changing the Microsoft account".into());
    }
    use std::sync::atomic::Ordering;
    if SIGNING_IN.swap(true, Ordering::SeqCst) {
        return Err("Sign-in already in progress".into());
    }
    let s = match settings() {
        Ok(s) => s,
        Err(_) => {
            SIGNING_IN.store(false, Ordering::SeqCst);
            return Err("Configure the Microsoft app and SharePoint folder first".into());
        }
    };
    let code = match graph::begin(&s.destination).await {
        Ok(c) => c,
        Err(e) => {
            SIGNING_IN.store(false, Ordering::SeqCst);
            return Err(message(e));
        }
    };
    let result = json!({"user_code":code.user_code,"verification_uri":code.verification_uri,"expires_in":code.expires_in});
    tauri::async_runtime::spawn(async move {
        let result: Result<()> = async {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(code.expires_in.min(1800));
            let mut interval = code.interval.unwrap_or(5).max(5);
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                match graph::poll(&s.destination, &code.device_code).await {
                    Ok(Some(token)) => {
                        let _guard = OPERATION.lock().await;
                        ensure!(
                            settings()?.destination == s.destination,
                            "Settings changed; sign in again"
                        );
                        let profile = graph::profile(&token.access_token).await?;
                        graph::Graph::new(token.access_token)?
                            .validate_destination(&s.destination)
                            .await?;
                        let mut saved = settings()?;
                        saved.profile = Some(profile);
                        saved.enabled = true;
                        credentials(&saved)?.set_password(
                            token
                                .refresh_token
                                .as_deref()
                                .context("Microsoft did not return offline access")?,
                        )?;
                        save(&saved)?;
                        notice("Signed in. Automatic upload enabled.");
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(e) if e.to_string() == "slow_down" => interval += 5,
                    Err(e) => return Err(e),
                }
            }
            anyhow::bail!("Sign-in expired; try again")
        }
        .await;
        if let Err(e) = result {
            notice(&e.to_string());
        }
        SIGNING_IN.store(false, Ordering::SeqCst);
    });
    Ok(result)
}
#[tauri::command]
pub async fn btbi_retry() -> Result<(), String> {
    let _guard = OPERATION.lock().await;
    let q = Queue::new(root().map_err(message)?.join("queue")).map_err(message)?;
    for folder in q.entries().map_err(message)? {
        queue::atomic_json(&folder.join("retry.json"), &json!({"after":0})).map_err(message)?;
    }
    Ok(())
}
