//! Independent source tracks. Index timestamps preserve gaps and pauses; tracks
//! are not claimed to identify a person or to share an aligned sample clock.
use anyhow::{ensure, Result};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Seek, SeekFrom, Write},
    path::Path,
};

pub struct Tracks {
    mic: Wav,
    system: Wav,
    index: BufWriter<File>,
}
struct Wav {
    file: BufWriter<File>,
    samples: u32,
}
impl Wav {
    fn new(path: &Path) -> Result<Self> {
        let mut file = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(path)?);
        file.write_all(&[0; 44])?;
        Ok(Self { file, samples: 0 })
    }
    fn append(&mut self, data: &[f32]) -> Result<u32> {
        let offset = self.samples;
        ensure!(
            data.len() as u64 + self.samples as u64 <= (u32::MAX as u64 - 36) / 2,
            "Audio track exceeds WAV limit"
        );
        for sample in data {
            ensure!(sample.is_finite(), "Invalid audio sample");
            self.file
                .write_all(&((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())?;
        }
        self.samples += data.len() as u32;
        Ok(offset)
    }
    fn finish(mut self) -> Result<u32> {
        let bytes = self.samples * 2;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(b"RIFF")?;
        self.file.write_all(&(36 + bytes).to_le_bytes())?;
        self.file.write_all(b"WAVEfmt ")?;
        self.file.write_all(&16u32.to_le_bytes())?;
        self.file.write_all(&1u16.to_le_bytes())?;
        self.file.write_all(&1u16.to_le_bytes())?;
        self.file.write_all(&48000u32.to_le_bytes())?;
        self.file.write_all(&96000u32.to_le_bytes())?;
        self.file.write_all(&2u16.to_le_bytes())?;
        self.file.write_all(&16u16.to_le_bytes())?;
        self.file.write_all(b"data")?;
        self.file.write_all(&bytes.to_le_bytes())?;
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        Ok(self.samples)
    }
}
impl Tracks {
    pub fn new(folder: &Path) -> Result<Self> {
        Ok(Self {
            mic: Wav::new(&folder.join("microphone.wav"))?,
            system: Wav::new(&folder.join("system.wav"))?,
            index: BufWriter::new(
                OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(folder.join("source-index.jsonl"))?,
            ),
        })
    }
    pub fn append(
        &mut self,
        microphone: bool,
        timestamp: f64,
        rate: u32,
        samples: &[f32],
    ) -> Result<()> {
        ensure!(
            rate == 48000 && timestamp.is_finite() && timestamp >= 0.0,
            "Invalid source audio clock/rate"
        );
        let offset = if microphone {
            self.mic.append(samples)?
        } else {
            self.system.append(samples)?
        };
        serde_json::to_writer(
            &mut self.index,
            &json!({"source":if microphone {"microphone"} else {"system"},"timestamp":timestamp,"sample_offset":offset,"samples":samples.len(),"sample_rate":rate}),
        )?;
        self.index.write_all(b"\n")?;
        Ok(())
    }
    pub fn finish(mut self, folder: &Path) -> Result<()> {
        let mic = self.mic.finish()?;
        let system = self.system.finish()?;
        self.index.flush()?;
        self.index.get_ref().sync_all()?;
        ensure!(mic > 0 || system > 0, "No source audio captured");
        crate::queue::atomic_json(
            &folder.join("sources-complete.json"),
            &json!({"microphone_samples":mic,"system_samples":system,"sample_rate":48000}),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_sources_and_gaps() {
        let d = tempfile::tempdir().unwrap();
        let mut t = Tracks::new(d.path()).unwrap();
        t.append(true, 0.0, 48000, &[0.5, -0.5]).unwrap();
        t.append(false, 0.1, 48000, &[0.25]).unwrap();
        t.append(true, 9.0, 48000, &[0.0]).unwrap();
        t.finish(d.path()).unwrap();
        let m = std::fs::read(d.path().join("microphone.wav")).unwrap();
        let s = std::fs::read(d.path().join("system.wav")).unwrap();
        assert_eq!(&m[..4], b"RIFF");
        assert_eq!(m.len(), 50);
        assert_eq!(s.len(), 46);
        let index = std::fs::read_to_string(d.path().join("source-index.jsonl")).unwrap();
        let rows: Vec<serde_json::Value> = index
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows[2]["timestamp"], 9.0);
        assert_eq!(rows[2]["sample_offset"], 2);
    }
    #[test]
    fn incomplete_never_marked_ready() {
        let d = tempfile::tempdir().unwrap();
        let mut t = Tracks::new(d.path()).unwrap();
        assert!(t.append(true, 0.0, 44100, &[0.0]).is_err());
        drop(t);
        assert!(!d.path().join("sources-complete.json").exists());
    }
}
