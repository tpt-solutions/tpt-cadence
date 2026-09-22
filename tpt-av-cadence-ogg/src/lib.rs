//! # tpt-av-cadence-ogg
//!
//! Ogg page/packet container layer (RFC 3533) shared by the codec crates
//! that ride in Ogg (Vorbis I, Opus per RFC 7845): capture-pattern and
//! header parsing, the Ogg CRC-32, and packet reassembly across page
//! boundaries via [`PageReader`].
//!
//! CRC checking follows libvorbis: a mismatch logs a warning but does not
//! fail the stream (deployed files exist with wrong CRCs).
//!
//! The reader is codec-agnostic — it exposes complete packets with the
//! granule position and BOS/EOS flags of the page each packet completed
//! on; header semantics (OpusHead, Vorbis identification headers, chained
//! links) belong to the codec crates.

use std::io::Read;

use log::warn;
use tpt_av_cadence_core::{BufferedSource, CadenceError, Result};

/// Maximum bytes of a single page (27 header + 255 segments + 65025 body).
const MAX_PAGE: usize = 27 + 255 + 255 * 255;

/// Metadata attached to a completed packet.
#[derive(Debug, Clone, Copy)]
pub struct PacketMeta {
    /// Granule position of the page on which the packet completed.
    pub granule: i64,
    /// The completing page carries the EOS flag.
    pub eos_page: bool,
    /// The completing page carries the BOS flag.
    pub bos_page: bool,
    /// Monotonic index of the completing page (page-boundary detection).
    pub page_index: u64,
}

/// Reassembles packets from the Ogg page sequence of one logical stream.
///
/// Chained links (a fresh BOS page after the stream started) end the
/// current link: [`PageReader::next_packet`] returns `None` there, and the
/// codec layer decides whether to open the next link.
pub struct PageReader {
    source: BufferedSource,
    crc_table: Box<[u32; 256]>,
    /// Complete page body of the page currently being consumed.
    body: Box<[u8]>,
    body_len: usize,
    body_pos: usize,
    /// Remaining lacing values of the current page.
    segments: Box<[u8; 255]>,
    segments_valid: usize,
    /// Packet currently being assembled across segments/pages.
    pending: Box<[u8]>,
    pending_len: usize,
    /// Set once the first page (BOS) has been consumed.
    started: bool,
    eof: bool,
    /// A new logical link (fresh BOS page) was encountered: the stream ends
    /// here for the first link.
    chain_end: bool,
    /// Monotonic page counter.
    page_index: u64,
    /// Granule of the current page (valid while segments remain).
    page_granule: i64,
    page_eos: bool,
    page_bos: bool,
}

fn corrupt(what: &str) -> CadenceError {
    CadenceError::CorruptData(format!("ogg: {what}"))
}

impl PageReader {
    /// Wraps a byte source (allocation confined here); `max_packet` bounds
    /// the largest reassemblable packet.
    pub fn new(source: BufferedSource, max_packet: usize) -> Self {
        PageReader {
            source,
            crc_table: Box::new(build_crc_table()),
            body: vec![0u8; MAX_PAGE].into_boxed_slice(),
            body_len: 0,
            body_pos: 0,
            segments: Box::new([0u8; 255]),
            segments_valid: 0,
            pending: vec![0u8; max_packet].into_boxed_slice(),
            pending_len: 0,
            started: false,
            eof: false,
            chain_end: false,
            page_index: 0,
            page_granule: 0,
            page_eos: false,
            page_bos: false,
        }
    }

    /// Rewinds the reader to an absolute byte offset (seek support). The
    /// codec layer re-establishes header state itself; packet and page
    /// accumulation state is dropped.
    pub fn reset(&mut self, pos: u64) -> Result<(), CadenceError> {
        self.source.seek_to(pos)?;
        self.clear_packet_state();
        // `started` stays set: a later BOS page marks a chained link.
        Ok(())
    }

    /// Rewinds to the first byte of the stream and forgets ALL page state,
    /// including the seen-BOS marker, so the leading header pages can be
    /// parsed again without the stream's own BOS page being mistaken for a
    /// chained link (which [`PageReader::reset`] would do).
    pub fn restart(&mut self) -> Result<(), CadenceError> {
        self.source.seek_to(0)?;
        self.clear_packet_state();
        self.started = false;
        Ok(())
    }

    fn clear_packet_state(&mut self) {
        self.body_len = 0;
        self.body_pos = 0;
        self.segments_valid = 0;
        self.pending_len = 0;
        self.eof = false;
        self.chain_end = false;
        self.page_index = 0;
        self.page_granule = 0;
        self.page_eos = false;
        self.page_bos = false;
    }

    /// The underlying buffered source (for seeking).
    pub fn source_mut(&mut self) -> &mut BufferedSource {
        &mut self.source
    }

    pub fn source(&self) -> &BufferedSource {
        &self.source
    }

    /// Reads the next complete packet into `out`, returning its byte length
    /// and metadata, or `None` at end of stream.
    pub fn next_packet(
        &mut self,
        out: &mut [u8],
    ) -> Result<Option<(usize, PacketMeta)>, CadenceError> {
        loop {
            // Serve a completed packet if the current segment ends one.
            while self.segments_valid > 0 {
                let seg = self.segments[0] as usize;
                // Shift the segment queue down by one.
                self.segments.copy_within(1.., 0);
                self.segments_valid -= 1;
                if self.pending_len + seg > self.pending.len() {
                    return Err(corrupt("packet exceeds the packet buffer"));
                }
                self.pending[self.pending_len..self.pending_len + seg]
                    .copy_from_slice(&self.body[self.body_pos..self.body_pos + seg]);
                self.pending_len += seg;
                self.body_pos += seg;
                if seg < 255 {
                    // Packet complete on this page.
                    let meta = PacketMeta {
                        granule: self.page_granule,
                        eos_page: self.page_eos,
                        bos_page: self.page_bos,
                        page_index: self.page_index,
                    };
                    let len = self.pending_len;
                    out[..len].copy_from_slice(&self.pending[..len]);
                    self.pending_len = 0;
                    return Ok(Some((len, meta)));
                }
            }
            if self.eof {
                return Ok(None);
            }
            self.refill_page()?;
            if self.chain_end {
                return Ok(None);
            }
        }
    }

    /// Reads and validates the next page, then serves its segments.
    fn refill_page(&mut self) -> Result<(), CadenceError> {
        let mut header = [0u8; 27];
        let mut got = 0usize;
        while got < 27 {
            let n = self
                .source
                .read(&mut header[got..])
                .map_err(CadenceError::from)?;
            if n == 0 {
                if got == 0 && self.pending_len == 0 {
                    self.eof = true;
                    return Ok(());
                }
                return Err(corrupt("truncated page header"));
            }
            got += n;
        }
        if &header[0..4] != b"OggS" {
            return Err(corrupt("bad capture pattern"));
        }
        if header[4] != 0 {
            return Err(corrupt("unsupported ogg version"));
        }
        // Safe: `header` is a fixed `[u8; 27]` array and each range below has
        // a length matching the target integer's byte width, so `try_into`
        // can never fail regardless of the bytes' values.
        let granule = i64::from_le_bytes(header[6..14].try_into().unwrap());
        let _serial = u32::from_le_bytes(header[14..18].try_into().unwrap());
        let _sequence = u32::from_le_bytes(header[18..22].try_into().unwrap());
        let crc_stored = u32::from_le_bytes(header[22..26].try_into().unwrap());
        let nsegs = header[26] as usize;
        let is_bos = header[5] & 0x02 != 0;

        if is_bos && self.started {
            // A fresh logical link: the first link's stream ends here.
            self.chain_end = true;
            return Ok(());
        }

        let mut seg_table = [0u8; 255];
        self.source.take_exact(&mut seg_table[..nsegs])?;
        let body_len = seg_table[..nsegs].iter().map(|&s| s as usize).sum();
        if body_len > self.body.len() {
            return Err(corrupt("page body exceeds maximum"));
        }
        self.source.take_exact(&mut self.body[..body_len])?;

        // CRC over the header with the CRC field zeroed, the segment table,
        // and the body.
        header[22..26].fill(0);
        let mut crc = 0u32;
        for chunk in [&header[..27], &seg_table[..nsegs], &self.body[..body_len]] {
            for &b in chunk.iter() {
                crc = (crc << 8) ^ self.crc_table[(((crc >> 24) as u8) ^ b) as usize];
            }
        }
        if crc != crc_stored {
            warn!("ogg page CRC mismatch (continuing, following libvorbis)");
        }

        self.segments[..nsegs].copy_from_slice(&seg_table[..nsegs]);
        self.segments_valid = nsegs;
        self.body_len = body_len;
        self.body_pos = 0;
        self.page_index += 1;
        self.page_granule = granule;
        self.page_eos = header[5] & 0x04 != 0;
        self.page_bos = is_bos;
        if self.page_bos {
            self.started = true;
        }
        Ok(())
    }
}

/// Ogg CRC-32: polynomial 0x04c11db7, MSB-first, init 0, no final XOR.
fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut r = (i as u32) << 24;
        for _ in 0..8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04c1_1db7
            } else {
                r << 1
            };
        }
        *entry = r;
    }
    table
}

/// CRC of a full page image: pass the header (with the 4 CRC bytes at
/// offset 22 zeroed), the segment table, and the page body as one slice or
/// concatenated parts.
pub fn page_crc(data: &[u8]) -> u32 {
    let table = build_crc_table();
    let mut crc = 0u32;
    for &b in data {
        crc = (crc << 8) ^ table[(((crc >> 24) as u8) ^ b) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vectors() {
        // Reference values for the Ogg CRC (poly 0x04c11db7, MSB-first).
        assert_eq!(page_crc(b""), 0);
        assert_eq!(page_crc(&[0x00]), 0);
        assert_eq!(page_crc(b"OggS"), 0x5fb0_a94f);
        assert_eq!(page_crc(&[1, 2, 3, 4]), 0xbe33_eab6);
    }

    #[test]
    fn assembles_packets_spanning_pages() {
        // Build a tiny ogg stream by hand: two pages, packet split across
        // them (a 300-byte packet: segments 255 + 45).
        let make_page = |granule: i64, htype: u8, segs: &[u8], body: &[u8]| {
            let mut header = vec![0u8; 27];
            header[0..4].copy_from_slice(b"OggS");
            header[4] = 0;
            header[5] = htype;
            header[6..14].copy_from_slice(&granule.to_le_bytes());
            header[26] = segs.len() as u8;
            let mut page = header;
            page.extend_from_slice(segs);
            page.extend_from_slice(body);
            let crc = page_crc(&page);
            page[22..26].copy_from_slice(&crc.to_le_bytes());
            page
        };
        let body: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let p1 = make_page(0, 0x02, &[255], &body[..255]);
        // Page 2 completes the packet (segment 45) and carries a 3-byte one.
        let p2 = make_page(1000, 0x04, &[45, 3], &{
            let mut b = body[255..300].to_vec();
            b.extend_from_slice(&[9, 9, 9]);
            b
        });
        let mut stream = p1;
        stream.extend_from_slice(&p2);

        let source = Box::new(std::io::Cursor::new(stream));
        let mut reader = PageReader::new(BufferedSource::new(source, 1024), 1024);
        let mut out = vec![0u8; 1024];

        let (len, meta) = reader.next_packet(&mut out).unwrap().unwrap();
        assert_eq!(len, 300);
        assert_eq!(meta.granule, 1000); // completed on page 2
        assert!(meta.eos_page);
        let (len, _) = reader.next_packet(&mut out).unwrap().unwrap();
        assert_eq!(len, 3);
        assert!(reader.next_packet(&mut out).unwrap().is_none());
    }
}
