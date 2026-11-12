//! Sample-bounded PCM16 WAV segments. JSON is the completion marker, published last.
use crate::NATIVE_SAMPLE_RATE_HZ;
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize)]
pub struct Boundary {
    pub kind: String,
    pub input_sample: Option<u64>,
    pub missing_samples: Option<u64>,
    pub output_sample: u64,
    pub recording_sample: u64,
}
#[derive(Serialize)]
struct Metadata {
    owner: &'static str,
    mode: &'static str,
    wav_file: String,
    sample_rate_hz: u32,
    channels: u8,
    encoding: &'static str,
    stream_id: u32,
    rf_freq_hz: f64,
    first_input_sample: u64,
    first_output_sample: u64,
    samples: u64,
    clipped_samples: u64,
    boundary: Option<Boundary>,
    end_reason: String,
}
struct Segment {
    file: File,
    json: File,
    stem: PathBuf,
    metadata: Metadata,
}
pub struct Recorder {
    directory: PathBuf,
    segment_samples: u64,
    max_recordings: usize,
    serial: u64,
    run: String,
    mode: &'static str,
    active: Option<Segment>,
    failed: bool,
}
impl Recorder {
    pub fn new(
        directory: PathBuf,
        segment_samples: u64,
        max_recordings: usize,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            segment_samples > 0 && segment_samples <= (u32::MAX as u64 - 36) / 2,
            "segment sample limit exceeds WAV size or is zero"
        );
        anyhow::ensure!(max_recordings > 0, "max recordings must be positive");
        fs::create_dir_all(&directory)?;
        let result = Self {
            directory,
            segment_samples,
            max_recordings,
            serial: 0,
            run: format!(
                "{}-{}",
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
                std::process::id()
            ),
            mode: "nbfm",
            active: None,
            failed: false,
        };
        result.prune()?;
        Ok(result)
    }
    pub(crate) fn set_mode(&mut self, mode: &'static str) {
        assert!(self.active.is_none(), "cannot change mode during a recording");
        self.mode = mode;
    }

    fn open(
        &mut self,
        stream_id: u32,
        rf: f64,
        counter: u64,
        output: u64,
        boundary: Option<Boundary>,
    ) -> anyhow::Result<()> {
        self.serial += 1;
        let stem = self
            .directory
            .join(format!("sigproc-demod-{}-{:08}", self.run, self.serial));
        // create_new is the reservation: never truncate another run, even after a restart.
        anyhow::ensure!(
            !stem.with_extension("wav").exists() && !stem.with_extension("json").exists(),
            "recording collision"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stem.with_extension("wav.part"))?;
        let json = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stem.with_extension("json.part"))?;
        file.write_all(&wav_header(0))?;
        self.active = Some(Segment {
            file,
            json,
            metadata: Metadata {
                owner: "sigproc-demod-v1",
                mode: self.mode,
                wav_file: stem
                    .with_extension("wav")
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                sample_rate_hz: NATIVE_SAMPLE_RATE_HZ,
                channels: 1,
                encoding: "PCM16LE",
                stream_id,
                rf_freq_hz: rf,
                first_input_sample: counter,
                first_output_sample: output,
                samples: 0,
                clipped_samples: 0,
                boundary,
                end_reason: String::new(),
            },
            stem,
        });
        Ok(())
    }
    pub fn write(
        &mut self,
        audio: &[f32],
        stream_id: u32,
        rf: f64,
        counter: u64,
        output: u64,
        boundary: Option<Boundary>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "recorder failed; incomplete parts preserved");
        let result = self.write_inner(audio, stream_id, rf, counter, output, boundary);
        if result.is_err() {
            // Never let Drop finalize a partial write as a completed recording.
            self.failed = true;
            self.active = None;
        }
        result
    }
    fn write_inner(
        &mut self,
        audio: &[f32],
        stream_id: u32,
        rf: f64,
        counter: u64,
        output: u64,
        mut boundary: Option<Boundary>,
    ) -> anyhow::Result<()> {
        let mut offset = 0;
        while offset < audio.len() {
            if self.active.is_none() {
                self.open(
                    stream_id,
                    rf,
                    counter + offset as u64,
                    output + offset as u64,
                    boundary.take(),
                )?;
            }
            let segment = self.active.as_mut().unwrap();
            let count = (self.segment_samples - segment.metadata.samples)
                .min((audio.len() - offset) as u64) as usize;
            let mut bytes = Vec::with_capacity(count * 2);
            for &sample in &audio[offset..offset + count] {
                anyhow::ensure!(sample.is_finite(), "non-finite audio");
                if sample.abs() > 1.0 {
                    segment.metadata.clipped_samples += 1;
                }
                bytes.extend_from_slice(
                    &((sample.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes(),
                );
            }
            segment.file.write_all(&bytes)?;
            segment.metadata.samples += count as u64;
            offset += count;
            if segment.metadata.samples == self.segment_samples {
                self.finish("sample_limit")?;
            }
        }
        Ok(())
    }
    pub fn finish(&mut self, reason: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!self.failed, "recorder failed; incomplete parts preserved");
        let result = self.finish_inner(reason);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn finish_inner(&mut self, reason: &str) -> anyhow::Result<()> {
        let Some(mut segment) = self.active.take() else {
            return Ok(());
        };
        segment.metadata.end_reason = reason.into();
        segment.file.seek(SeekFrom::Start(0))?;
        segment
            .file
            .write_all(&wav_header(segment.metadata.samples as u32 * 2))?;
        segment.file.sync_all()?;
        serde_json::to_writer_pretty(&mut segment.json, &segment.metadata)?;
        segment.json.sync_all()?;
        drop(segment.file);
        drop(segment.json);
        fs::rename(
            segment.stem.with_extension("wav.part"),
            segment.stem.with_extension("wav"),
        )?;
        fs::rename(
            segment.stem.with_extension("json.part"),
            segment.stem.with_extension("json"),
        )?;
        self.prune()
    }
    fn prune(&self) -> anyhow::Result<()> {
        let mut pairs = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_prefix("sigproc-demod-")
                .and_then(|s| s.strip_suffix(".json"))
            else {
                continue;
            };
            if id.split('-').count() != 3
                || !id.chars().all(|c| c.is_ascii_digit() || c == '-')
                || !entry.file_type()?.is_file()
            {
                continue;
            }
            let wav = path.with_extension("wav");
            if !fs::symlink_metadata(&wav)
                .map(|m| m.file_type().is_file())
                .unwrap_or(false)
            {
                continue;
            }
            // Only our completed pairs are eligible. Never follow a sidecar symlink.
            if entry.metadata()?.len() > 8192 {
                continue;
            }
            let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&fs::read(&path)?) else {
                continue;
            };
            if meta["owner"] != "sigproc-demod-v1"
                || meta["wav_file"].as_str() != wav.file_name().and_then(|s| s.to_str())
            {
                continue;
            }
            pairs.push((entry.metadata()?.modified()?, path, wav));
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let excess = pairs.len().saturating_sub(self.max_recordings);
        for (_, json, wav) in pairs.into_iter().take(excess) {
            fs::remove_file(json)?;
            fs::remove_file(wav)?;
        }
        Ok(())
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.failed {
            if let Err(error) = self.finish("drop") {
                log::error!("recording finalization failed: {error:#}");
            }
        }
    }
}
fn wav_header(bytes: u32) -> [u8; 44] {
    let mut header = [0; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(bytes + 36).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&1u16.to_le_bytes());
    header[24..28].copy_from_slice(&NATIVE_SAMPLE_RATE_HZ.to_le_bytes());
    header[28..32].copy_from_slice(&(NATIVE_SAMPLE_RATE_HZ * 2).to_le_bytes());
    header[32..34].copy_from_slice(&2u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&bytes.to_le_bytes());
    header
}
