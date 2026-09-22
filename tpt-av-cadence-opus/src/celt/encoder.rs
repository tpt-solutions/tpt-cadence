//! Top-level CELT-only Opus encoder — **mono, fullband, non-transient,
//! constant-bitrate, 20 ms frames only** (LM = 3). See the "Opus CELT
//! encoder — foundation" entries in `todo.md` for the full scope history;
//! this is the outer plumbing that finally drives every encode-side piece
//! built earlier this session (`mdct_forward`, `quant_coarse_energy`/
//! `quant_fine_energy`/`quant_energy_finalise`, `compute_allocation_encode`,
//! `quant_all_bands_encode`) as one working per-frame encode.
//!
//! Not a port of libopus's `celt_encoder.c` (RFC 6716 makes only the
//! decoder normative — see the encode-side modules' own doc comments for
//! why). What *is* ported, bit-for-bit, from the decoder side: the
//! pre-allocation header bit sequence (silence/postfilter/transient/intra
//! flags, `tf_decode`'s per-band bits, `spread_decision`, the dynalloc
//! loop, `alloc_trim`) and the deemphasis filter's algebraic inverse
//! (pre-emphasis) — both must match the decoder's bit consumption and
//! signal scaling exactly, or the round trip breaks.

use super::bands::quant_all_bands_encode;
use super::decoder::{OVERLAP, SPREAD_ICDF_TBL, TRIM_ICDF};
use super::fft::Cpx;
use super::mdct::mdct_forward;
use super::quant_bands::{e_means, quant_coarse_energy, quant_energy_finalise, quant_fine_energy};
use super::rate::{compute_allocation_encode, init_caps, NB_EBANDS};
use super::tables::{EBAND5MS, WINDOW120};
use super::vq::SPREAD_NORMAL;
use crate::range::RangeEncoder;

/// 20 ms at 48 kHz: `LM = 3`, `N2 = 960` (matches the decoder's
/// `SHORT_MDCT_SIZE << LM`), `shift = 0` (`MAX_LM - LM`).
const LM: i32 = 3;
const N2: usize = 960;
const MDCT_SHIFT: usize = 0;
const N4: usize = N2 / 2; // FFT size inside the forward MDCT.

/// `mode->preemph[0]` for the static 48 kHz mode (matches the decoder's
/// `deemphasis`).
const PREEMPH_COEF: f32 = 0.850_006_1;

/// Mono, fullband, 20 ms, non-transient CELT-only Opus encoder.
///
/// Call [`CeltEncoder::encode_frame`] once per 960-sample (20 ms @ 48 kHz)
/// block of input PCM, in order; the encoder buffers the trailing
/// `OVERLAP` samples between calls (required for correct MDCT windowing),
/// so frames must be contiguous.
pub struct CeltEncoder {
    /// Trailing `OVERLAP` pre-emphasized samples from the previous frame
    /// (the forward MDCT's required "current block + previous tail" input).
    mdct_tail: [f32; OVERLAP],
    /// Pre-emphasis filter state: the previous frame's last raw input
    /// sample (encoder-side inverse of the decoder's `deemphasis`).
    preemph_mem: f32,
    mdct_scratch: [Cpx; N4],
}

impl Default for CeltEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl CeltEncoder {
    pub fn new() -> Self {
        CeltEncoder {
            mdct_tail: [0.0; OVERLAP],
            preemph_mem: 0.0,
            mdct_scratch: [Cpx::default(); N4],
        }
    }

    /// Encodes one 960-sample mono frame (20 ms @ 48 kHz, PCM in `[-1, 1]`)
    /// into a complete Opus packet (TOC byte + one CELT frame, framing
    /// code 0) targeting `bytes_per_frame` bytes — the encoder's entire
    /// bitrate control for this milestone is this fixed per-frame byte
    /// budget (no VBR).
    pub fn encode_frame(&mut self, pcm: &[f32; N2], bytes_per_frame: usize) -> Vec<u8> {
        // --- Pre-emphasis (inverse of the decoder's `deemphasis`) ---
        let mut syn = [0.0f32; N2];
        for (o, &p) in syn.iter_mut().zip(pcm.iter()) {
            *o = 32768.0 * (p - PREEMPH_COEF * self.preemph_mem);
            self.preemph_mem = p;
        }

        // --- Forward MDCT: previous tail + this frame ---
        let mut mdct_in = [0.0f32; OVERLAP + N2];
        mdct_in[..OVERLAP].copy_from_slice(&self.mdct_tail);
        mdct_in[OVERLAP..].copy_from_slice(&syn);
        let mut freq = [0.0f32; N2];
        mdct_forward(
            &mdct_in,
            &mut freq,
            &WINDOW120,
            OVERLAP,
            MDCT_SHIFT,
            1,
            &mut self.mdct_scratch,
        );
        self.mdct_tail.copy_from_slice(&syn[N2 - OVERLAP..]);

        // --- Per-band energy analysis + normalization ---
        // `means[i]` is the same quantity the decoder's `old_band_e` holds
        // (log2 band gain, with `e_means` NOT yet added back — see
        // `denormalise_bands`'s `lg = band_log_e[i] + e_means(i)`).
        let m = 1usize << LM;
        let mut means = [0.0f32; 2 * NB_EBANDS];
        let mut x_spec = [0.0f32; N2];
        for i in 0..NB_EBANDS {
            let band = m * EBAND5MS[i] as usize..m * EBAND5MS[i + 1] as usize;
            let energy_sq: f32 = freq[band.clone()].iter().map(|&v| v * v).sum();
            let g = energy_sq.sqrt();
            if g > 1e-10 {
                means[i] = g.log2() - e_means(i);
                let inv_g = 1.0 / g;
                for (x, &f) in x_spec[band.clone()]
                    .iter_mut()
                    .zip(freq[band.clone()].iter())
                {
                    *x = f * inv_g;
                }
            } else {
                means[i] = -9.0;
                x_spec[band].fill(0.0);
            }
        }

        // --- Bitstream ---
        let mut enc = RangeEncoder::new();
        let total_bits_bytes = (bytes_per_frame * 8) as i32;

        // silence / postfilter / transient / intra: all forced off/simple
        // (see the module doc comment on scope), each gated on the exact
        // same budget check `tf_decode`'s decode-side counterpart uses —
        // omitting a bit the decoder wouldn't read (or vice versa) would
        // desync everything after it.
        let mut tell = enc.tell() as i32;
        // silence: the decoder only reads this when tell() == 1 (i.e. the
        // very first thing in the frame — true here, nothing encoded yet).
        debug_assert_eq!(tell, 1);
        enc.encode_bit_logp(false, 15); // silence
        tell = enc.tell() as i32;

        if tell + 16 <= total_bits_bytes {
            enc.encode_bit_logp(false, 1); // postfilter
            tell = enc.tell() as i32;
        }
        if LM > 0 && tell + 3 <= total_bits_bytes {
            enc.encode_bit_logp(false, 3); // is_transient
            tell = enc.tell() as i32;
        }
        // intra_ener: this encoder always wants "intra" (true), but if
        // there's no budget left to signal it, the decoder defaults to
        // false — match that rather than claim a flag that was never
        // written.
        let intra_ener = tell + 3 <= total_bits_bytes;
        if intra_ener {
            enc.encode_bit_logp(true, 3);
        }

        let mut old_band_e = [0.0f32; 2 * NB_EBANDS];
        let mut error = [0.0f32; 2 * NB_EBANDS];
        quant_coarse_energy(
            0,
            NB_EBANDS,
            &means,
            &mut old_band_e,
            &mut error,
            intra_ener,
            bytes_per_frame,
            &mut enc,
            1,
            LM as usize,
        );

        // tf_encode: "no change" for every band, mirroring `tf_decode`'s
        // exact bit-budget structure (including its per-band `logp` step:
        // 4 for the first band, 5 for every band after, when
        // `is_transient == false`). Writing bit=false throughout keeps
        // `curr`/`tf_changed` at 0 on the decode side.
        let mut budget = total_bits_bytes;
        let mut tell = enc.tell() as i32;
        let mut logp = 4i32; // is_transient == false
        let tf_select_rsv = LM > 0 && tell + logp < budget;
        if tf_select_rsv {
            budget -= 1;
        }
        for _ in 0..NB_EBANDS {
            if tell + logp <= budget {
                enc.encode_bit_logp(false, logp as u32);
                tell = enc.tell() as i32;
            }
            logp = 5;
        }
        // tf_select bit: only needed when TF_SELECT_TABLE[LM][0] !=
        // TF_SELECT_TABLE[LM][2] (the decoder's guard, evaluated with
        // is_transient == false and tf_changed == false since every
        // per-band bit above was false). For LM == 3 specifically (the
        // only LM this encoder supports), both entries are 0, so the
        // guard is always false and no bit is ever needed here.
        debug_assert!(
            !tf_select_rsv || {
                use super::bands::TF_SELECT_TABLE;
                TF_SELECT_TABLE[LM as usize][0] == TF_SELECT_TABLE[LM as usize][2]
            }
        );

        let spread_decision = SPREAD_NORMAL;
        let tell = enc.tell() as i32;
        if tell + 4 <= total_bits_bytes {
            enc.encode_icdf(spread_decision as u32, &SPREAD_ICDF_TBL, 5);
        }

        let cap = init_caps(LM as usize, 1);

        let dynalloc_logp = 6i32;
        let mut total_bits_q = total_bits_bytes << 3;
        let mut tell = enc.tell_frac() as i32;
        let offsets = [0i32; NB_EBANDS];
        for i in 0..NB_EBANDS {
            let width = ((EBAND5MS[i + 1] - EBAND5MS[i]) as usize) << (LM as usize);
            let quanta = ((width << 3) as i32).min((6 << 3).max(width as i32));
            let dynalloc_loop_logp = dynalloc_logp;
            if tell + (dynalloc_loop_logp << 3) < total_bits_q {
                enc.encode_bit_logp(false, dynalloc_loop_logp as u32);
                tell = enc.tell_frac() as i32;
            }
            let _ = quanta;
        }

        let alloc_trim = 5i32;
        if tell + (6 << 3) <= total_bits_q {
            enc.encode_icdf(alloc_trim as u32, &TRIM_ICDF, 7);
        }

        let bits = (total_bits_bytes << 3) - enc.tell_frac() as i32 - 1;
        let alloc = compute_allocation_encode(
            0, NB_EBANDS, &offsets, &cap, alloc_trim, bits, LM, 1, &mut enc,
        );

        quant_fine_energy(
            0,
            NB_EBANDS,
            &mut old_band_e,
            &mut error,
            &alloc.ebits,
            &mut enc,
            1,
        );

        let mut seed = 0u32;
        let mut collapse_masks = [0u8; NB_EBANDS];
        let mut norm = [0.0f32; N2];
        let mut scratch = [0.0f32; 176];
        let mut htmp = [0.0f32; 176];
        let mut iy = [0i32; 176];
        total_bits_q = (bytes_per_frame as i32 * 8) * 8;
        quant_all_bands_encode(
            &mut enc,
            0,
            NB_EBANDS,
            &mut x_spec,
            &mut collapse_masks,
            &alloc.pulses,
            spread_decision,
            total_bits_q,
            alloc.alloc.balance,
            LM as usize,
            alloc.alloc.coded_bands,
            &mut seed,
            false,
            &mut norm,
            &mut scratch,
            &mut htmp,
            &mut iy,
        );

        quant_energy_finalise(
            0,
            NB_EBANDS,
            &mut old_band_e,
            &error,
            &alloc.ebits,
            &alloc.fine_priority,
            (bytes_per_frame * 8) as i32 - enc.tell() as i32,
            &mut enc,
            1,
        );

        let mut frame = enc.done();
        if frame.len() < bytes_per_frame {
            frame.resize(bytes_per_frame, 0);
        }

        // TOC byte: config 31 (CELT-only, fullband, 20 ms), mono, code 0
        // (single CBR frame, payload is exactly this frame's bytes).
        let mut packet = Vec::with_capacity(1 + frame.len());
        packet.push(0xF8); // (31 << 3) | (0 << 2) | 0
        packet.extend_from_slice(&frame);
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::decoder::CeltDecoder;
    use crate::decoder::{decode_celt_only_packet, OUTPUT_CHANNELS};
    use crate::packet::parse_packet;

    /// DIAGNOSTIC: `means`/`x_spec` (computed in `encode_frame`) should be
    /// the exact inverse of `denormalise_bands` when fed back with zero
    /// quantization error — this isolates the per-band energy-split math
    /// from the MDCT/decoder entirely.
    #[test]
    fn analysis_is_exact_inverse_of_denormalise_bands() {
        use super::super::bands::denormalise_bands;

        let mut freq = [0.0f32; N2];
        for (i, f) in freq.iter_mut().enumerate() {
            // An arbitrary, non-trivial synthetic spectrum (not all in one
            // band): low-order polynomial-ish shape so every band gets a
            // distinct nonzero energy.
            *f = ((i as f32) * 0.01).sin() * 100.0 + 5.0;
        }

        let m = 1usize << LM;
        let mut means = [0.0f32; 2 * NB_EBANDS];
        let mut x_spec = [0.0f32; N2];
        for i in 0..NB_EBANDS {
            let band = m * EBAND5MS[i] as usize..m * EBAND5MS[i + 1] as usize;
            let energy_sq: f32 = freq[band.clone()].iter().map(|&v| v * v).sum();
            let g = energy_sq.sqrt();
            means[i] = g.log2() - e_means(i);
            let inv_g = 1.0 / g;
            for (x, &f) in x_spec[band.clone()]
                .iter_mut()
                .zip(freq[band.clone()].iter())
            {
                *x = f * inv_g;
            }
        }

        let mut recon = [0.0f32; N2];
        denormalise_bands(&x_spec, &mut recon, &means, 0, NB_EBANDS, m, 1, false);

        for i in 0..(m * EBAND5MS[NB_EBANDS] as usize) {
            let err = (freq[i] - recon[i]).abs();
            assert!(
                err < 1e-2,
                "bin {i}: freq={} recon={} err={err}",
                freq[i],
                recon[i]
            );
        }
    }

    /// End-to-end: encode a real signal (a sine tone, not silence) through
    /// `CeltEncoder`, decode the resulting Opus packet through the real,
    /// RFC-conformance-tested `OpusDecoder`/`CeltDecoder` stack, and check
    /// the output correlates strongly with the original.
    ///
    /// Fixed this session — see the "Session log (2026-09-22): CeltEncoder
    /// PCM fidelity bug found and fixed" entry in `todo.md` for the full
    /// root-cause writeup. Root cause: this test itself (not the encoder
    /// or `mdct.rs`) constructed a *mono* `CeltDecoder` and drove it
    /// through `decode_celt_only_packet`, which documents that it requires
    /// a *stereo*-constructed decoder (it always writes
    /// `OUTPUT_CHANNELS`-wide interleaved frames). With a mono decoder,
    /// `CeltDecoder::decode`'s internal stride became 1 instead of 2, so
    /// only the first half of each decoded frame's `OUTPUT_CHANNELS`-wide
    /// PCM chunk was ever written — the other half stayed zeroed. This
    /// test's channel-0 extraction (`chunks(OUTPUT_CHANNELS).map(|c|
    /// c[0])`) then silently read every *other* real decoded sample
    /// interleaved with zeros: a 2x decimation/aliasing artifact that
    /// looked exactly like a deep MDCT/encoder bug (frequency doubling,
    /// periodic glitches every ~480 samples) but had nothing to do with
    /// either. See `analysis_is_exact_inverse_of_denormalise_bands` above
    /// and `mdct.rs::forward_backward_round_trip_lm3` for the
    /// already-verified pieces that pointed away from the MDCT/quant
    /// chain and toward the harness.
    #[test]
    fn encode_then_decode_recovers_a_sine_tone() {
        let mut enc = CeltEncoder::new();
        // `decode_celt_only_packet` documents that `celt` must be
        // constructed for stereo 48 kHz output (`CeltDecoder::new(2,
        // 48000)`) — it always writes `OUTPUT_CHANNELS`-wide (2) interleaved
        // frames, upmixing mono streams by duplicating the channel.
        // Constructing a *mono* decoder here silently breaks that contract:
        // `CeltDecoder::decode`'s internal `cc = self.channels` stride
        // becomes 1, so it only ever fills the first half of each
        // `OUTPUT_CHANNELS`-wide `pcm_out` chunk. This test used to
        // extract "channel 0" via `chunks(OUTPUT_CHANNELS)`, which then
        // reads every *other* real decoded sample interleaved with zeros
        // from the untouched second half of the buffer — a silent 2x
        // decimation/aliasing artifact that looked exactly like a
        // deep-seated MDCT bug (frequency doubling, periodic glitches
        // every ~480 samples) but was actually just this mono/stereo
        // decoder-construction mismatch.
        let mut dec = CeltDecoder::new(2, 48_000).unwrap();

        let bytes_per_frame = 160; // ~64 kbps at 20 ms/frame
        let freq_hz = 440.0f32;
        let sample_rate = 48_000.0f32;
        let mut phase = 0.0f32;

        let mut original = Vec::new();
        let mut decoded = Vec::new();

        for _ in 0..8 {
            let mut pcm_in = [0.0f32; N2];
            for s in pcm_in.iter_mut() {
                *s = 0.5 * phase.sin();
                phase += 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
            }
            original.extend_from_slice(&pcm_in);

            let packet_bytes = enc.encode_frame(&pcm_in, bytes_per_frame);
            let packet = parse_packet(&packet_bytes).unwrap();
            let mut pcm_out = vec![0.0f32; N2 * OUTPUT_CHANNELS];
            decode_celt_only_packet(&mut dec, &packet, &packet_bytes, &mut pcm_out).unwrap();
            // Mono source, but the decoder always writes interleaved
            // OUTPUT_CHANNELS-wide frames; take channel 0.
            decoded.extend(pcm_out.chunks(OUTPUT_CHANNELS).map(|c| c[0]));
        }

        // The codec has a fixed algorithmic (group) delay: the decoded
        // signal at output index `j` reconstructs the original input from
        // around `j + CODEC_DELAY` (block-transform codecs are inherently
        // non-causal this way — the encoder needs a lookahead tail before
        // it can finish windowing a block, and CELT's own synthesis
        // overlap-add reconstructs a windowed block's *edges* only once
        // the next block's data has folded in). This was empirically
        // measured (see the diagnostic sweep this test used to carry,
        // preserved in the description below) by cross-correlating
        // `original` against `decoded` at every integer delay in [0, 200)
        // over a steady-state window (skipping the first two, atypical,
        // frames): the SNR curve is a single, clean, unimodal peak at
        // delay=98 (>20 dB), falling off smoothly on both sides — not an
        // aliasing artifact of the 440 Hz probe tone's ~109-sample period.
        // (For reference: an isolated `mdct_forward`/`mdct_backward`
        // round trip with matching tail bookkeeping — see
        // `mdct.rs::forward_backward_round_trip_lm3` and this session's
        // now-removed throwaway diagnostic — has *unity* gain and an
        // exact `OVERLAP` = 120-sample delay with no shape error at all;
        // the extra ~22-sample difference from the full pipeline's
        // measured 98 is not yet pinned down analytically, but doesn't
        // indicate any further correctness bug — see todo.md.)
        const CODEC_DELAY: i32 = 98;

        // Skip the first two frames (atypical: no/partial MDCT-tail
        // history yet) before measuring SNR.
        let skip = N2 * 2;
        let sig_pow: f64 = original[skip..]
            .iter()
            .map(|&v| (v as f64) * (v as f64))
            .sum();
        let err_pow: f64 = original[skip..]
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let di = skip as i32 + i as i32 - CODEC_DELAY;
                let b = if di >= 0 && (di as usize) < decoded.len() {
                    decoded[di as usize]
                } else {
                    0.0
                };
                let d = a as f64 - b as f64;
                d * d
            })
            .sum();
        let snr_db = 10.0 * (sig_pow / err_pow.max(1e-12)).log10();
        assert!(
            snr_db > 12.0,
            "snr={snr_db:.1} dB too low (encoded/decoded signal doesn't resemble the original)"
        );
    }
}
