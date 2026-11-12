use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Missing parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn hash(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut b = [0u8; 65536];
    loop {
        let n = f.read(&mut b)?;
        if n == 0 {
            break;
        }
        h.update(&b[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destination {
    pub tenant_id: String,
    pub client_id: String,
    pub drive_id: String,
    pub folder_id: String,
}
impl Destination {
    pub fn validate(&self) -> Result<()> {
        Uuid::parse_str(&self.tenant_id)?;
        Uuid::parse_str(&self.client_id)?;
        for id in [&self.drive_id, &self.folder_id] {
            ensure!(
                !id.is_empty()
                    && id.len() < 300
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.!".contains(c)),
                "Invalid SharePoint item identifier"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PackageFile {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub capture_id: Uuid,
    pub meeting_id: String,
    pub recorder_id: String,
    pub recorder_name: String,
    pub destination: Destination,
    pub files: Vec<PackageFile>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UploadState {
    pub status: String,
    pub error: Option<String>,
    pub remote_folder: Option<String>,
    pub file_index: usize,
    pub upload_url: Option<String>,
    pub offset: u64,
    pub attempts: u32,
}
pub struct Queue {
    pub root: PathBuf,
}
impl Queue {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    pub fn entries(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.root)? {
            let p = e?.path();
            if p.join("manifest.json").is_file() {
                out.push(p);
            }
        }
        out.sort();
        Ok(out)
    }
    pub fn enqueue(
        &self,
        source: &Path,
        meeting_id: &str,
        recorder_id: &str,
        recorder_name: &str,
        destination: Destination,
        transcript: &serde_json::Value,
    ) -> Result<Uuid> {
        destination.validate()?;
        ensure!(!recorder_id.is_empty(), "Sign in before uploading");
        let metadata: serde_json::Value =
            serde_json::from_reader(File::open(source.join("metadata.json"))?)?;
        ensure!(metadata["status"] == "completed", "Recording is incomplete");
        ensure!(
            source.join("capture-ready.json").is_file(),
            "Recording/transcription finalization not verified"
        );
        ensure!(
            source.join("sources-complete.json").is_file(),
            "Source tracks are incomplete"
        );
        let id = metadata["capture_id"]
            .as_str()
            .context("Missing capture identity")?
            .parse::<Uuid>()?;
        let folder = self.root.join(id.to_string());
        if folder.join("manifest.json").exists() {
            let old = Self::manifest(&folder)?;
            ensure!(
                old.meeting_id == meeting_id
                    && old.recorder_id == recorder_id
                    && old.destination == destination,
                "Capture already queued for another identity/destination"
            );
            return Ok(id);
        }
        // A staging directory is never processed. A crash leaves originals intact.
        let staging = tempfile::Builder::new()
            .prefix(".staging-")
            .tempdir_in(&self.root)?;
        let mut files = Vec::new();
        for name in [
            "audio.mp4",
            "microphone.wav",
            "system.wav",
            "source-index.jsonl",
            "sources-complete.json",
            "metadata.json",
            "capture-ready.json",
            "recorder.json",
        ] {
            let src = source.join(name);
            ensure!(src.is_file(), "Required capture file missing: {name}");
            let dst = staging.path().join(name);
            fs::copy(&src, &dst)?;
            File::open(&dst)?.sync_all()?;
            files.push(PackageFile {
                name: name.into(),
                bytes: dst.metadata()?.len(),
                sha256: hash(&dst)?,
            });
        }
        let dst = staging.path().join("transcript.json");
        atomic_json(&dst, transcript)?;
        files.push(PackageFile {
            name: "transcript.json".into(),
            bytes: dst.metadata()?.len(),
            sha256: hash(&dst)?,
        });
        let manifest = Manifest {
            version: 1,
            capture_id: id,
            meeting_id: meeting_id.into(),
            recorder_id: recorder_id.into(),
            recorder_name: recorder_name.into(),
            destination,
            files,
        };
        atomic_json(&staging.path().join("manifest.json"), &manifest)?;
        atomic_json(
            &staging.path().join("state.json"),
            &UploadState {
                status: "Pending".into(),
                ..Default::default()
            },
        )?;
        ensure!(!folder.exists(), "Queue directory needs review");
        fs::rename(staging.path(), &folder)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(id)
    }
    pub fn manifest(folder: &Path) -> Result<Manifest> {
        Ok(serde_json::from_reader(File::open(
            folder.join("manifest.json"),
        )?)?)
    }
    pub fn state(folder: &Path) -> Result<UploadState> {
        Ok(serde_json::from_reader(File::open(
            folder.join("state.json"),
        )?)?)
    }
    pub fn save_state(folder: &Path, s: &UploadState) -> Result<()> {
        atomic_json(&folder.join("state.json"), s)
    }
    pub fn verify(folder: &Path, m: &Manifest) -> Result<()> {
        for f in &m.files {
            ensure!(
                Path::new(&f.name).components().count() == 1 && !f.name.starts_with('.'),
                "Unsafe package filename"
            );
            let p = folder.join(&f.name);
            ensure!(
                p.metadata()?.len() == f.bytes && hash(&p)? == f.sha256,
                "Queued file changed: {}",
                f.name
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn atomic_state_survives_replacement() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("s.json");
        atomic_json(&p, &1).unwrap();
        atomic_json(&p, &2).unwrap();
        assert_eq!(
            serde_json::from_reader::<_, u32>(File::open(p).unwrap()).unwrap(),
            2
        );
    }
    #[test]
    fn validates_destination() {
        let mut d = Destination {
            tenant_id: Uuid::new_v4().to_string(),
            client_id: Uuid::new_v4().to_string(),
            drive_id: "b!abc".into(),
            folder_id: "abc".into(),
        };
        assert!(d.validate().is_ok());
        d.folder_id = "../bad".into();
        assert!(d.validate().is_err());
    }
    #[test]
    fn detects_corruption() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("audio"), b"ok").unwrap();
        let m = Manifest {
            version: 1,
            capture_id: Uuid::new_v4(),
            meeting_id: "m".into(),
            recorder_id: "r".into(),
            recorder_name: "r".into(),
            destination: Destination {
                tenant_id: "".into(),
                client_id: "".into(),
                drive_id: "".into(),
                folder_id: "".into(),
            },
            files: vec![PackageFile {
                name: "audio".into(),
                bytes: 2,
                sha256: hash(&d.path().join("audio")).unwrap(),
            }],
        };
        Queue::verify(d.path(), &m).unwrap();
        fs::write(d.path().join("audio"), b"no").unwrap();
        assert!(Queue::verify(d.path(), &m).is_err());
    }
    fn fixture() -> (tempfile::TempDir, Destination) {
        let d = tempfile::tempdir().unwrap();
        let id = Uuid::new_v4();
        atomic_json(
            &d.path().join("metadata.json"),
            &serde_json::json!({"status":"completed","capture_id":id}),
        )
        .unwrap();
        for n in [
            "audio.mp4",
            "microphone.wav",
            "system.wav",
            "source-index.jsonl",
            "sources-complete.json",
            "capture-ready.json",
            "recorder.json",
        ] {
            fs::write(d.path().join(n), b"fixture").unwrap();
        }
        (
            d,
            Destination {
                tenant_id: Uuid::new_v4().to_string(),
                client_id: Uuid::new_v4().to_string(),
                drive_id: "drive".into(),
                folder_id: "folder".into(),
            },
        )
    }
    #[test]
    fn enqueue_is_idempotent_and_snapshot_is_immutable() {
        let (src, dest) = fixture();
        let root = tempfile::tempdir().unwrap();
        let q = Queue::new(root.path().into()).unwrap();
        let id = q
            .enqueue(
                src.path(),
                "meeting",
                "person",
                "Matt",
                dest.clone(),
                &serde_json::json!({"segments":[]}),
            )
            .unwrap();
        fs::write(src.path().join("audio.mp4"), b"changed original").unwrap();
        assert_eq!(
            q.enqueue(
                src.path(),
                "meeting",
                "person",
                "Matt",
                dest.clone(),
                &serde_json::json!({"segments":[]})
            )
            .unwrap(),
            id
        );
        assert_eq!(q.entries().unwrap().len(), 1);
        let path = q.entries().unwrap().remove(0);
        Queue::verify(&path, &Queue::manifest(&path).unwrap()).unwrap();
        assert!(q
            .enqueue(
                src.path(),
                "meeting",
                "other",
                "Patrick",
                dest,
                &serde_json::json!({})
            )
            .is_err());
    }
    #[test]
    fn missing_completion_is_not_queued() {
        let (src, dest) = fixture();
        fs::remove_file(src.path().join("capture-ready.json")).unwrap();
        let root = tempfile::tempdir().unwrap();
        let q = Queue::new(root.path().into()).unwrap();
        assert!(q
            .enqueue(src.path(), "m", "p", "Matt", dest, &serde_json::json!({}))
            .is_err());
        assert!(q.entries().unwrap().is_empty());
    }
    #[test]
    fn restart_reads_saved_session_without_exposing_it_in_manifest() {
        let d = tempfile::tempdir().unwrap();
        let state = UploadState {
            status: "Uploading".into(),
            upload_url: Some("https://tenant.sharepoint.com/session?secret".into()),
            offset: 327680,
            ..Default::default()
        };
        Queue::save_state(d.path(), &state).unwrap();
        assert_eq!(Queue::state(d.path()).unwrap().offset, 327680);
    }
}
