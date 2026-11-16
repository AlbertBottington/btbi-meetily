//! Memory-only transport for immutable segments and final assembly.
//! No local retry spool. Callers retain pending segments in a bounded RAM queue.
use crate::{
    cloud_audio::{self, Segment, SegmentInfo},
    graph::{checked_json, Graph},
    queue::Destination,
};
use anyhow::{ensure, Context, Result};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const BLOCK: usize = 327_680 * 10;
fn item(d: &Destination, capture: Uuid, name: &str) -> String {
    format!(
        "drives/{}/items/{}:/{}/{}",
        d.drive_id, d.folder_id, capture, name
    )
}
impl Graph {
    pub async fn begin_cloud_capture(&self, d: &Destination, capture: Uuid) -> Result<()> {
        d.validate()?;
        self.validate_destination(d).await?;
        checked_json(self.post(&format!("drives/{}/items/{}/children",d.drive_id,d.folder_id),
            json!({"name":capture.to_string(),"folder":{},"@microsoft.graph.conflictBehavior":"fail"})).await?).await?;
        Ok(())
    }
    async fn memory_session(&self, path: &str) -> Result<String> {
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
            .to_owned();
        self.validate_transfer_url(&url)?;
        Ok(url)
    }
    async fn memory_block(&self, url: &str, bytes: &[u8], offset: u64, total: u64) -> Result<()> {
        ensure!(
            !bytes.is_empty() && offset + bytes.len() as u64 <= total,
            "Invalid upload block"
        );
        // No bearer token on the preauthenticated upload URL.
        let r = self
            .client
            .put(url)
            .header(
                "Content-Range",
                format!(
                    "bytes {}-{}/{}",
                    offset,
                    offset + bytes.len() as u64 - 1,
                    total
                ),
            )
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Cloud audio upload interrupted"))?;
        let status = r.status();
        let v = checked_json(r).await?;
        if offset + bytes.len() as u64 == total {
            ensure!(
                status.as_u16() == 200 || status.as_u16() == 201,
                "Final audio upload not committed"
            );
            ensure!(
                v["size"].as_u64() == Some(total),
                "Final audio size mismatch"
            );
        } else {
            ensure!(status.as_u16() == 202, "Unexpected upload commit");
            let range = v["nextExpectedRanges"][0]
                .as_str()
                .context("Missing upload offset")?;
            ensure!(
                range.split('-').next().and_then(|s| s.parse::<u64>().ok())
                    == Some(offset + bytes.len() as u64),
                "Upload offset mismatch"
            );
        }
        Ok(())
    }
    pub(crate) async fn put_memory_verified(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let hash = cloud_audio::digest(bytes);
        if self
            .existing_matches(path, bytes.len() as u64, &hash)
            .await?
        {
            return Ok(());
        }
        let url = self.memory_session(path).await?;
        let mut offset = 0;
        for block in bytes.chunks(BLOCK) {
            self.memory_block(&url, block, offset, bytes.len() as u64)
                .await?;
            offset += block.len() as u64;
        }
        ensure!(
            self.existing_matches(path, bytes.len() as u64, &hash)
                .await?,
            "Uploaded content missing"
        );
        Ok(())
    }
    pub async fn upload_cloud_segment(
        &self,
        d: &Destination,
        capture: Uuid,
        segment: &Segment,
    ) -> Result<()> {
        d.validate()?;
        cloud_audio::verified_pcm(&segment.info, &segment.wav)?;
        self.put_memory_verified(
            &item(
                d,
                capture,
                &format!("segment-{:08}.wav", segment.info.sequence),
            ),
            &segment.wav,
        )
        .await
    }
    pub async fn upload_cloud_track_segment(
        &self,
        d: &Destination,
        capture: Uuid,
        track: usize,
        segment: &Segment,
    ) -> Result<()> {
        d.validate()?;
        cloud_audio::verified_pcm(&segment.info, &segment.wav)?;
        self.put_memory_verified(
            &item(d, capture, &track_name(track, segment.info.sequence)?),
            &segment.wav,
        )
        .await
    }
    pub async fn upload_cloud_index(
        &self,
        d: &Destination,
        capture: Uuid,
        track: usize,
        sequence: u32,
        rows: &[cloud_audio::TimingRow],
    ) -> Result<String> {
        ensure!(track == 1 || track == 2, "Invalid source track");
        let bytes = serde_json::to_vec(rows)?;
        ensure!(bytes.len() <= 10 * 1024 * 1024, "Timing segment too large");
        self.put_memory_verified(
            &item(d, capture, &format!("source-{track}-{sequence:08}.json")),
            &bytes,
        )
        .await?;
        Ok(cloud_audio::digest(&bytes))
    }
    pub async fn publish_cloud_capture(
        &self,
        d: &Destination,
        capture: Uuid,
        manifest: &serde_json::Value,
    ) -> Result<()> {
        let bytes = serde_json::to_vec(manifest)?;
        ensure!(
            bytes.len() <= 10 * 1024 * 1024,
            "Capture manifest exceeds limit"
        );
        self.put_memory_verified(&item(d, capture, "capture.json"), &bytes)
            .await
    }
    async fn read_segment(&self, path: &str, info: &SegmentInfo) -> Result<Vec<u8>> {
        let v = checked_json(self.get(path).await?).await?;
        let expected = 44 + info.samples as usize * 2;
        ensure!(
            v["size"].as_u64() == Some(expected as u64),
            "Remote segment size mismatch"
        );
        let url = v["@microsoft.graph.downloadUrl"]
            .as_str()
            .context("Missing segment download URL")?;
        self.validate_transfer_url(url)?;
        let mut r = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Segment download failed"))?;
        ensure!(r.status().is_success(), "Segment download rejected");
        let mut bytes = Vec::with_capacity(expected);
        while let Some(chunk) = r
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Segment download interrupted"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= expected,
                "Remote segment exceeded size bound"
            );
            bytes.extend(chunk);
        }
        cloud_audio::verified_pcm(info, &bytes)?;
        Ok(bytes)
    }
    /// Run after recording stops, on a worker rather than the audio callback.
    /// Reads one remote segment at a time and uploads a single WAV. Leaves all
    /// source segments intact on any error. Cleanup is a separate verified phase.
    pub async fn assemble_cloud_audio(
        &self,
        d: &Destination,
        capture: Uuid,
        parts: &[SegmentInfo],
    ) -> Result<String> {
        self.assemble_cloud_track(d, capture, 0, parts).await
    }
    pub async fn assemble_cloud_track(
        &self,
        d: &Destination,
        capture: Uuid,
        track: usize,
        parts: &[SegmentInfo],
    ) -> Result<String> {
        d.validate()?;
        let head = cloud_audio::assembly_header(parts)?;
        let total = 44 + parts.iter().map(|p| p.samples as u64 * 2).sum::<u64>();
        let path = item(d, capture, track_output(track)?);
        // First pass verifies every source and computes the expected final hash.
        // It also makes an interrupted assembly retry idempotent after commit.
        let mut hash = Sha256::new();
        hash.update(&head);
        for p in parts {
            let b = self
                .read_segment(&item(d, capture, &track_name(track, p.sequence)?), p)
                .await?;
            hash.update(&b[44..]);
        }
        let expected = format!("{:x}", hash.finalize());
        if self.existing_matches(&path, total, &expected).await? {
            return Ok(expected);
        }
        let url = self.memory_session(&path).await?;
        let mut pending = head;
        let mut offset = 0u64;
        for p in parts {
            let bytes = self
                .read_segment(&item(d, capture, &track_name(track, p.sequence)?), p)
                .await?;
            let mut pcm = &bytes[44..];
            while !pcm.is_empty() {
                let n = (BLOCK - pending.len()).min(pcm.len());
                pending.extend(&pcm[..n]);
                pcm = &pcm[n..];
                if pending.len() == BLOCK {
                    self.memory_block(&url, &pending, offset, total).await?;
                    offset += pending.len() as u64;
                    pending.clear();
                }
            }
        }
        if !pending.is_empty() {
            self.memory_block(&url, &pending, offset, total).await?;
        }
        ensure!(
            self.existing_matches(&path, total, &expected).await?,
            "Assembled recording missing"
        );
        Ok(expected)
    }
}

fn track_output(track: usize) -> Result<&'static str> {
    match track {
        0 => Ok("recording.wav"),
        1 => Ok("microphone.wav"),
        2 => Ok("system.wav"),
        _ => anyhow::bail!("Invalid audio track"),
    }
}
fn track_name(track: usize, sequence: u32) -> Result<String> {
    let prefix = match track {
        0 => "segment",
        1 => "microphone",
        2 => "system",
        _ => anyhow::bail!("Invalid audio track"),
    };
    Ok(format!("{prefix}-{sequence:08}.wav"))
}

impl Graph {
    async fn cloud_json(&self, path: &str) -> Result<serde_json::Value> {
        let meta = checked_json(self.get(path).await?).await?;
        let size = meta["size"].as_u64().context("Missing manifest size")?;
        ensure!(size <= 10 * 1024 * 1024, "Remote manifest exceeds limit");
        let url = meta["@microsoft.graph.downloadUrl"]
            .as_str()
            .context("Missing manifest URL")?;
        self.validate_transfer_url(url)?;
        let mut r = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Manifest download failed"))?;
        ensure!(r.status().is_success(), "Manifest download rejected");
        let mut bytes = Vec::new();
        while let Some(b) = r
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("Manifest download interrupted"))?
        {
            ensure!(
                bytes.len() + b.len() <= size as usize,
                "Manifest size exceeded"
            );
            bytes.extend(b);
        }
        ensure!(bytes.len() == size as usize, "Manifest truncated");
        Ok(serde_json::from_slice(&bytes)?)
    }
    /// Single-capture worker. No filesystem staging and no unscoped discovery.
    /// A completion receipt enables retry after cleanup interruption.
    pub async fn process_cloud_capture(
        &self,
        d: &Destination,
        capture: Uuid,
    ) -> Result<serde_json::Value> {
        d.validate()?;
        let manifest = self.cloud_json(&item(d, capture, "capture.json")).await?;
        ensure!(
            manifest["version"] == 1
                && manifest["capture_id"].as_str() == Some(capture.to_string().as_str())
                && manifest["state"] == "ready_for_assembly",
            "Invalid capture manifest"
        );
        let tracks: Vec<Vec<SegmentInfo>> = serde_json::from_value(manifest["tracks"].clone())?;
        ensure!(
            tracks.len() == 3 && !tracks[0].is_empty(),
            "Incomplete capture tracks"
        );
        for parts in &tracks {
            if !parts.is_empty() {
                cloud_audio::assembly_header(parts)?;
            }
        }
        for (track, parts) in tracks.iter().enumerate().skip(1) {
            for p in parts {
                let hash = p
                    .index_sha256
                    .as_deref()
                    .context("Missing source timing checksum")?;
                let value = self
                    .cloud_json(&item(
                        d,
                        capture,
                        &format!("source-{track}-{:08}.json", p.sequence),
                    ))
                    .await?;
                ensure!(
                    cloud_audio::digest(&serde_json::to_vec(&value)?) == hash,
                    "Source timing checksum mismatch"
                );
            }
        }
        ensure!(
            manifest["transcript"]["segments"].is_array(),
            "Missing finalized transcript"
        );
        let transcript = serde_json::to_vec(&manifest["transcript"])?;
        self.put_memory_verified(&item(d, capture, "transcripts.json"), &transcript)
            .await?;
        let completion_path = item(d, capture, "complete.json");
        let existing = self.get(&completion_path).await?;
        let receipt = if existing.status() == reqwest::StatusCode::NOT_FOUND {
            let mut files = Vec::new();
            for (track, parts) in tracks.iter().enumerate() {
                if parts.is_empty() {
                    continue;
                }
                let hash = self.assemble_cloud_track(d, capture, track, parts).await?;
                files.push(json!({"name":track_output(track)?,"sha256":hash,"bytes":44+parts.iter().map(|p|p.samples as u64*2).sum::<u64>()}));
            }
            files.push(json!({"name":"transcripts.json","sha256":cloud_audio::digest(&transcript),"bytes":transcript.len()}));
            let receipt = json!({"version":1,"capture_id":capture,"manifest_sha256":cloud_audio::digest(&serde_json::to_vec(&manifest)?),"files":files});
            self.put_memory_verified(&completion_path, &serde_json::to_vec(&receipt)?)
                .await?;
            receipt
        } else {
            ensure!(existing.status().is_success(), "Completion lookup failed");
            self.cloud_json(&completion_path).await?
        };
        ensure!(
            receipt["version"] == 1
                && receipt["capture_id"].as_str() == Some(capture.to_string().as_str())
                && receipt["manifest_sha256"]
                    == cloud_audio::digest(&serde_json::to_vec(&manifest)?),
            "Completion receipt conflicts with capture"
        );
        let files = receipt["files"]
            .as_array()
            .context("Invalid completion files")?;
        let expected_count = tracks.iter().filter(|p| !p.is_empty()).count() + 1;
        ensure!(files.len() == expected_count, "Incomplete receipt");
        // Verify every final file again before deleting any segment, including on retry.
        for (i, track) in tracks.iter().enumerate().filter(|(_, p)| !p.is_empty()) {
            let name = track_output(i)?;
            let matches: Vec<_> = files.iter().filter(|f| f["name"] == name).collect();
            ensure!(matches.len() == 1, "Missing or duplicate final recording");
            let f = matches[0];
            let size = 44 + track.iter().map(|p| p.samples as u64 * 2).sum::<u64>();
            ensure!(f["bytes"].as_u64() == Some(size), "Final size mismatch");
            ensure!(
                self.existing_matches(
                    &item(d, capture, name),
                    size,
                    f["sha256"].as_str().context("Missing final checksum")?
                )
                .await?,
                "Final recording missing"
            );
        }
        ensure!(
            files
                .iter()
                .filter(|f| f["name"] == "transcripts.json"
                    && f["sha256"] == cloud_audio::digest(&transcript)
                    && f["bytes"].as_u64() == Some(transcript.len() as u64))
                .count()
                == 1,
            "Transcript receipt mismatch"
        );
        for (track, parts) in tracks.iter().enumerate() {
            for p in parts {
                let path = item(d, capture, &track_name(track, p.sequence)?);
                let r = self.get(&path).await?;
                if r.status() == reqwest::StatusCode::NOT_FOUND {
                    continue;
                }
                let meta = checked_json(r).await?;
                let etag = meta["eTag"]
                    .as_str()
                    .context("Missing segment version; cleanup refused")?;
                self.read_segment(&path, p).await?;
                let r = self
                    .client
                    .delete(format!("{}/{}", self.base, path))
                    .bearer_auth(&self.token)
                    .header("If-Match", etag)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("Segment cleanup interrupted"))?;
                ensure!(
                    r.status() == reqwest::StatusCode::NO_CONTENT
                        || r.status() == reqwest::StatusCode::NOT_FOUND,
                    "Segment changed or cleanup refused; retained for review"
                );
            }
        }
        self.put_memory_verified(&item(d,capture,"cleanup.json"),&serde_json::to_vec(&json!({"version":1,"capture_id":capture,"completion_sha256":cloud_audio::digest(&serde_json::to_vec(&receipt)?)}))?).await?;
        Ok(receipt)
    }
}

impl Graph {
    pub async fn ready_cloud_captures(&self, d: &Destination) -> Result<Vec<Uuid>> {
        d.validate()?;
        let route = format!("drives/{}/items/{}/children", d.drive_id, d.folder_id);
        let mut path = format!("{route}?$select=id,name,folder&$top=100");
        let mut captures = Vec::new();
        for _ in 0..100 {
            let page = checked_json(self.get(&path).await?).await?;
            for child in page["value"]
                .as_array()
                .context("Invalid capture listing")?
            {
                if !child["folder"].is_object() {
                    continue;
                }
                let Some(id) = child["name"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                    continue;
                };
                let r = self.get(&item(d, id, "capture.json")).await?;
                if r.status() == reqwest::StatusCode::NOT_FOUND {
                    continue;
                }
                ensure!(r.status().is_success(), "Capture lookup failed");
                let cleaned = self.get(&item(d, id, "cleanup.json")).await?;
                if cleaned.status() == reqwest::StatusCode::NOT_FOUND {
                    captures.push(id);
                } else {
                    ensure!(
                        cleaned.status().is_success(),
                        "Cleanup status lookup failed"
                    );
                }
            }
            match page["@odata.nextLink"].as_str() {
                None => return Ok(captures),
                Some(next) => {
                    let prefix = format!("{}/{}?", self.base, route);
                    let query = next
                        .strip_prefix(&prefix)
                        .context("Unexpected listing continuation route")?;
                    path = format!("{route}?{query}");
                }
            }
        }
        anyhow::bail!("Capture listing exceeds bounded scan limit")
    }
}
