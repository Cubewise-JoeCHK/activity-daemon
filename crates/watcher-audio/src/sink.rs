//! Audio sinks: where captured PCM bytes go.
//!
//! Capture is decoupled from storage through the [`AudioSink`] trait so the
//! engine is testable without an audio device: unit tests use [`FakeSink`],
//! the Windows capture engine writes [`WavSink`] files. `WavSink` itself is
//! cross-platform and tested everywhere.
//!
//! Files are written to `<final>.wav.part` first and renamed to the final
//! name on finalize, so a crash never leaves half-named recordings behind
//! (atomic-ish on the same volume).

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Recording destination for one stream.
pub trait AudioSink: Send {
    /// Append raw PCM bytes (already 16-bit, interleaved, in sink's format).
    fn write(&mut self, chunk: &[u8]) -> io::Result<()>;

    /// PCM bytes written so far (excludes the WAV header).
    fn bytes_written(&self) -> u64;

    /// Patch the WAV header, flush, rename `.part` to the final name and
    /// return the total byte count (PCM data only, header excluded).
    fn finalize(&mut self) -> io::Result<u64>;
}

/// Max PCM payload for one WAV file (just under the 4 GiB RIFF limit).
pub const MAX_WAV_DATA_BYTES: u64 = 0xFFFF_0000;

/// Build a 44-byte canonical PCM WAV header. `data_len` is patched later at
/// offsets 4 (RIFF size) and 40 (data size).
pub fn wav_header(sample_rate: u32, channels: u16, bits: u16, data_len: u32) -> [u8; 44] {
    let byte_rate = sample_rate * channels as u32 * bits as u32 / 8;
    let block_align = channels * bits / 8;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM format
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    h[32..34].copy_from_slice(&block_align.to_le_bytes());
    h[34..36].copy_from_slice(&bits.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// WAV sink writing 16-bit PCM to `<final>.wav.part`, renamed to `<final>.wav`
/// on finalize.
pub struct WavSink {
    writer: Option<BufWriter<File>>,
    part_path: PathBuf,
    final_path: PathBuf,
    pcm_bytes: u64,
}

impl WavSink {
    /// Create the sink, making parent directories. The `.part` file is
    /// created immediately with a placeholder header (patched on finalize).
    pub fn create(
        final_path: PathBuf,
        sample_rate: u32,
        channels: u16,
        bits: u16,
    ) -> io::Result<Self> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let part_path = crate::naming::part_path(&final_path);
        let mut file = File::create(&part_path)?;
        file.write_all(&wav_header(sample_rate, channels, bits, 0))?;
        Ok(WavSink {
            writer: Some(BufWriter::new(file)),
            part_path,
            final_path,
            pcm_bytes: 0,
        })
    }

    /// The path the file will have after finalize.
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    fn patch_header(&mut self, file: &mut File) -> io::Result<()> {
        let data_len = u32::try_from(self.pcm_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "wav too large"))?;
        file.seek(SeekFrom::Start(4))?;
        file.write_all(&(36 + data_len).to_le_bytes())?;
        file.seek(SeekFrom::Start(40))?;
        file.write_all(&data_len.to_le_bytes())?;
        file.sync_all()
    }
}

impl AudioSink for WavSink {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let remaining = MAX_WAV_DATA_BYTES.saturating_sub(self.pcm_bytes);
        let take = (chunk.len() as u64).min(remaining) as usize;
        if take == 0 {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "recording exceeded the 4 GiB WAV limit; stream truncated",
            ));
        }
        if let Some(w) = self.writer.as_mut() {
            w.write_all(&chunk[..take])?;
        }
        self.pcm_bytes += take as u64;
        if take < chunk.len() {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "recording exceeded the 4 GiB WAV limit; stream truncated",
            ));
        }
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.pcm_bytes
    }

    fn finalize(&mut self) -> io::Result<u64> {
        if let Some(mut w) = self.writer.take() {
            w.flush()?;
            let mut file = w
                .into_inner()
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.patch_header(&mut file)?;
            if !self.final_path.exists() {
                std::fs::rename(&self.part_path, &self.final_path)?;
            }
        }
        Ok(self.pcm_bytes)
    }
}

/// In-memory fake sink for tests: records how much was written and can be
/// made to fail after N bytes to exercise error paths.
pub struct FakeSink {
    pcm_bytes: u64,
    fail_after_bytes: Option<u64>,
    pub finalized: bool,
    pub write_errors: usize,
}

impl FakeSink {
    pub fn new() -> Self {
        FakeSink {
            pcm_bytes: 0,
            fail_after_bytes: None,
            finalized: false,
            write_errors: 0,
        }
    }

    /// The sink starts failing writes once more than `limit` bytes are
    /// accumulated.
    pub fn failing_after(limit: u64) -> Self {
        FakeSink {
            pcm_bytes: 0,
            fail_after_bytes: Some(limit),
            finalized: false,
            write_errors: 0,
        }
    }
}

impl Default for FakeSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSink for FakeSink {
    fn write(&mut self, chunk: &[u8]) -> io::Result<()> {
        if let Some(limit) = self.fail_after_bytes {
            if self.pcm_bytes + chunk.len() as u64 > limit {
                self.write_errors += 1;
                return Err(io::Error::other("fake sink failure"));
            }
        }
        self.pcm_bytes += chunk.len() as u64;
        Ok(())
    }

    fn bytes_written(&self) -> u64 {
        self.pcm_bytes
    }

    fn finalize(&mut self) -> io::Result<u64> {
        self.finalized = true;
        Ok(self.pcm_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_fields_are_canonical_pcm() {
        // mono 48k 16-bit, 6 bytes of data (3 frames).
        let h = wav_header(48_000, 1, 16, 6);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(h[4..8].try_into().unwrap()), 42);
        assert_eq!(&h[8..12], b"WAVE");
        assert_eq!(&h[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(h[20..22].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(h[22..24].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(h[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 96_000);
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(h[34..36].try_into().unwrap()), 16);
        assert_eq!(&h[36..40], b"data");
        assert_eq!(u32::from_le_bytes(h[40..44].try_into().unwrap()), 6);

        // stereo 48k 16-bit.
        let h = wav_header(48_000, 2, 16, 0);
        assert_eq!(u32::from_le_bytes(h[28..32].try_into().unwrap()), 192_000);
        assert_eq!(u16::from_le_bytes(h[32..34].try_into().unwrap()), 4);
    }

    #[test]
    fn wav_sink_writes_part_then_renames_with_patched_header() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-test-{}", std::process::id()));
        let final_path = dir.join("20261104T091432_manual_mic.wav");
        let _ = std::fs::remove_dir_all(&dir);
        let mut sink = WavSink::create(final_path.clone(), 48_000, 1, 16).expect("create");
        let part = crate::naming::part_path(&final_path);

        sink.write(&[1, 2, 3]).expect("write 1");
        sink.write(&[4, 5, 6]).expect("write 2");
        assert!(part.exists(), ".part file must exist while recording");
        assert!(!final_path.exists(), "final file must not exist yet");
        assert_eq!(sink.bytes_written(), 6);

        let n = sink.finalize().expect("finalize");
        assert_eq!(n, 6);
        assert!(!part.exists());
        let bytes = std::fs::read(&final_path).expect("read final");
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 6);

        // Finalize is idempotent-ish: second call reports the same size.
        let n2 = sink.finalize().expect("finalize twice");
        assert_eq!(n2, 6);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_sink_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-nested-{}", std::process::id()));
        let final_path = dir.join("2026").join("11").join("04").join("x_mic.wav");
        let mut sink = WavSink::create(final_path.clone(), 16_000, 2, 16).expect("create nested");
        sink.write(&[9u8; 8]).expect("write");
        sink.finalize().expect("finalize");
        assert!(final_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wav_sink_rejects_writes_beyond_wav_limit() {
        let dir = std::env::temp_dir().join(format!("watcher-audio-limit-{}", std::process::id()));
        let final_path = dir.join("big_mic.wav");
        let mut sink = WavSink::create(final_path.clone(), 48_000, 1, 16).expect("create");
        // Simulate the limit without allocating 4 GiB.
        sink.pcm_bytes = MAX_WAV_DATA_BYTES;
        let err = sink.write(&[0u8; 8]).expect_err("must refuse beyond limit");
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        // Still finalizes what it has.
        let n = sink.finalize().expect("finalize after refusal");
        assert_eq!(n, MAX_WAV_DATA_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fake_sink_counts_and_fails_on_demand() {
        let mut sink = FakeSink::new();
        sink.write(&[0u8; 100]).expect("write");
        assert_eq!(sink.bytes_written(), 100);
        assert!(sink.finalize().expect("finalize") == 100);
        assert!(sink.finalized);

        let mut flaky = FakeSink::failing_after(10);
        assert!(flaky.write(&[0u8; 8]).is_ok());
        assert!(flaky.write(&[0u8; 8]).is_err(), "must fail past the limit");
        assert_eq!(flaky.write_errors, 1);
    }
}
