//! Header decoding: identification, comment (skipped), and setup packets
//! (spec sections 4.2, 6.2.1, 7.2.2, 8.6.1).

use crate::bitreader::BitReader;
use crate::codebook::Codebook;
use crate::floor::{Floor, Floor0, Floor1};
use crate::residue::{Residue, MAX_DIM};
use tpt_av_cadence_core::CadenceError;

/// A parsed identification header.
#[derive(Debug, Clone)]
pub struct IdHeader {
    pub channels: u16,
    pub sample_rate: u32,
    /// Exponents: blocksize_0 = 1 << e0 <= blocksize_1 = 1 << e1.
    pub blocksize_0_exp: u32,
    pub blocksize_1_exp: u32,
}

/// A decoded audio-packet mode.
#[derive(Debug, Clone)]
pub struct Mode {
    /// `false` = short block, `true` = long block.
    pub blockflag: bool,
    pub mapping: usize,
}

/// Mapping type 0 (the only Vorbis I mapping).
#[derive(Debug, Clone)]
pub struct Mapping {
    pub coupling_steps: usize,
    pub magnitude: Box<[usize]>,
    pub angle: Box<[usize]>,
    pub submaps: usize,
    /// Channel -> submap (implicit 0 when `submaps == 1`).
    pub mux: Box<[u8]>,
    pub submap_floor: Box<[u8]>,
    pub submap_residue: Box<[u8]>,
}

/// Fully parsed setup: everything packet decode needs.
pub struct Setup {
    pub codebooks: Vec<Codebook>,
    pub floors: Vec<Floor>,
    pub residues: Vec<Residue>,
    pub mappings: Vec<Mapping>,
    pub modes: Vec<Mode>,
}

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("vorbis header: {what}"))
}

fn invalid(what: &str) -> CadenceError {
    CadenceError::InvalidFormat(format!("vorbis header: {what}"))
}

/// Verifies the common `\xNNvorbis` packet prefix; returns the packet type.
pub fn packet_type(packet: &[u8]) -> Result<u8, CadenceError> {
    if packet.len() < 7 || &packet[1..7] != b"vorbis" {
        return Err(invalid("not a vorbis header packet"));
    }
    Ok(packet[0])
}

/// Parses the identification header (30 bytes of payload).
pub fn parse_id(packet: &[u8]) -> Result<IdHeader, CadenceError> {
    if packet_type(packet)? != 1 {
        return Err(invalid("first packet is not the identification header"));
    }
    if packet.len() < 30 {
        return Err(invalid("identification header truncated"));
    }
    let payload = &packet[7..];
    let mut br = BitReader::new(payload);
    let version = br.read_bits(32)?;
    let channels = br.read_bits(8)? as u16;
    let sample_rate = br.read_bits(32)?;
    let _bitrate_max = br.read_bits(32)? as i32;
    let _bitrate_nominal = br.read_bits(32)? as i32;
    let _bitrate_min = br.read_bits(32)? as i32;
    let bs0 = br.read_bits(4)?;
    let bs1 = br.read_bits(4)?;
    let framing = br.read_bit()?;
    if version != 0 {
        return Err(invalid("unsupported vorbis version"));
    }
    if channels == 0 || sample_rate == 0 {
        return Err(invalid("invalid channels or sample rate"));
    }
    if !(6..=13).contains(&bs0) || !(6..=13).contains(&bs1) || bs0 > bs1 {
        return Err(invalid("illegal blocksizes"));
    }
    if !framing {
        return Err(invalid("identification header framing bit unset"));
    }
    Ok(IdHeader {
        channels,
        sample_rate,
        blocksize_0_exp: bs0,
        blocksize_1_exp: bs1,
    })
}

/// Validates the comment header and skips its content.
pub fn skip_comment(packet: &[u8]) -> Result<(), CadenceError> {
    if packet_type(packet)? != 3 {
        return Err(invalid("second packet is not the comment header"));
    }
    let payload = &packet[7..];
    // Lengths are plain little-endian u32s; validate bounds instead of
    // trusting them.
    let mut off = 0usize;
    let field = |off: &mut usize, payload: &[u8]| -> Result<u32, CadenceError> {
        if *off + 4 > payload.len() {
            return Err(corrupt("comment header truncated"));
        }
        let v = u32::from_le_bytes([
            payload[*off],
            payload[*off + 1],
            payload[*off + 2],
            payload[*off + 3],
        ]);
        *off += 4;
        Ok(v)
    };
    let vendor_len = field(&mut off, payload)? as usize;
    off += vendor_len;
    if off > payload.len() {
        return Err(corrupt("comment vendor string overruns packet"));
    }
    let count = field(&mut off, payload)? as usize;
    for _ in 0..count {
        let len = field(&mut off, payload)? as usize;
        off += len;
        if off > payload.len() {
            return Err(corrupt("comment list overruns packet"));
        }
    }
    // Framing bit: bit 0 of the next unread octet (LSB-first reader).
    if off >= payload.len() || payload[off] & 1 == 0 {
        return Err(corrupt("comment header framing bit unset"));
    }
    Ok(())
}

/// Parses the full setup header.
pub fn parse_setup(packet: &[u8], id: &IdHeader) -> Result<Setup, CadenceError> {
    if packet_type(packet)? != 5 {
        return Err(invalid("third packet is not the setup header"));
    }
    let mut br = BitReader::new(&packet[7..]);

    // Codebooks.
    let codebook_count = br.read_bits(8)? as usize + 1;
    let mut codebooks = Vec::with_capacity(codebook_count);
    for _i in 0..codebook_count {
        let sync = br.read_bits(24)?;
        if sync != 0x564342 {
            return Err(corrupt("codebook sync pattern mismatch"));
        }
        codebooks.push(Codebook::parse(&mut br)?);
    }

    // Time-domain transforms (placeholders, must be zero).
    let time_count = br.read_bits(6)? as usize + 1;
    for _ in 0..time_count {
        if br.read_bits(16)? != 0 {
            return Err(invalid("nonzero time-domain transform"));
        }
    }

    // Floors.
    let floor_count = br.read_bits(6)? as usize + 1;
    let mut floors = Vec::with_capacity(floor_count);
    for _ in 0..floor_count {
        match br.read_bits(16)? {
            0 => {
                let order = br.read_bits(8)? as usize;
                let rate = br.read_bits(16)?;
                let bark_map_size = br.read_bits(16)?;
                let amplitude_bits = br.read_bits(6)?;
                let amplitude_offset = br.read_bits(8)?;
                let num_books = br.read_bits(4)? as usize + 1;
                let mut books = Vec::with_capacity(num_books);
                for _ in 0..num_books {
                    let b = br.read_bits(8)? as u8;
                    if b as usize >= codebooks.len() {
                        return Err(corrupt("floor0 book number out of range"));
                    }
                    books.push(b);
                }
                if order == 0 || order > 64 {
                    return Err(corrupt("floor0 order out of range"));
                }
                floors.push(Floor::Zero(Floor0 {
                    order,
                    rate,
                    bark_map_size,
                    amplitude_bits,
                    amplitude_offset,
                    books: books.into_boxed_slice(),
                    maps: Vec::new(),
                }));
            }
            1 => {
                let partitions = br.read_bits(5)? as usize;
                let mut partition_class = vec![0u8; partitions];
                let mut maximum_class: i32 = -1;
                for c in partition_class.iter_mut() {
                    *c = br.read_bits(4)? as u8;
                    maximum_class = maximum_class.max(*c as i32);
                }
                // With zero partitions there are zero classes (the spec's
                // range `0..=maximum_class` is empty when maximum_class is
                // still its initial -1); parsing a phantom class here
                // desyncs the entire setup header.
                let classes = if partitions == 0 {
                    0
                } else {
                    maximum_class.max(0) as usize + 1
                };
                let mut class_dimensions = vec![0u8; classes];
                let mut class_subclasses = vec![0u8; classes];
                let mut class_masterbooks = vec![-1i16; classes];
                let mut subclass_books = vec![-1i16; classes * 8];
                for j in 0..classes {
                    class_dimensions[j] = br.read_bits(3)? as u8 + 1;
                    class_subclasses[j] = br.read_bits(2)? as u8;
                    if class_subclasses[j] > 0 {
                        let m = br.read_bits(8)? as i16;
                        if m as usize >= codebooks.len() {
                            return Err(corrupt("floor1 masterbook out of range"));
                        }
                        class_masterbooks[j] = m;
                    }
                    for k in 0..(1usize << class_subclasses[j]) {
                        let b = br.read_bits(8)? as i16 - 1;
                        if b >= 0 && b as usize >= codebooks.len() {
                            return Err(corrupt("floor1 subclass book out of range"));
                        }
                        subclass_books[j * 8 + k] = b;
                    }
                }
                let multiplier = br.read_bits(2)? + 1;
                let rangebits = br.read_bits(4)?;
                let blocksize_1 = 1usize << id.blocksize_1_exp;
                if rangebits == 0 && partitions > 0 {
                    return Err(invalid("floor1 rangebits 0 with partitions"));
                }
                let rangemax = 1usize << rangebits;
                if rangemax > blocksize_1 / 2 {
                    return Err(invalid("floor1 range exceeds the long blocksize"));
                }
                let mut x = vec![0u32; 2];
                x[1] = rangemax as u32;
                for &class_byte in partition_class.iter() {
                    let class = class_byte as usize;
                    for _ in 0..class_dimensions[class] {
                        if x.len() >= 65 {
                            return Err(invalid("floor1 x_list exceeds 65 entries"));
                        }
                        x.push(br.read_bits(rangebits)?);
                    }
                }
                let values = x.len();
                eprintln!("DBG floor1 values={values} at bit {}", br.bit_pos);
                // Precompute neighbors and sort order (spec 7.2.2/FFmpeg
                // ff_vorbis_ready_floor1_list).
                let mut low = vec![0usize; values];
                let mut high = vec![1usize; values];
                let mut sort = (0..values).collect::<Vec<_>>();
                for i in 2..values {
                    for j in 2..i {
                        if x[j] < x[i] {
                            if x[j] > x[low[i]] {
                                low[i] = j;
                            }
                        } else if x[j] < x[high[i]] {
                            high[i] = j;
                        }
                    }
                }
                for i in 0..values - 1 {
                    for j in i + 1..values {
                        if x[i] == x[j] {
                            return Err(invalid("floor1 duplicate x value"));
                        }
                        if x[sort[i]] > x[sort[j]] {
                            sort.swap(i, j);
                        }
                    }
                }
                floors.push(Floor::One(Floor1 {
                    partitions,
                    partition_class: partition_class.into_boxed_slice(),
                    class_dimensions: class_dimensions.into_boxed_slice(),
                    class_subclasses: class_subclasses.into_boxed_slice(),
                    class_masterbooks: class_masterbooks.into_boxed_slice(),
                    subclass_books: subclass_books.into_boxed_slice(),
                    multiplier,
                    x: x.into_boxed_slice(),
                    low: low.into_boxed_slice(),
                    high: high.into_boxed_slice(),
                    sort: sort.into_boxed_slice(),
                }));
            }
            other => return Err(invalid(&format!("unsupported floor type {other}"))),
        }
    }

    // Residues.
    let residue_count = br.read_bits(6)? as usize + 1;
    eprintln!("DBG residues={residue_count} at bit {}", br.bit_pos);
    let mut residues = Vec::with_capacity(residue_count);
    for _ in 0..residue_count {
        let residue_type = br.read_bits(16)? as u8;
        if residue_type > 2 {
            return Err(invalid("unsupported residue type"));
        }
        let begin = br.read_bits(24)?;
        let end = br.read_bits(24)?;
        let partition_size = br.read_bits(24)? + 1;
        let classifications = br.read_bits(6)? + 1;
        let classbook = br.read_bits(8)? as u8;
        if classbook as usize >= codebooks.len() {
            return Err(corrupt("residue classbook out of range"));
        }
        // The spec reads the cascade vectors of ALL classifications first,
        // then a second pass supplies the value-book number for every set
        // cascade bit (spec §8.2; FFmpeg's two-loop parse). Interleaving
        // the two reads desyncs the bitstream after the first cascade.
        let mut cascades = vec![0u8; classifications as usize];
        for cascade in cascades.iter_mut() {
            let low_bits = br.read_bits(3)? as u8;
            let mut high_bits = 0u8;
            if br.read_bit()? {
                high_bits = br.read_bits(5)? as u8;
            }
            *cascade = high_bits.wrapping_mul(8).wrapping_add(low_bits);
        }
        let mut books = vec![-1i16; classifications as usize * 8];
        for (i, &cascade) in cascades.iter().enumerate() {
            for j in 0..8 {
                if cascade & (1 << j) != 0 {
                    let b = br.read_bits(8)? as i16;
                    if b as usize >= codebooks.len() {
                        return Err(corrupt("residue book out of range"));
                    }
                    if !codebooks[b as usize].has_lookup() {
                        return Err(corrupt("residue book without a value mapping"));
                    }
                    books[i * 8 + j] = b;
                }
            }
        }
        residues.push(Residue {
            residue_type,
            begin,
            end,
            partition_size,
            classifications,
            classbook,
            books: books.into_boxed_slice(),
        });
    }

    // Mappings.
    let mapping_count = br.read_bits(6)? as usize + 1;
    let channels = id.channels as usize;
    let chan_bits = BitReader::ilog(channels as i64 - 1);
    let mut mappings = Vec::with_capacity(mapping_count);
    for _ in 0..mapping_count {
        if br.read_bits(16)? != 0 {
            return Err(invalid("unsupported mapping type"));
        }
        let submaps = if br.read_bit()? {
            br.read_bits(4)? as usize + 1
        } else {
            1
        };
        let (coupling_steps, magnitude, angle) = if br.read_bit()? {
            if channels < 2 {
                return Err(invalid("coupling with fewer than two channels"));
            }
            let steps = br.read_bits(8)? as usize + 1;
            let mut mag = vec![0usize; steps];
            let mut ang = vec![0usize; steps];
            for k in 0..steps {
                mag[k] = br.read_bits(chan_bits)? as usize;
                ang[k] = br.read_bits(chan_bits)? as usize;
                if mag[k] >= channels || ang[k] >= channels || mag[k] == ang[k] {
                    return Err(invalid("illegal coupling channel"));
                }
            }
            (steps, mag.into_boxed_slice(), ang.into_boxed_slice())
        } else {
            (0usize, Box::default(), Box::default())
        };
        if br.read_bits(2)? != 0 {
            return Err(invalid("nonzero reserved mapping field"));
        }
        let mut mux = vec![0u8; channels];
        if submaps > 1 {
            for m in mux.iter_mut() {
                *m = br.read_bits(4)? as u8;
                if *m as usize >= submaps {
                    return Err(corrupt("channel mux exceeds submap count"));
                }
            }
        }
        let mut submap_floor = vec![0u8; submaps];
        let mut submap_residue = vec![0u8; submaps];
        for j in 0..submaps {
            let _time = br.read_bits(8)?; // placeholder, discarded
            submap_floor[j] = br.read_bits(8)? as u8;
            submap_residue[j] = br.read_bits(8)? as u8;
            if submap_floor[j] as usize >= floors.len() {
                return Err(corrupt("submap floor out of range"));
            }
            if submap_residue[j] as usize >= residues.len() {
                return Err(corrupt("submap residue out of range"));
            }
        }
        mappings.push(Mapping {
            coupling_steps,
            magnitude,
            angle,
            submaps,
            mux: mux.into_boxed_slice(),
            submap_floor: submap_floor.into_boxed_slice(),
            submap_residue: submap_residue.into_boxed_slice(),
        });
    }

    eprintln!(
        "DBG mapping dump: {} mappings; {:?}",
        mappings.len(),
        mappings
            .iter()
            .map(|m| (
                m.submaps,
                m.coupling_steps,
                m.magnitude.to_vec(),
                m.angle.to_vec(),
                m.mux.to_vec(),
                m.submap_floor.to_vec(),
                m.submap_residue.to_vec()
            ))
            .collect::<Vec<_>>()
    );
    // Modes.
    let mode_count = br.read_bits(6)? as usize + 1;
    let mut modes = Vec::with_capacity(mode_count);
    for _ in 0..mode_count {
        let blockflag = br.read_bit()?;
        let windowtype = br.read_bits(16)?;
        let transformtype = br.read_bits(16)?;
        if windowtype != 0 || transformtype != 0 {
            return Err(invalid("mode window/transform type must be zero"));
        }
        let mapping = br.read_bits(8)? as usize;
        if mapping >= mappings.len() {
            return Err(corrupt("mode mapping out of range"));
        }
        modes.push(Mode { blockflag, mapping });
    }
    if !br.read_bit()? {
        return Err(corrupt("setup header framing bit unset"));
    }

    Ok(Setup {
        codebooks,
        floors,
        residues,
        mappings,
        modes,
    })
}

/// The largest codebook dimension found among residue classbooks (workspace
/// sizing).
pub fn max_classbook_dimensions(setup: &Setup) -> usize {
    setup
        .residues
        .iter()
        .map(|r| setup.codebooks[r.classbook as usize].dimensions)
        .max()
        .unwrap_or(1)
        .min(MAX_DIM)
}
