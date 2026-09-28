//! SILK decoder internals (`silk/*` in libopus 1.5.2 / RFC 6716 §4.2).
//!
//! New module tree mirroring `celt/`, ported from the reference with
//! bit-exact integer arithmetic. Work is staged in the todo.md SILK
//! breakdown (Tiers 1–4).
//!
//! Implemented (Tier 1):
//! - [`tables`]: entropy-coding tables and NLSF codebooks.
//! - [`decode_indices`]: per-frame side-info indices — VAD/LBRR flags,
//!   signal type + quantization offset, then the gain/NLSF/pitch/LTP
//!   index fields. Also defines
//!   [`decode_indices::SideInfoIndices`], the provisional struct the
//!   Tier 2 modules code against.
//! - [`sigproc`]: fixed-point arithmetic helpers (`silk/SigProc_FIX.h`,
//!   `silk/macros.h`, `silk/Inlines.h`, `silk/sort.c` macros).
//! - [`stereo`]: mid/side predictor index decode and MS→LR unmixing
//!   (`silk/stereo_decode_pred.c`, `silk/stereo_MS_to_LR.c`).
//! - [`resampler`]: internal-rate (8/12/16 kHz) to output-rate
//!   conversion, ported bit-exactly from `silk/resampler*.c` (all four
//!   kernels: copy, 2x allpass upsample, IIR+FIR upscale, AR2+FIR
//!   downscale).
//! - [`gains`]: gain dequantization (RFC 6716 §4.2.4) — delta
//!   accumulation into the persistent `LastGainIndex` state, the
//!   inter-frame 16-step-down clamp (disabled on packet loss), and the
//!   `silk_log2lin` Q16 conversion (`silk/gain_quant.c`,
//!   `silk/log2lin.c`).
//!
//! Implemented (Tier 2):
//! - [`nlsf`]: NLSF decode (stage-1 + predictive stage-2 residuals),
//!   stabilization, inter-frame interpolation, NLSF→LPC conversion
//!   with bandwidth expansion and inverse-prediction-gain stability
//!   check (`silk/NLSF_decode.c`, `NLSF_stabilize.c`, `NLSF2A.c`,
//!   `LPC_fit.c`, `LPC_inv_pred_gain.c`, `bwexpander*.c`).
//! - [`pitch`]: pitch lag reconstruction (primary lag + per-subframe
//!   contour VQ offsets, clamped to the 2–18 ms search range) and the
//!   LTP filter-codebook lookup / LTP scale (`silk/decode_pitch.c` and
//!   the voiced branch of `silk/decode_parameters.c`).
//! - [`excitation`]: excitation decoding — per-block pulse counts with
//!   LSB-shift escapes, shell-coded pulse positions, LSB refinement,
//!   sign decode, and the seed-dithered Q14 excitation reconstruction
//!   (`silk/decode_pulses.c`, `shell_coder.c`, `code_signs.c`, the
//!   "Decode excitation" block of `decode_core.c`).
//!
//! Implemented (Tier 3):
//! - [`plc`]: packet loss concealment and comfort noise — reset/
//!   dispatch, good-frame parameter snapshot, excitation rewhitening +
//!   LTP/LPC re-synthesis with attenuating gains and drifting pitch,
//!   good-frame energy fade-in, and CNG estimation/insertion
//!   (`silk/PLC.c`, `silk/CNG.c`).
//! - [`synthesis`]: the inverse-NSQ / LTP / LPC synthesis core
//!   (`silk/decode_core.c`) — per-subframe gain application with
//!   gain-change rescaling of the short- and long-term states, LTP
//!   prediction over the re-whitened short-term residual for voiced
//!   subframes, the 10th/16th-order LPC synthesis filter, and the Q0
//!   output quantization.
//!
//! Encoder side (2026-09-26 foundation; see todo.md):
//! - [`encode_indices`]/[`encode_pulses`]: bit-exact mirrors of the
//!   decoder's side-info and excitation entropy coding.
//! - [`gains`]/[`nlsf_quant`]/[`ltp_quant`]: encoder halves of the gain,
//!   NLSF (`silk_NLSF_encode` + delayed-decision trellis) and LTP
//!   (`silk_VQ_WMat_EC`/`silk_quant_LTP_gains`) quantizers.
//! - [`lpc_analysis`]: float analysis kernels plus the exact
//!   fixed-point `silk_A2NLSF`.
//! - [`noise_shape`]: the noise-shaping analysis
//!   (`silk_noise_shape_analysis_FLP` + `silk_warped_autocorrelation_FLP`)
//!   — per-subframe warped-correlation gains (wired into the quantizer and
//!   the LPC analysis, +0.8-1.2 dB SNR) plus the shaping filter, tilt,
//!   harmonic gain and `Lambda`.
//! - [`nsq`]: closed-loop forward NSQ running the decoder's own
//!   arithmetic (bit-identical reconstruction).
//! - [`encoder`]: the top-level [`encoder::SilkEncoder`] frame assembly
//!   with the ported `control_SNR` rate tables.

pub mod decode_indices;
pub mod decoder;
pub mod encode_indices;
pub mod encode_pulses;
pub mod encoder;
pub mod excitation;
pub mod gains;
pub mod lpc_analysis;
pub mod ltp_quant;
pub mod nlsf;
pub mod nlsf_quant;
pub mod noise_shape;
pub mod nsq;
pub mod nsq_ref;
pub mod pitch;
pub mod plc;
pub mod resampler;
pub mod sigproc;
pub mod stereo;
pub mod synthesis;
pub mod tables;
pub mod vad;
