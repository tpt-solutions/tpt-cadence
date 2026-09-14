//! FFmpeg subprocess comparison harness.
//!
//! Decodes a file with the suite's decoder and with FFmpeg (via CLI
//! subprocess, emitting raw interleaved `f32le`), then asserts the PCM
//! output matches within a tolerance. Use tolerance `0.0` for bit-exact
//! integer formats and a small epsilon for float formats whose decode path
//! may differ by one ULP.
//!
//! All conformance tests should skip gracefully when FFmpeg is not installed
//! (see [`ffmpeg_available`]); CI pins an FFmpeg build so the comparison
//! always runs there.

use std::path::Path;
use std::process::Command;

use tpt_av_cadence_core::{CadenceError, Decoder};

/// Errors from the conformance harness.
#[derive(Debug, thiserror::Error)]
pub enum ConformanceError {
    /// FFmpeg is not installed or not on `PATH`.
    #[error("FFmpeg is not available on PATH")]
    ReferenceUnavailable,
    /// FFmpeg exited with an error.
    #[error("FFmpeg failed: {0}")]
    FfmpegFailed(String),
    /// The decoder returned an error.
    #[error("decoder error: {0}")]
    Decode(#[from] CadenceError),
    /// PCM output diverged beyond the tolerance.
    #[error(
        "PCM mismatch at sample {index}: tpt={tpt}, ffmpeg={reference} (tolerance {tolerance})"
    )]
    Mismatch {
        index: usize,
        tpt: f32,
        reference: f32,
        tolerance: f32,
    },
    /// Sample counts diverged.
    #[error("length mismatch: tpt produced {tpt} samples, ffmpeg produced {reference}")]
    LengthMismatch { tpt: usize, reference: usize },
    /// Process/I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Cheap probe for FFmpeg availability. Callers use it to skip conformance
/// tests on machines without FFmpeg (CI always has it).
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Decodes `path` with FFmpeg into raw interleaved `f32` samples.
pub fn decode_with_ffmpeg(path: &Path) -> Result<Vec<f32>, ConformanceError> {
    let output = Command::new("ffmpeg")
        .args(["-v", "quiet", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-acodec", "pcm_f32le", "-"])
        .output()?;
    if !output.status.success() {
        return Err(ConformanceError::FfmpegFailed(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(output
        .stdout
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
}

/// Decodes a file with the suite's decoder and with FFmpeg, then asserts the
/// PCM output matches within `tolerance` (`0.0` = bit-exact).
///
/// Returns [`ConformanceError::ReferenceUnavailable`] when FFmpeg is not
/// installed; tests typically early-return `Ok(())` in that case.
pub fn assert_bit_exact_vs_ffmpeg(
    file_path: &Path,
    tpt_decoder: &mut dyn Decoder,
    tolerance: f32,
) -> Result<(), ConformanceError> {
    if !ffmpeg_available() {
        return Err(ConformanceError::ReferenceUnavailable);
    }
    let reference = decode_with_ffmpeg(file_path)?;

    let channels = tpt_decoder.info().channels as usize;
    let mut tpt = Vec::new();
    let mut buf = vec![0.0f32; 4096 * channels.max(1)];
    loop {
        let frames = tpt_decoder.decode(&mut buf)?;
        if frames == 0 {
            break;
        }
        tpt.extend_from_slice(&buf[..frames * channels]);
    }

    if tpt.len() != reference.len() {
        return Err(ConformanceError::LengthMismatch {
            tpt: tpt.len(),
            reference: reference.len(),
        });
    }
    for (index, (a, b)) in tpt.iter().zip(reference.iter()).enumerate() {
        if (a - b).abs() > tolerance {
            return Err(ConformanceError::Mismatch {
                index,
                tpt: *a,
                reference: *b,
                tolerance,
            });
        }
    }
    Ok(())
}
