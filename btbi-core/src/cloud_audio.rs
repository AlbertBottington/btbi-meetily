//! Disk-free PCM segmentation and assembly. The caller owns transport and retry.
//! No filesystem APIs: audio is bounded in memory and failed transfers must never
//! be represented as complete. Segments are immutable WAV files, not MP4 fragments.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const RATE: u32 = 48_000;
pub const SEGMENT_SAMPLES: usize = RATE as usize * 30;
pub const MAX_SAMPLES: u64 = (u32::MAX as u64 - 36) / 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SegmentInfo {
    pub sequence: u32,
    pub samples: u32,
    pub sha256: String,
}
pub struct Segment {
    pub info: SegmentInfo,
    pub wav: Vec<u8>,
}
/// Header for mono PCM16. Maximum length is checked before any upload begins.
pub fn header(samples: u64) -> Result<Vec<u8>> {
    ensure!(samples <= MAX_SAMPLES, "Recording exceeds WAV size limit");
    let bytes = (samples * 2) as u32;
    let mut h = Vec::with_capacity(44);
    h.extend(b"RIFF");
    h.extend((36 + bytes).to_le_bytes());
    h.extend(b"WAVEfmt ");
    h.extend(16u32.to_le_bytes());
    h.extend(1u16.to_le_bytes());
    h.extend(1u16.to_le_bytes());
    h.extend(RATE.to_le_bytes());
    h.extend((RATE * 2).to_le_bytes());
    h.extend(2u16.to_le_bytes());
    h.extend(16u16.to_le_bytes());
    h.extend(b"data");
    h.extend(bytes.to_le_bytes());
    Ok(h)
}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub struct Segmenter {
    pcm: Vec<u8>,
    sequence: u32,
    total: u64,
}
impl Default for Segmenter {
    fn default() -> Self {
        Self {
            pcm: Vec::with_capacity(SEGMENT_SAMPLES * 2),
            sequence: 0,
            total: 0,
        }
    }
}
impl Segmenter {
    /// Emit synchronously into a bounded, nonblocking transport queue. On any
    /// error the owner must fail the capture; retrying this input duplicates it.
    pub fn push(
        &mut self,
        samples: &[f32],
        rate: u32,
        mut emit: impl FnMut(Segment) -> Result<()>,
    ) -> Result<()> {
        ensure!(rate == RATE, "Unsupported recording sample rate");
        ensure!(
            samples.iter().all(|s| s.is_finite()),
            "Invalid audio samples"
        );
        ensure!(
            self.total + samples.len() as u64 <= MAX_SAMPLES,
            "Recording exceeds WAV size limit"
        );
        for sample in samples {
            self.pcm
                .extend(((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
            self.total += 1;
            if self.pcm.len() == SEGMENT_SAMPLES * 2 {
                emit(self.take()?)?;
            }
        }
        Ok(())
    }
    fn take(&mut self) -> Result<Segment> {
        let samples = self.pcm.len() as u32 / 2;
        let mut wav = header(samples as u64)?;
        wav.append(&mut self.pcm);
        let info = SegmentInfo {
            sequence: self.sequence,
            samples,
            sha256: digest(&wav),
        };
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Too many segments"))?;
        Ok(Segment { info, wav })
    }
    pub fn finish(&mut self) -> Result<Option<Segment>> {
        if self.pcm.is_empty() {
            Ok(None)
        } else {
            self.take().map(Some)
        }
    }
}
/// Validate the ordered, bounded input set before starting a final upload.
pub fn assembly_header(parts: &[SegmentInfo]) -> Result<Vec<u8>> {
    ensure!(!parts.is_empty(), "No audio segments");
    let mut total = 0u64;
    for (i, p) in parts.iter().enumerate() {
        ensure!(
            p.sequence as usize == i,
            "Missing or reordered audio segment"
        );
        ensure!(
            p.samples > 0 && p.samples as usize <= SEGMENT_SAMPLES,
            "Invalid segment size"
        );
        ensure!(
            p.sha256.len() == 64 && p.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid segment checksum"
        );
        total += p.samples as u64;
    }
    header(total)
}
/// Check each downloaded segment before forwarding its PCM to the final upload.
pub fn verified_pcm<'a>(info: &SegmentInfo, wav: &'a [u8]) -> Result<&'a [u8]> {
    ensure!(
        info.samples > 0 && info.samples as usize <= SEGMENT_SAMPLES,
        "Invalid segment size"
    );
    ensure!(
        wav.len() == 44 + info.samples as usize * 2,
        "Segment size mismatch"
    );
    ensure!(
        &wav[..44] == header(info.samples as u64)?.as_slice(),
        "Segment format mismatch"
    );
    ensure!(digest(wav) == info.sha256, "Segment checksum mismatch");
    Ok(&wav[44..])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_and_final_words_survive_assembly() {
        let data: Vec<f32> = (0..SEGMENT_SAMPLES + 17)
            .map(|n| (n % 101) as f32 / 101.0)
            .collect();
        let mut s = Segmenter::default();
        let mut parts = Vec::new();
        for chunk in data.chunks(511) {
            s.push(chunk, RATE, |p| {
                parts.push(p);
                Ok(())
            })
            .unwrap();
        }
        parts.push(s.finish().unwrap().unwrap());
        assert!(s.finish().unwrap().is_none());
        assert_eq!(parts.len(), 2);
        let info: Vec<_> = parts.iter().map(|p| p.info.clone()).collect();
        let mut joined = assembly_header(&info).unwrap();
        for p in &parts {
            joined.extend(verified_pcm(&p.info, &p.wav).unwrap());
        }
        let mut expected = header(data.len() as u64).unwrap();
        for x in data {
            expected.extend(((x * 32767.0).round() as i16).to_le_bytes());
        }
        assert_eq!(joined, expected);
    }
    #[test]
    fn reject_missing_reordered_corrupt_and_oversized_audio() {
        let mut s = Segmenter::default();
        s.push(&[0.2, 0.3], RATE, |_| Ok(())).unwrap();
        let mut p = s.finish().unwrap().unwrap();
        let mut info = p.info.clone();
        info.sequence = 1;
        assert!(assembly_header(&[info]).is_err());
        p.wav[44] ^= 1;
        assert!(verified_pcm(&p.info, &p.wav).is_err());
        assert!(header(MAX_SAMPLES + 1).is_err());
        assert!(s.push(&[f32::NAN], RATE, |_| Ok(())).is_err());
        assert!(s.push(&[0.0], 44100, |_| Ok(())).is_err());
    }
    #[test]
    fn full_transport_queue_is_an_error_not_silent_loss() {
        let mut s = Segmenter::default();
        assert!(s
            .push(&vec![0.0; SEGMENT_SAMPLES], RATE, |_| anyhow::bail!(
                "Upload backlog full"
            ))
            .is_err());
    }
}
