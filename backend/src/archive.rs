//! Archive backend for `io.dbx.files` (B-ARCHIVE route): `files/archiveList`
//! and `files/extract` over tar / tar.gz / tgz.
//!
//! Design points (PROGRESS-A-FILES §3 handover spec):
//! - **No new cargo dependencies.** The dependency tree carries no flate crate
//!   (checked Cargo.lock), so the gzip layer is a hand-written inflate
//!   (stored / fixed / dynamic Huffman blocks, RFC 1951) + gzip wrapper
//!   (RFC 1952, CRC32 + ISIZE verified). The tar layer is a hand-written
//!   512-byte-header parser (u star prefix, GNU `L` longname, PAX `x`/`g`
//!   extended headers with `path=`/`size=` overrides, base-256 numeric
//!   fields). zip is explicitly rejected as Phase 2.
//! - **Bomb guards**: entry-count cap (50k) and decompressed/payload size cap
//!   (1 GiB) bound both listing and extraction.
//! - **zip-slip safety**: every entry path goes through [`sanitize_entry_path`]
//!   (rejects `..`, absolute paths, Windows drive/backslash paths); link
//!   entries (typeflag `1`/`2`) are refused for extraction so a symlink can
//!   never redirect a write outside the target.
//! - Errors are `String` and surface as business errors -32000 via
//!   `main.rs::to_plugin_error`; payloads are camelCase over the wire.

#![allow(dead_code)]

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use opendal::Operator;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Limits (tar bomb guards)
// ---------------------------------------------------------------------------

/// Maximum number of real entries parsed from one archive.
pub const MAX_ARCHIVE_ENTRIES: usize = 50_000;
/// Maximum decompressed tar size (gzip) / total extracted payload (bytes).
pub const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// Maximum raw archive file size accepted for both methods (bytes).
pub const MAX_ARCHIVE_FILE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB
/// `files/extract` runs inline (synchronous `{success}`) when the archive
/// holds at most this many files…
pub const MAX_SYNC_EXTRACT_ENTRIES: usize = 10;
/// …and at most this many payload bytes; anything bigger degrades to a job.
pub const MAX_SYNC_EXTRACT_BYTES: u64 = 8 * 1024 * 1024;

/// Parser limits shared by listing and extraction.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Hard cap on real entries (tar bomb guard).
    pub max_entries: usize,
    /// Hard cap on the decompressed tar size / extracted payload (bytes).
    pub max_bytes: u64,
}

pub const DEFAULT_LIMITS: Limits = Limits {
    max_entries: MAX_ARCHIVE_ENTRIES,
    max_bytes: MAX_ARCHIVE_BYTES,
};

// ---------------------------------------------------------------------------
// Wire payloads
// ---------------------------------------------------------------------------

/// One entry as returned by `files/archiveList`. `kind` is `"file"` or
/// `"directory"` (A-FILES §3.1 handover contract); `path` is the entry path
/// *inside* the archive (no leading slash). `modifiedAt` is Unix epoch millis
/// (tar mtime is seconds; omitted when the header carries 0).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveEntry {
    pub name: String,
    pub path: String,
    pub kind: &'static str,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at: Option<u64>,
}

/// One regular-file entry planned for extraction: sanitized path (relative to
/// the extraction target) + payload bytes.
#[derive(Debug, Clone)]
pub struct ExtractEntry {
    pub path: String,
    pub data: Vec<u8>,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// Format detection + gzip layer
// ---------------------------------------------------------------------------

/// Normalizes the raw archive bytes to plain tar: zip is rejected as Phase 2,
/// gzip streams are fully inflated (size-capped), plain tar passes through
/// borrowed.
pub fn prepare<'a>(raw: &'a [u8], limits: &Limits) -> Result<Cow<'a, [u8]>, String> {
    if raw.len() >= 2 && &raw[..2] == b"PK" {
        return Err(
            "zip archives are not supported yet (Phase 2); use .tar / .tar.gz / .tgz".to_string(),
        );
    }
    if raw.len() >= 2 && raw[..2] == [0x1f, 0x8b] {
        Ok(Cow::Owned(gunzip(raw, limits.max_bytes)?))
    } else {
        Ok(Cow::Borrowed(raw))
    }
}

/// Human-readable kind for error messages.
pub fn kind_of(raw: &[u8]) -> &'static str {
    if raw.len() >= 2 && &raw[..2] == b"PK" {
        "zip"
    } else if raw.len() >= 2 && raw[..2] == [0x1f, 0x8b] {
        "tar.gz"
    } else {
        "tar"
    }
}

/// Inflates a gzip stream (RFC 1952 + RFC 1951). Verifies CRC32 and ISIZE;
/// enforces `max_out` while decoding (decompression bomb guard).
pub fn gunzip(raw: &[u8], max_out: u64) -> Result<Vec<u8>, String> {
    if raw.len() < 18 {
        return Err("gzip stream is too short".to_string());
    }
    if raw[2] != 8 {
        return Err(format!(
            "unsupported gzip compression method {} (only deflate/8 is supported)",
            raw[2]
        ));
    }
    let flg = raw[3];
    let mut pos = 10usize;
    // FEXTRA (bit 2): 2-byte LE length + payload.
    if flg & 0x04 != 0 {
        let len = raw
            .get(pos..pos + 2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize)
            .ok_or("truncated gzip header (FEXTRA)")?;
        pos += 2 + len;
    }
    // FNAME (bit 3) / FCOMMENT (bit 4): NUL-terminated strings.
    for flag in [0x08, 0x10] {
        if flg & flag != 0 {
            let end = raw[pos..]
                .iter()
                .position(|&b| b == 0)
                .ok_or("truncated gzip header (unterminated string)")?;
            pos += end + 1;
        }
    }
    // FHCRC (bit 1): 2-byte header CRC.
    if flg & 0x02 != 0 {
        pos += 2;
    }
    if pos + 8 >= raw.len() {
        return Err("truncated gzip stream".to_string());
    }
    let mut out = Vec::new();
    inflate(&raw[pos..raw.len() - 8], max_out, &mut out)?;
    let trailer = &raw[raw.len() - 8..];
    let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let isize_word = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if crc32(&out) != crc {
        return Err("gzip stream is corrupt (CRC32 mismatch)".to_string());
    }
    if out.len() as u32 != isize_word {
        return Err("gzip stream is corrupt (ISIZE mismatch)".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Minimal DEFLATE decoder (RFC 1951) — no flate crate in the dependency tree.
// ---------------------------------------------------------------------------

/// CRC-32 (IEEE 0xEDB88320 reflected), table generated at compile time.
const fn make_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0usize;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                0xEDB8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}

const CRC_TABLE: [u32; 256] = make_crc_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut state = 0xFFFF_FFFFu32;
    for &byte in data {
        state = CRC_TABLE[((state ^ byte as u32) & 0xff) as usize] ^ (state >> 8);
    }
    !state
}

/// LSB-first bit reader over a byte slice (DEFLATE bit order).
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            nbits: 0,
        }
    }

    fn fill(&mut self) {
        while self.nbits <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.nbits;
            self.pos += 1;
            self.nbits += 8;
        }
    }

    fn bits(&mut self, count: u32) -> Result<u32, String> {
        if count == 0 {
            return Ok(0);
        }
        self.fill();
        if self.nbits < count {
            return Err("unexpected end of deflate stream".to_string());
        }
        let value = (self.buf & ((1u64 << count) - 1)) as u32;
        self.buf >>= count;
        self.nbits -= count;
        Ok(value)
    }

    /// Discards bits up to the next byte boundary (for stored blocks).
    fn align_to_byte(&mut self) {
        let drop = self.nbits % 8;
        self.buf >>= drop;
        self.nbits -= drop;
    }

    /// Copies `count` bytes straight from the byte-aligned stream (stored
    /// blocks). Must be called after [`Self::align_to_byte`].
    fn copy_bytes(&mut self, out: &mut Vec<u8>, count: usize, max_out: u64) -> Result<(), String> {
        let buffered = (self.nbits / 8) as usize;
        let start = self.pos - buffered;
        if start + count > self.data.len() {
            return Err("unexpected end of deflate stream (stored block)".to_string());
        }
        if out.len() as u64 + count as u64 > max_out {
            return Err(format!(
                "decompressed archive exceeds the {max_out} byte guard (tar bomb?)"
            ));
        }
        out.extend_from_slice(&self.data[start..start + count]);
        self.pos = start + count;
        self.buf = 0;
        self.nbits = 0;
        Ok(())
    }
}

/// Canonical Huffman decoding table (puff-style counts + sorted symbols).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Self, String> {
        let mut counts = [0u16; 16];
        for &length in lengths {
            if length > 15 {
                return Err("invalid huffman code length".to_string());
            }
            counts[length as usize] += 1;
        }
        counts[0] = 0;
        // Over-subscription check (incomplete tables are legal in DEFLATE for
        // the distance tree; over-subscribed ones never are).
        let mut left: i32 = 1;
        for count in counts.iter().skip(1) {
            left <<= 1;
            left -= *count as i32;
            if left < 0 {
                return Err("over-subscribed huffman code set".to_string());
            }
        }
        let mut offsets = [0usize; 17];
        for len in 1..16 {
            offsets[len + 1] = offsets[len] + counts[len] as usize;
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[length as usize]] = symbol as u16;
                offsets[length as usize] += 1;
            }
        }
        Ok(Self { counts, symbols })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: usize = 0;
        for len in 1..=15 {
            code |= reader.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[index + (code - first) as usize]);
            }
            index += count as usize;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid huffman code in deflate stream".to_string())
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order in which code-length code lengths are stored (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

fn fixed_tables() -> (Huffman, Huffman) {
    let mut lengths = [0u8; 288];
    for (i, slot) in lengths.iter_mut().enumerate() {
        *slot = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // Fixed tables are structurally valid; construction cannot fail.
    let lit = Huffman::build(&lengths).expect("fixed literal table is valid");
    let dist = Huffman::build(&[5u8; 30]).expect("fixed distance table is valid");
    (lit, dist)
}

/// Decodes one block body (literal/length symbols) with the given tables.
fn decode_block(
    reader: &mut BitReader<'_>,
    lit: &Huffman,
    dist: &Huffman,
    out: &mut Vec<u8>,
    max_out: u64,
) -> Result<(), String> {
    loop {
        let symbol = lit.decode(reader)?;
        match symbol {
            0..=255 => {
                if out.len() as u64 >= max_out {
                    return Err(format!(
                        "decompressed archive exceeds the {max_out} byte guard (tar bomb?)"
                    ));
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let index = (symbol - 257) as usize;
                let length = LENGTH_BASE[index] as usize
                    + reader.bits(LENGTH_EXTRA[index] as u32)? as usize;
                let dist_symbol = dist.decode(reader)? as usize;
                if dist_symbol >= 30 {
                    return Err("invalid distance symbol in deflate stream".to_string());
                }
                let distance =
                    DIST_BASE[dist_symbol] as usize + reader.bits(DIST_EXTRA[dist_symbol] as u32)? as usize;
                if distance > out.len() {
                    return Err("deflate back-reference before start of output".to_string());
                }
                if out.len() + length > max_out as usize {
                    return Err(format!(
                        "decompressed archive exceeds the {max_out} byte guard (tar bomb?)"
                    ));
                }
                let start = out.len() - distance;
                for offset in 0..length {
                    let byte = out[start + offset];
                    out.push(byte);
                }
            }
            _ => return Err("invalid literal/length symbol in deflate stream".to_string()),
        }
    }
}

fn read_dynamic_tables(reader: &mut BitReader<'_>) -> Result<(Huffman, Huffman), String> {
    let hlit = reader.bits(5)? as usize + 257;
    let hdist = reader.bits(5)? as usize + 1;
    let hclen = reader.bits(4)? as usize + 4;
    let mut cl_lengths = [0u8; 19];
    // The HCLEN header carries only the first hclen lengths, transmitted in
    // the RFC 1951 §3.2.7 CL_ORDER permutation — not sequentially.
    for &slot in CL_ORDER.iter().take(hclen) {
        cl_lengths[slot] = reader.bits(3)? as u8;
    }
    let cl = Huffman::build(&cl_lengths)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut index = 0usize;
    while index < lengths.len() {
        let symbol = cl.decode(reader)?;
        match symbol {
            0..=15 => {
                lengths[index] = symbol as u8;
                index += 1;
            }
            16 => {
                if index == 0 {
                    return Err("repeat code with no previous length".to_string());
                }
                let previous = lengths[index - 1];
                let repeat = 3 + reader.bits(2)? as usize;
                if index + repeat > lengths.len() {
                    return Err("code length repeat overflows table".to_string());
                }
                for _ in 0..repeat {
                    lengths[index] = previous;
                    index += 1;
                }
            }
            17 => {
                let repeat = 3 + reader.bits(3)? as usize;
                index = write_zeros(&mut lengths, index, repeat)?;
            }
            18 => {
                let repeat = 11 + reader.bits(7)? as usize;
                index = write_zeros(&mut lengths, index, repeat)?;
            }
            _ => return Err("invalid code length symbol".to_string()),
        }
    }
    if lengths[256] == 0 {
        return Err("dynamic block has no end-of-block code".to_string());
    }
    let lit = Huffman::build(&lengths[..hlit])?;
    let dist = Huffman::build(&lengths[hlit..])?;
    Ok((lit, dist))
}

fn write_zeros(lengths: &mut [u8], index: usize, repeat: usize) -> Result<usize, String> {
    if index + repeat > lengths.len() {
        return Err("zero repeat overflows code length table".to_string());
    }
    for slot in &mut lengths[index..index + repeat] {
        *slot = 0;
    }
    Ok(index + repeat)
}

/// Inflates a raw DEFLATE stream into `out` (enforcing `max_out`).
fn inflate(src: &[u8], max_out: u64, out: &mut Vec<u8>) -> Result<(), String> {
    let mut reader = BitReader::new(src);
    loop {
        let bfinal = reader.bits(1)?;
        let btype = reader.bits(2)?;
        match btype {
            0 => {
                reader.align_to_byte();
                let len = reader.bits(16)? as usize;
                let nlen = reader.bits(16)? as usize;
                if len != (!nlen & 0xffff) {
                    return Err("stored block length check failed".to_string());
                }
                reader.copy_bytes(out, len, max_out)?;
            }
            1 => {
                let (lit, dist) = fixed_tables();
                decode_block(&mut reader, &lit, &dist, out, max_out)?;
            }
            2 => {
                let (lit, dist) = read_dynamic_tables(&mut reader)?;
                decode_block(&mut reader, &lit, &dist, out, max_out)?;
            }
            _ => return Err("invalid deflate block type 3".to_string()),
        }
        if bfinal == 1 {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// tar parsing (hand-written 512-byte header walker)
// ---------------------------------------------------------------------------

/// One parsed tar record after meta-entry resolution. `data` is the payload
/// slice for regular files (empty for directories / links / sparse meta).
struct RawEntry<'a> {
    path: String,
    typeflag: u8,
    size: u64,
    mtime: u64,
    data: &'a [u8],
    is_link: bool,
    is_dir: bool,
}

/// Reads a NUL/space-padded ASCII string field.
fn read_string(field: &[u8]) -> String {
    let end = field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end])
        .trim_end()
        .to_string()
}

/// Parses a tar numeric field: octal ASCII, or GNU base-256 when the high bit
/// of the first byte is set.
fn parse_numeric(field: &[u8]) -> Result<u64, String> {
    if field.is_empty() {
        return Err("empty tar numeric field".to_string());
    }
    if field[0] & 0x80 != 0 {
        // GNU base-256: first byte's high bit marks the extension; the value
        // is the remaining bits big-endian.
        let mut value: u64 = (field[0] & 0x7f) as u64;
        for &byte in &field[1..] {
            value = value
                .checked_mul(256)
                .and_then(|shifted| shifted.checked_add(byte as u64))
                .ok_or("tar numeric field overflows u64")?;
        }
        return Ok(value);
    }
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(&text, 8).map_err(|error| format!("invalid octal tar field '{text}': {error}"))
}

/// Header checksum: sum of all 512 bytes with the checksum field blanked to
/// ASCII spaces (the signing convention used by every mainstream tar writer).
fn header_checksum(header: &[u8]) -> u64 {
    header
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                0x20 as u64
            } else {
                byte as u64
            }
        })
        .sum()
}

/// Parses PAX extended-header records (`<len> <key>=<value>\n`).
fn parse_pax_records(payload: &[u8]) -> Vec<(String, String)> {
    let mut records = Vec::new();
    let mut pos = 0usize;
    while pos < payload.len() {
        let space = match payload[pos..].iter().position(|&b| b == b' ') {
            Some(space) => pos + space,
            None => break,
        };
        let length_text = String::from_utf8_lossy(&payload[pos..space]).to_string();
        let Ok(record_len) = length_text.trim().parse::<usize>() else {
            break;
        };
        if record_len == 0 || pos + record_len > payload.len() {
            break;
        }
        let record = &payload[pos..pos + record_len];
        if let Some(eq) = record.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&record[space - pos + 1..eq]).to_string();
            let mut value = &record[eq + 1..];
            if value.last() == Some(&b'\n') {
                value = &value[..value.len() - 1];
            }
            records.push((key, String::from_utf8_lossy(value).to_string()));
        }
        pos += record_len;
    }
    records
}

/// zip-slip guard: normalizes an entry path from the archive and refuses
/// anything that could escape the extraction target (`..` components,
/// absolute paths, Windows drive / UNC / backslash paths, empty results).
/// Accepted inputs are collapsed to a clean relative path (`./a` → `a`,
/// `a//b` → `a/b`).
pub fn sanitize_entry_path(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("unsafe archive entry path: empty".to_string());
    }
    if name.contains('\\') {
        return Err(format!(
            "unsafe archive entry path '{name}': backslash separators are refused (zip-slip guard)"
        ));
    }
    if name.starts_with('/') {
        return Err(format!(
            "unsafe archive entry path '{name}': absolute paths are refused (zip-slip guard)"
        ));
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return Err(format!(
            "unsafe archive entry path '{name}': windows drive paths are refused (zip-slip guard)"
        ));
    }
    let mut components: Vec<&str> = Vec::new();
    for component in name.split('/') {
        match component {
            "" | "." => continue,
            ".." => {
                return Err(format!(
                    "unsafe archive entry path '{name}': '..' traversal is refused (zip-slip guard)"
                ));
            }
            other => components.push(other),
        }
    }
    let normalized = components.join("/");
    if normalized.is_empty() {
        return Err(format!(
            "unsafe archive entry path '{name}': resolves to nothing"
        ));
    }
    Ok(normalized)
}

/// Walks the 512-byte tar stream, resolving GNU/PAX long-name meta entries
/// into final entry records. `collect_data` keeps payload slices for regular
/// files (extraction); listing skips them.
fn parse_tar<'a>(data: &'a [u8], limits: &Limits, collect_data: bool) -> Result<Vec<RawEntry<'a>>, String> {
    if data.len() < 512 {
        return Err(format!(
            "not a valid tar archive: {} bytes is smaller than one 512-byte header",
            data.len()
        ));
    }
    let mut entries: Vec<RawEntry<'a>> = Vec::new();
    let mut pending_longname: Option<String> = None;
    let mut pending_pax: Vec<(String, String)> = Vec::new();
    let mut global_pax: Vec<(String, String)> = Vec::new();
    let mut offset = 0usize;
    // Iteration cap bounds the loop even if every block is a meta entry.
    let max_iterations = limits.max_entries.saturating_mul(4) + 64;
    let mut iterations = 0usize;

    while offset + 512 <= data.len() {
        iterations += 1;
        if iterations > max_iterations {
            return Err(format!(
                "tar archive exceeds {} header blocks (tar bomb guard)",
                max_iterations
            ));
        }
        let header = &data[offset..offset + 512];
        if header.iter().all(|&byte| byte == 0) {
            break; // end-of-archive marker (first all-zero block terminates)
        }
        let stored_checksum = parse_numeric(&header[148..156]).map_err(|_| {
            format!("not a valid tar archive: header checksum mismatch at offset {offset}")
        })?;
        if header_checksum(header) != stored_checksum {
            return Err(format!(
                "not a valid tar archive: header checksum mismatch at offset {offset}"
            ));
        }
        let typeflag = header[156];
        let raw_size = parse_numeric(&header[124..136])?;
        let mtime = parse_numeric(&header[136..148]).unwrap_or(0);
        let header_name = read_string(&header[0..100]);
        let padded = (raw_size.div_ceil(512) * 512) as usize;
        let payload_end = offset + 512 + raw_size as usize;
        if payload_end > data.len() {
            return Err(format!(
                "truncated tar archive: entry at offset {offset} claims {raw_size} payload bytes past the end"
            ));
        }
        let payload = &data[offset + 512..payload_end];
        offset += 512 + padded;

        match typeflag {
            // GNU long name / long link name: payload applies to the next entry.
            b'L' => {
                pending_longname = Some(read_string(payload));
                continue;
            }
            b'K' => continue,
            // PAX extended header ('x' = next entry) / global ('g' = all).
            b'x' | b'X' => {
                pending_pax = parse_pax_records(payload);
                continue;
            }
            b'g' => {
                global_pax = parse_pax_records(payload);
                continue;
            }
            _ => {}
        }

        // Resolve the per-entry PAX overrides before slicing (a PAX `size`
        // may exceed what the octal header field can hold) and before the
        // pending map is cleared for the next entry.
        let lookup = |records: &[(String, String)], key: &str| {
            records
                .iter()
                .find(|(record_key, _)| record_key == key)
                .map(|(_, value)| value.clone())
        };
        let size_override = lookup(&pending_pax, "size");
        let path_override = lookup(&pending_pax, "path").or_else(|| lookup(&global_pax, "path"));
        pending_pax.clear();

        let mut size = raw_size;
        if let Some(size_text) = size_override {
            size = size_text
                .trim()
                .parse::<u64>()
                .map_err(|_| format!("invalid PAX size override '{size_text}'"))?;
        }
        // Recompute the payload window when the override moved it.
        let header_end = offset - padded;
        let payload_end = header_end + size as usize;
        if payload_end > data.len() {
            return Err(format!(
                "truncated tar archive: entry at offset {offset} claims {size} payload bytes past the end"
            ));
        }
        let payload = &data[header_end..payload_end];

        let mut name = pending_longname.take().unwrap_or_default();
        if let Some(pax_path) = path_override {
            name = pax_path;
        }
        if name.is_empty() {
            // ustar prefix field (POSIX: magic "ustar\0"); GNU ("ustar ") has
            // no prefix and carries long names via the 'L' entry handled above.
            let is_posix_ustar = &header[257..262] == b"ustar" && header[262] == 0;
            let prefix = if is_posix_ustar {
                read_string(&header[345..500])
            } else {
                String::new()
            };
            name = if prefix.is_empty() {
                header_name
            } else {
                format!("{prefix}/{header_name}")
            };
        }
        if name.is_empty() {
            continue;
        }

        let is_link = typeflag == b'1' || typeflag == b'2';
        let is_dir = typeflag == b'5' || name.ends_with('/');
        let path = sanitize_entry_path(&name)?;
        let is_regular = typeflag == b'0' || typeflag == 0 || typeflag == b'7';
        let data_slice = if collect_data && is_regular {
            &payload[..size.min(payload.len() as u64) as usize]
        } else {
            &[][..]
        };
        if entries.len() >= limits.max_entries {
            return Err(format!(
                "archive contains more than {} entries (tar bomb guard)",
                limits.max_entries
            ));
        }
        entries.push(RawEntry {
            path,
            typeflag,
            size: if is_link { 0 } else { size },
            mtime,
            data: data_slice,
            is_link,
            is_dir,
        });
    }
    if entries.is_empty() {
        return Err("not a valid tar archive: no entries found".to_string());
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Public operations
// ---------------------------------------------------------------------------

/// Full entry list for `files/archiveList` (archive order, pagination happens
/// at the call site). Accepts raw archive bytes (plain tar or gzip-wrapped;
/// zip is rejected as Phase 2).
pub fn list_entries(raw: &[u8], limits: &Limits) -> Result<Vec<ArchiveEntry>, String> {
    let data = prepare(raw, limits)?;
    let raw_entries = parse_tar(&data, limits, false)?;
    Ok(raw_entries
        .into_iter()
        .map(|entry| {
            let name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_string();
            ArchiveEntry {
                name,
                path: entry.path.clone(),
                kind: if entry.is_dir { "directory" } else { "file" },
                size: if entry.is_dir { 0 } else { entry.size },
                modified_at: if entry.mtime > 0 {
                    Some(entry.mtime.saturating_mul(1000))
                } else {
                    None
                },
            }
        })
        .collect())
}

/// Extraction plan for `files/extract`: regular files only (directories are
/// created implicitly from parent paths), every path sanitized, payload bytes
/// collected. Link entries are refused outright — a symlink in the archive
/// could redirect any subsequent write outside the target (zip-slip).
pub fn extract_entries(raw: &[u8], limits: &Limits) -> Result<Vec<ExtractEntry>, String> {
    let data = prepare(raw, limits)?;
    let raw_entries = parse_tar(&data, limits, true)?;
    let mut total: u64 = 0;
    let mut plan = Vec::new();
    for entry in raw_entries {
        if entry.is_link {
            return Err(format!(
                "refusing to extract archive entry '{}' (symbolic/hard link entries are rejected to prevent path escape)",
                entry.path
            ));
        }
        if entry.is_dir {
            continue;
        }
        total = total.saturating_add(entry.size);
        if total > limits.max_bytes {
            return Err(format!(
                "archive payload exceeds the {} byte extraction guard (tar bomb?)",
                limits.max_bytes
            ));
        }
        plan.push(ExtractEntry {
            path: entry.path,
            data: entry.data.to_vec(),
            size: entry.size,
        });
    }
    Ok(plan)
}

/// Pagination window shared with the `files/archiveList` handler: 1-based
/// `page`, `page_size >= 1`. Returns the `[start, end)` slice bounds into a
/// `total`-long list; out-of-range pages yield an empty window.
pub fn paginate(total: u64, page: u64, page_size: u64) -> (usize, usize) {
    let total_usize = total as usize;
    let start = page
        .saturating_sub(1)
        .saturating_mul(page_size.max(1)) as usize;
    if start >= total_usize {
        return (total_usize, total_usize);
    }
    let end = (start + page_size.max(1) as usize).min(total_usize);
    (start, end)
}

/// Reads the archive file from storage (size-capped) for either method.
pub async fn read_archive(operator: &Operator, path: &str) -> Result<Vec<u8>, String> {
    let metadata = operator
        .stat(path)
        .await
        .map_err(|error| format!("Failed to stat archive '{path}': {error}"))?;
    if metadata.mode().is_dir() {
        return Err(format!("Cannot read archive '{path}': it is a directory"));
    }
    let size = metadata.content_length();
    if size > MAX_ARCHIVE_FILE_BYTES {
        return Err(format!(
            "Archive '{path}' is {size} bytes; the archive methods cap input at \
             {MAX_ARCHIVE_FILE_BYTES} bytes"
        ));
    }
    let buffer = operator
        .read(path)
        .await
        .map_err(|error| format!("Failed to read archive '{path}': {error}"))?;
    Ok(buffer.to_vec())
}

/// Writes a planned extraction into `target_base` ("" = connection root):
/// creates parent directories lazily, writes each entry, honors the
/// cooperative `cancel` flag between entries (callers translate a
/// "extraction canceled" error into the Canceled job state by checking the
/// same flag). Returns `(files_written, bytes_written)`.
pub async fn write_entries<F>(
    operator: &Operator,
    plan: Vec<ExtractEntry>,
    target_base: &str,
    cancel: &AtomicBool,
    mut on_progress: F,
) -> Result<(u64, u64), String>
where
    F: FnMut(u64, u64),
{
    let base = target_base.trim().trim_matches('/');
    if !base.is_empty() {
        operator
            .create_dir(&format!("{base}/"))
            .await
            .map_err(|error| format!("Failed to create target directory '{base}': {error}"))?;
    }
    let mut created_dirs: HashSet<String> = HashSet::new();
    let mut files_done = 0u64;
    let mut bytes_done = 0u64;
    for entry in &plan {
        if cancel.load(Ordering::Acquire) {
            return Err("extraction canceled".to_string());
        }
        let target_file = if base.is_empty() {
            entry.path.clone()
        } else {
            format!("{base}/{}", entry.path)
        };
        let parent = match target_file.rfind('/') {
            Some(index) => target_file[..index].to_string(),
            None => String::new(),
        };
        if !parent.is_empty() && created_dirs.insert(parent.clone()) {
            operator
                .create_dir(&format!("/{parent}/"))
                .await
                .map_err(|error| format!("Failed to create directory '{parent}' during extract: {error}"))?;
        }
        operator
            .write(&target_file, entry.data.clone())
            .await
            .map_err(|error| format!("Failed to write extracted file '{target_file}': {error}"))?;
        files_done += 1;
        bytes_done += entry.size;
        on_progress(files_done, bytes_done);
    }
    Ok((files_done, bytes_done))
}

// ---------------------------------------------------------------------------
// Tests — fixtures are built in-process (minimal tar/gzip byte writers).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Appends one 512-byte tar header + payload padding for `data`.
    fn push_entry(buf: &mut Vec<u8>, name: &str, typeflag: u8, data: &[u8], mtime: u64) {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[156] = typeflag;
        let size_text = format!("{:011o}\0", data.len());
        header[124..124 + size_text.len()].copy_from_slice(size_text.as_bytes());
        let mtime_text = format!("{:011o}\0", mtime);
        header[136..136 + mtime_text.len()].copy_from_slice(mtime_text.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header
            .iter()
            .enumerate()
            .map(|(index, &byte)| if (148..156).contains(&index) { 0x20 } else { byte as u32 })
            .sum();
        let checksum_text = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_text.as_bytes());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(data);
        let padding = data.len().div_ceil(512) * 512 - data.len();
        buf.extend(std::iter::repeat(0u8).take(padding));
    }

    fn build_tar(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (name, typeflag, data) in entries {
            push_entry(&mut buf, name, *typeflag, data, 1_700_000_000);
        }
        buf.extend(std::iter::repeat(0u8).take(1024)); // end-of-archive blocks
        buf
    }

    /// Minimal gzip wrapper with a single stored deflate block.
    fn gzip_stored(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
        out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
        let len = payload.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&crc32(payload).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out
    }

    /// LSB-first bit writer for hand-built fixed-Huffman blocks.
    struct BitWriter {
        out: Vec<u8>,
        current: u8,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { out: Vec::new(), current: 0, nbits: 0 }
        }
        fn push_bit(&mut self, bit: u8) {
            self.current |= bit << self.nbits;
            self.nbits += 1;
            if self.nbits == 8 {
                self.out.push(self.current);
                self.current = 0;
                self.nbits = 0;
            }
        }
        /// Huffman codes travel MSB-first through the LSB-first bit stream.
        fn push_code(&mut self, code: u32, length: u32) {
            for offset in (0..length).rev() {
                self.push_bit(((code >> offset) & 1) as u8);
            }
        }
        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                self.out.push(self.current);
            }
            self.out
        }
    }

    /// gzip of literals 'a','b' via a fixed-Huffman block.
    fn gzip_fixed_ab() -> Vec<u8> {
        let mut bits = BitWriter::new();
        bits.push_bit(1); // BFINAL
        bits.push_bit(1); // BTYPE=01 (fixed) — LSB bit first
        bits.push_bit(0);
        for literal in [b'a', b'b'] {
            bits.push_code(0x30 + literal as u32, 8);
        }
        bits.push_code(256, 7); // end of block
        let deflate = bits.finish();
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
        out.extend_from_slice(&deflate);
        out.extend_from_slice(&crc32(b"ab").to_le_bytes());
        out.extend_from_slice(&2u32.to_le_bytes());
        out
    }

    // -- tar header parsing ---------------------------------------------------

    #[test]
    fn tar_parses_files_and_directories() {
        let tar = build_tar(&[
            ("docs/", b'5', b""),
            ("docs/readme.md", b'0', b"# hello"),
            ("empty.bin", b'0', b""),
        ]);
        let entries = list_entries(&tar, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "docs");
        assert_eq!(entries[0].kind, "directory");
        assert_eq!(entries[0].name, "docs");
        assert_eq!(entries[1].path, "docs/readme.md");
        assert_eq!(entries[1].kind, "file");
        assert_eq!(entries[1].size, 7);
        assert_eq!(entries[1].name, "readme.md");
        // mtime 1_700_000_000 s → millis
        assert_eq!(entries[1].modified_at, Some(1_700_000_000_000));
        let json = serde_json::to_string(&entries[1]).unwrap();
        assert!(json.contains("\"modifiedAt\":1700000000000"), "{json}");
        assert!(json.contains("\"kind\":\"file\""), "{json}");
    }

    #[test]
    fn tar_ustar_prefix_resolves_long_names() {
        let prefix = "very/long/prefix/path/that/keeps/going/deeper/and/deeper/still";
        let name = "segment-names-can-also-be-quite-long-but-under-100-chars.txt";
        assert!(prefix.len() <= 155 && name.len() <= 100);
        let mut buf = Vec::new();
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[156] = b'0';
        header[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        let size_text = format!("{:011o}\0", 3);
        header[124..124 + size_text.len()].copy_from_slice(size_text.as_bytes());
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u32 = header
            .iter()
            .enumerate()
            .map(|(index, &byte)| if (148..156).contains(&index) { 0x20 } else { byte as u32 })
            .sum();
        let checksum_text = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_text.as_bytes());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(b"abc");
        buf.extend(std::iter::repeat(0u8).take(509 + 1024));
        let entries = list_entries(&buf, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].path,
            format!("{prefix}/{name}"),
            "ustar prefix joins the name with '/'"
        );
    }

    #[test]
    fn tar_gnu_longname_entry_resolves() {
        let long_name = format!("{}.txt", "long".repeat(60)); // 240+ chars
        let mut buf = Vec::new();
        // GNU 'L' meta entry carries the real name as its payload.
        push_entry(&mut buf, "././@LongLink", b'L', long_name.as_bytes(), 0);
        push_entry(&mut buf, "truncated-name", b'0', b"data!", 0);
        buf.extend(std::iter::repeat(0u8).take(1024));
        let entries = list_entries(&buf, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 1, "meta entries are consumed: {entries:?}");
        assert_eq!(entries[0].path, long_name);
        assert_eq!(entries[0].size, 5);
    }

    #[test]
    fn tar_pax_path_and_size_overrides_apply() {
        let long_name = format!("pax/{}", "n".repeat(150));
        // PAX record: "<total-len> path=<value>\n" — the length counts itself,
        // so iterate the digit-count to a fixed point.
        let content_len = "path=".len() + long_name.len() + 1 + 1; // value + \n + space
        let mut record_len = content_len + 1;
        loop {
            let next = content_len + record_len.to_string().len();
            if next == record_len {
                break;
            }
            record_len = next;
        }
        let pax_record = format!("{record_len} path={long_name}\n");
        assert_eq!(pax_record.len(), record_len);
        let mut buf = Vec::new();
        push_entry(&mut buf, "PaxHeaders/0", b'x', pax_record.as_bytes(), 0);
        push_entry(&mut buf, "ignored", b'0', b"payload", 0);
        buf.extend(std::iter::repeat(0u8).take(1024));
        let entries = list_entries(&buf, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, long_name, "PAX path= overrides the header name");
    }

    #[test]
    fn tar_base256_numeric_fields_parse() {
        // size field with the high bit set: base-256 encoding of 70000.
        let mut header = [0u8; 512];
        header[..7].copy_from_slice(b"big.bin");
        header[156] = b'0';
        header[124] = 0x80;
        // 70000 = 0x11170 → big-endian, right-aligned in the 11 value bytes
        header[125..136].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0x00, 0x01, 0x11, 0x70]);
        let checksum: u32 = header
            .iter()
            .enumerate()
            .map(|(index, &byte)| if (148..156).contains(&index) { 0x20 } else { byte as u32 })
            .sum();
        let checksum_text = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum_text.as_bytes());
        // Full payload + end blocks so the length check passes.
        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend(std::iter::repeat(0u8).take(70000));
        tar.extend(std::iter::repeat(0u8).take(1024));
        let entries = list_entries(&tar, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries[0].size, 70000, "base-256 size field decodes");
    }

    #[test]
    fn tar_checksum_mismatch_is_rejected() {
        let mut tar = build_tar(&[("a.txt", b'0', b"x")]);
        tar[3] ^= 0xff; // corrupt a name byte → header checksum mismatch
        let error = list_entries(&tar, &DEFAULT_LIMITS).unwrap_err();
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn tar_not_a_tar_is_reported_clearly() {
        let junk = vec![0x42u8; 1024];
        let error = list_entries(&junk, &DEFAULT_LIMITS).unwrap_err();
        assert!(error.contains("not a valid tar archive"), "{error}");
        let short = vec![1, 2, 3];
        assert!(list_entries(&short, &DEFAULT_LIMITS).is_err());
    }

    // -- zip-slip -------------------------------------------------------------

    #[test]
    fn sanitize_rejects_escape_paths_table() {
        let rejected = [
            "../evil.txt",
            "/absolute/path",
            "a/../../up.txt",
            "..",
            "../",
            "C:\\win\\evil",
            "C:/win/evil",
            "back\\slash.txt",
            "./..",
            "",
            "   ",
        ];
        for name in rejected {
            let error = sanitize_entry_path(name).unwrap_err();
            assert!(
                error.contains("unsafe archive entry path"),
                "'{name}' must be rejected, got: {error}"
            );
        }
        let accepted = [
            ("a/b.txt", "a/b.txt"),
            ("./a.txt", "a.txt"),
            ("a//b//c", "a/b/c"),
            ("a/./b", "a/b"),
            ("dir/", "dir"),
        ];
        for (input, expected) in accepted {
            assert_eq!(sanitize_entry_path(input).unwrap(), expected, "'{input}'");
        }
    }

    #[test]
    fn tar_with_escape_entries_is_rejected_at_parse() {
        let tar = build_tar(&[("../escape.txt", b'0', b"x")]);
        let error = list_entries(&tar, &DEFAULT_LIMITS).unwrap_err();
        assert!(error.contains("zip-slip"), "{error}");
    }

    #[test]
    fn extract_refuses_link_entries() {
        let tar = build_tar(&[
            ("good.txt", b'0', b"ok"),
            ("evil-link", b'2', b"/etc/passwd"), // symlink target as payload
        ]);
        let error = extract_entries(&tar, &DEFAULT_LIMITS).unwrap_err();
        assert!(error.contains("symbolic/hard link"), "{error}");
        // Listing still works (links render as plain files there).
        let entries = list_entries(&tar, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn extract_plan_holds_only_files_with_payload() {
        let tar = build_tar(&[
            ("dir/", b'5', b""),
            ("dir/one.txt", b'0', b"first"),
            ("dir/two.txt", b'0', b"second!"),
        ]);
        let plan = extract_entries(&tar, &DEFAULT_LIMITS).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].path, "dir/one.txt");
        assert_eq!(plan[0].data, b"first");
        assert_eq!(plan[1].size, 7);
    }

    // -- bomb guards ------------------------------------------------------------

    #[test]
    fn entry_count_cap_is_enforced() {
        let limits = Limits { max_entries: 3, max_bytes: u64::MAX };
        let tar = build_tar(&[
            ("a", b'0', b"1"),
            ("b", b'0', b"2"),
            ("c", b'0', b"3"),
            ("d", b'0', b"4"),
        ]);
        let error = list_entries(&tar, &limits).unwrap_err();
        assert!(error.contains("tar bomb guard"), "{error}");
    }

    #[test]
    fn payload_size_cap_is_enforced_for_extract() {
        let limits = Limits { max_entries: 100, max_bytes: 10 };
        let tar = build_tar(&[("big.bin", b'0', &[0u8; 32][..])]);
        let error = extract_entries(&tar, &limits).unwrap_err();
        assert!(error.contains("extraction guard"), "{error}");
    }

    // -- gzip / inflate -----------------------------------------------------------

    #[test]
    fn gunzip_stored_roundtrip() {
        let payload = b"stored block payload, maybe longer than a few bytes".repeat(4);
        let gz = gzip_stored(&payload);
        assert_eq!(gunzip(&gz, u64::MAX).unwrap(), payload);
    }

    #[test]
    fn gunzip_fixed_huffman_roundtrip() {
        let gz = gzip_fixed_ab();
        assert_eq!(gunzip(&gz, u64::MAX).unwrap(), b"ab");
    }

    #[test]
    fn gunzip_rejects_corrupt_crc() {
        let mut gz = gzip_stored(b"hello world");
        let crc_index = gz.len() - 8;
        gz[crc_index] ^= 0xff;
        let error = gunzip(&gz, u64::MAX).unwrap_err();
        assert!(error.contains("CRC32"), "{error}");
    }

    #[test]
    fn gunzip_enforces_size_cap() {
        let gz = gzip_stored(&[0u8; 4096]);
        let error = gunzip(&gz, 1024).unwrap_err();
        assert!(error.contains("guard"), "{error}");
    }

    #[test]
    fn prepare_rejects_zip_as_phase2() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[0u8; 32]);
        let error = prepare(&zip, &DEFAULT_LIMITS).unwrap_err();
        assert!(error.contains("Phase 2"), "{error}");
        assert!(list_entries(&zip, &DEFAULT_LIMITS).unwrap_err().contains("Phase 2"));
    }

    #[test]
    fn prepare_inflates_gzip_and_keeps_plain_tar() {
        let tar = build_tar(&[("a.txt", b'0', b"zzz")]);
        let gz = gzip_stored(&tar);
        match prepare(&gz, &DEFAULT_LIMITS).unwrap() {
            Cow::Owned(data) => assert_eq!(data, tar),
            Cow::Borrowed(_) => panic!("gzip input must be inflated"),
        }
        match prepare(&tar, &DEFAULT_LIMITS).unwrap() {
            Cow::Borrowed(data) => assert_eq!(data, &tar[..]),
            Cow::Owned(_) => panic!("plain tar must pass through borrowed"),
        }
        assert_eq!(kind_of(&gz), "tar.gz");
        assert_eq!(kind_of(&tar), "tar");
    }

    #[test]
    fn gzip_wrapped_tar_lists_entries() {
        let tar = build_tar(&[
            ("pkg/", b'5', b""),
            ("pkg/inner.txt", b'0', b"inner payload"),
        ]);
        let gz = gzip_stored(&tar);
        let entries = list_entries(&gz, &DEFAULT_LIMITS).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].path, "pkg/inner.txt");
        assert_eq!(entries[1].size, 13);
    }

    // -- pagination ----------------------------------------------------------------

    #[test]
    fn paginate_boundaries_table() {
        // (total, page, page_size) → (start, end)
        let cases = [
            (5u64, 1u64, 2u64, (0usize, 2usize)),
            (5, 2, 2, (2, 4)),
            (5, 3, 2, (4, 5)),
            (5, 4, 2, (5, 5)),  // out of range → empty
            (5, 9, 2, (5, 5)),  // far out of range
            (0, 1, 2, (0, 0)),  // empty archive
            (3, 0, 2, (0, 2)),  // page 0 clamps to page 1
            (3, 1, 0, (0, 1)),  // page_size 0 clamps to 1
        ];
        for (total, page, page_size, expected) in cases {
            assert_eq!(
                paginate(total, page, page_size),
                expected,
                "paginate({total}, {page}, {page_size})"
            );
        }
    }
}
