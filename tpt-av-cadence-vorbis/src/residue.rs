//! Residue vectors: types 0, 1 and 2 (spec section 8).
//!
//! All scratch space lives in [`ResidueWorkspace`], allocated once at
//! decoder setup, so packet decode performs no allocation.

use crate::bitreader::BitReader;
use crate::codebook::Codebook;
use tpt_av_cadence_core::CadenceError;

/// Maximum codebook dimension tolerated in residue decode (guards the VQ
/// scratch buffer; real books are far below this).
pub(crate) const MAX_DIM: usize = 8192;

/// Preallocated scratch for [`Residue::decode`].
pub struct ResidueWorkspace {
    /// Type-2 interleaved vector, `channels * blocksize_1/2` samples.
    pub scratch: Box<[f32]>,
    /// Per-channel classification store, `channels * partitions_to_read`.
    all_classes: Box<[u32]>,
    /// One classword's classifications.
    classes: Box<[u32]>,
    /// One VQ value vector.
    vec_buf: Box<[f32]>,
}

impl ResidueWorkspace {
    pub fn new(channels: usize, blocksize_1: usize, max_classwords: usize) -> Self {
        let partitions_max = (blocksize_1 / 2) * channels;
        ResidueWorkspace {
            scratch: vec![0.0; channels * (blocksize_1 / 2)].into_boxed_slice(),
            all_classes: vec![0u32; partitions_max.max(8)].into_boxed_slice(),
            classes: vec![0u32; max_classwords.max(1)].into_boxed_slice(),
            vec_buf: vec![0.0; MAX_DIM].into_boxed_slice(),
        }
    }
}

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("vorbis residue: {what}"))
}

/// Reads one classword (a run of `classwords` classifications packed
/// MSB-first into a single classbook symbol) into `out`.
fn read_classifications(
    classbook: &Codebook,
    classifications: u32,
    classwords: usize,
    br: &mut BitReader,
    out: &mut [u32],
) -> Result<(), CadenceError> {
    let mut temp = classbook.read_scalar(br)? as u32;
    for i in (0..classwords).rev() {
        out[i] = temp % classifications;
        temp /= classifications;
    }
    Ok(())
}

/// A configured residue vector coder (spec 8.6.1).
pub struct Residue {
    pub residue_type: u8,
    pub begin: u32,
    pub end: u32,
    pub partition_size: u32,
    pub classifications: u32,
    pub classbook: u8,
    /// Book numbers per classification per pass; -1 = unused. Flattened
    /// `[classification * 8 + pass]`.
    pub books: Box<[i16]>,
}

impl Residue {
    /// Decodes `ch` residue vectors of length `n` into `vectors`, honoring
    /// `do_not_decode`. Every vector must hold `n` samples (already zeroed
    /// by the caller). `vectors` holds the submap bundle's channel buffers
    /// in bundle order.
    #[allow(clippy::too_many_arguments)]
    pub fn decode(
        &self,
        books: &[Codebook],
        br: &mut BitReader,
        vectors: &mut [Box<[f32]>],
        members: &[usize],
        do_not_decode: &[bool],
        ws: &mut ResidueWorkspace,
        n: usize,
    ) -> Result<(), CadenceError> {
        // The bundle size is the member count, not `vectors.len()` (which
        // holds every channel of the stream): multi-submap mappings route
        // a subset of channels per submap, e.g. a 5.1 stream's LFE alone.
        let ch = members.len();
        if ch == 0 {
            return Ok(());
        }
        if self.residue_type == 2 {
            self.decode_type2(books, br, vectors, members, do_not_decode, ws, n)
        } else {
            self.decode_type01(books, br, vectors, members, do_not_decode, ws, n)
        }
    }

    /// Types 0 and 1: per-channel vectors, differing only in intra-partition
    /// interleave (spec 8.6.2 with 8.6.3/8.6.4).
    #[allow(clippy::too_many_arguments)]
    fn decode_type01(
        &self,
        books: &[Codebook],
        br: &mut BitReader,
        vectors: &mut [Box<[f32]>],
        members: &[usize],
        do_not_decode: &[bool],
        ws: &mut ResidueWorkspace,
        n: usize,
    ) -> Result<(), CadenceError> {
        let ch = vectors.len();
        let begin = (self.begin.min(n as u32)) as usize;
        let end = (self.end.min(n as u32)) as usize;
        let n_to_read = end.saturating_sub(begin);
        if n_to_read == 0 {
            return Ok(());
        }
        let classbook = &books[self.classbook as usize];
        let classwords_per_codeword = classbook.dimensions;
        if classwords_per_codeword > ws.classes.len() {
            return Err(corrupt("classword dimension exceeds workspace"));
        }
        let partitions_to_read = n_to_read / self.partition_size as usize;
        if partitions_to_read == 0 {
            return Ok(());
        }
        if ch * partitions_to_read > ws.all_classes.len() {
            return Err(corrupt("classification workspace too small"));
        }
        let psize = self.partition_size as usize;

        for pass in 0..8u32 {
            let mut partition_count = 0usize;
            while partition_count < partitions_to_read {
                if pass == 0 {
                    for (j, skip) in do_not_decode.iter().enumerate().take(ch) {
                        if *skip {
                            continue;
                        }
                        read_classifications(
                            classbook,
                            self.classifications,
                            classwords_per_codeword,
                            br,
                            &mut ws.classes,
                        )?;
                        for (i, c) in ws.classes.iter().enumerate() {
                            let idx = partition_count + i;
                            if idx < partitions_to_read {
                                ws.all_classes[j * partitions_to_read + idx] = *c;
                            }
                        }
                    }
                }
                let mut i = 0usize;
                while i < classwords_per_codeword && partition_count < partitions_to_read {
                    for (j, skip) in do_not_decode.iter().enumerate().take(ch) {
                        if *skip {
                            continue;
                        }
                        let vqclass = ws.all_classes[j * partitions_to_read + partition_count];
                        let vqbook = self.books[vqclass as usize * 8 + pass as usize];
                        if vqbook >= 0 {
                            let book = &books[vqbook as usize];
                            let member = members[j];
                            let mut vec = &mut *vectors[member];
                            decode_partition(
                                self.residue_type,
                                book,
                                br,
                                &mut vec,
                                begin + partition_count * psize,
                                psize,
                                n,
                                &mut ws.vec_buf,
                            )?;
                        }
                    }
                    partition_count += 1;
                    i += 1;
                }
            }
        }
        Ok(())
    }

    /// Type 2: one interleaved vector of `ch * n` samples decoded as type 1,
    /// then deinterleaved (spec 8.6.5). When every channel is marked
    /// do-not-decode nothing is read and the (zeroed) vectors stand.
    #[allow(clippy::too_many_arguments)]
    fn decode_type2(
        &self,
        books: &[Codebook],
        br: &mut BitReader,
        vectors: &mut [Box<[f32]>],
        members: &[usize],
        do_not_decode: &[bool],
        ws: &mut ResidueWorkspace,
        n: usize,
    ) -> Result<(), CadenceError> {
        let ch = vectors.len();
        if do_not_decode.iter().all(|&d| d) {
            return Ok(());
        }
        let total = n * ch;
        if ws.scratch.len() < total {
            return Err(corrupt("interleave scratch too small"));
        }
        let begin = (self.begin.min(total as u32)) as usize;
        let end = (self.end.min(total as u32)) as usize;
        let n_to_read = end.saturating_sub(begin);
        if n_to_read == 0 {
            return Ok(());
        }
        let classbook = &books[self.classbook as usize];
        let classwords_per_codeword = classbook.dimensions;
        if classwords_per_codeword > ws.classes.len() {
            return Err(corrupt("classword dimension exceeds workspace"));
        }
        let partitions_to_read = n_to_read / self.partition_size as usize;
        if partitions_to_read == 0 {
            return Ok(());
        }
        let psize = self.partition_size as usize;
        ws.scratch[..total].fill(0.0);

        for pass in 0..8u32 {
            let mut partition_count = 0usize;
            while partition_count < partitions_to_read {
                // One classword per outer iteration (classifications for
                // the next `classwords_per_codeword` partitions), read in
                // pass 0 only — mirroring `decode_type01` and the spec's
                // per-chunk interleaving of classifications and VQ reads.
                if pass == 0 {
                    read_classifications(
                        classbook,
                        self.classifications,
                        classwords_per_codeword,
                        br,
                        &mut ws.classes,
                    )?;
                    for (k, c) in ws.classes.iter().enumerate() {
                        let idx = partition_count + k;
                        if idx < partitions_to_read {
                            ws.all_classes[idx] = *c;
                        }
                    }
                }
                let mut i = 0usize;
                while i < classwords_per_codeword && partition_count < partitions_to_read {
                    let vqclass = ws.all_classes[partition_count];
                    let vqbook = self.books[vqclass as usize * 8 + pass as usize];
                    if vqbook >= 0 {
                        let book = &books[vqbook as usize];
                        let mut inter = &mut ws.scratch[..total];
                        decode_partition(
                            1,
                            book,
                            br,
                            &mut inter,
                            begin + partition_count * psize,
                            psize,
                            total,
                            &mut ws.vec_buf,
                        )?;
                    }
                    partition_count += 1;
                    i += 1;
                }
            }
        }
        // Deinterleave into the channel vectors.
        for (j, &member) in members.iter().enumerate() {
            for (i, out) in vectors[member].iter_mut().enumerate().take(n) {
                *out = ws.scratch[i * ch + j];
            }
        }
        Ok(())
    }
}

/// Decodes one partition of `psize` scalars starting at `offset` into
/// `vector` (spec 8.6.3/8.6.4).
#[allow(clippy::too_many_arguments)]
fn decode_partition(
    residue_type: u8,
    book: &Codebook,
    br: &mut BitReader,
    vector: &mut &mut [f32],
    offset: usize,
    psize: usize,
    n: usize,
    vec_buf: &mut [f32],
) -> Result<(), CadenceError> {
    let dims = book.dimensions;
    if dims == 0 || dims > MAX_DIM {
        return Err(corrupt("book dimension out of range"));
    }
    if residue_type == 0 {
        if psize % dims != 0 {
            return Err(corrupt("partition size not a multiple of book dimension"));
        }
        let step = psize / dims;
        for p in 0..step {
            book.read_vector(br, vec_buf)?;
            for (d, &v) in vec_buf.iter().enumerate().take(dims) {
                let idx = offset + p + d * step;
                if idx < n {
                    vector[idx] += v;
                }
            }
        }
    } else {
        let mut done = 0usize;
        while done < psize {
            book.read_vector(br, vec_buf)?;
            for (d, &v) in vec_buf.iter().enumerate().take(dims) {
                let idx = offset + done + d;
                if idx < n {
                    vector[idx] += v;
                }
            }
            done += dims;
        }
    }
    Ok(())
}
