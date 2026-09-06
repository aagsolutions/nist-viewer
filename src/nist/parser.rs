/*
 * Copyright (c) 2026 Aurel Avramescu.
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do
 * so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND,
 * EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
 * MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 * NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT
 * HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
 * WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
 * OTHER DEALINGS IN THE SOFTWARE.
 */

//! Streaming-style parser for ANSI/NIST-ITL Traditional Encoding files.

use anyhow::{anyhow, bail, Context, Result};

use super::format::*;
use super::records::{NistRecord, TextField};

/// A parsed NIST biometric file.
#[derive(Debug, Clone)]
pub struct NistFile {
    /// The Type-1 Transaction Information record (always present).
    pub transaction: NistRecord,
    /// All records after the Type-1 record, in source order.
    pub records: Vec<NistRecord>,
    /// Bytes consumed from the input. Useful for diagnostic messages.
    pub bytes_read: usize,
}

impl NistFile {
    /// Returns the iterator over all records (Type-1 first, then the rest).
    pub fn all_records(&self) -> impl Iterator<Item = &NistRecord> {
        std::iter::once(&self.transaction).chain(self.records.iter())
    }

    /// Returns all records whose type matches `record_type`, including the Type-1.
    pub fn records_of_type(&self, record_type: u8) -> Vec<&NistRecord> {
        self.all_records()
            .filter(|r| r.record_type == record_type)
            .collect()
    }
}

/// Charset decoder strategy for the textual fields. Mirrors the values declared
/// in the Type-1 `DCS` field.
#[derive(Debug, Clone, Copy)]
enum Charset {
    /// Default — Latin/Windows codepage (matches NIST's "000" default).
    DEFAULT,
    /// UTF-16 (`002`).
    UTF16,
    /// UTF-8 (`003`).
    UTF8,
}

/// Streaming cursor over a NIST byte buffer.
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    /// Reads until one of `separators` is hit, returning the bytes before it.
    /// Stops at the buffer end without advancing past the missing separator.
    fn read_until(&mut self, separators: &[u8]) -> Vec<u8> {
        let start = self.pos;
        while let Some(&b) = self.bytes.get(self.pos) {
            if separators.contains(&b) {
                break;
            }
            self.pos += 1;
        }
        self.bytes[start..self.pos].to_vec()
    }

    /// Reads `n` bytes, advancing the cursor. Errors if fewer than `n` remain.
    fn read_n(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("cursor overflow"))?;
        if end > self.bytes.len() {
            bail!(
                "unexpected end of file at position {}: needed {} bytes, only {} left",
                self.pos,
                n,
                self.remaining()
            );
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }
}

/// Decode a byte sequence with the configured charset into `String`.
fn decode_charset(bytes: &[u8], charset: Charset) -> String {
    match charset {
        Charset::DEFAULT => {
            // NIST files in their default charset are 7-bit ASCII compatible.
            // Replace any non-ASCII byte with '?' so the GUI never crashes.
            bytes
                .iter()
                .map(|&b| if b < 0x80 { b as char } else { '?' })
                .collect()
        }
        Charset::UTF8 => String::from_utf8_lossy(bytes).into_owned(),
        Charset::UTF16 => {
            // UTF-16 LE with optional BOM. If we have an odd number of bytes we
            // simply drop the trailing one — better than crashing.
            let mut slice = bytes;
            if slice.starts_with(&[0xFF, 0xFE]) {
                slice = &slice[2..];
            }
            let mut out = String::with_capacity(slice.len() / 2);
            let mut chunks = slice.chunks_exact(2);
            for chunk in &mut chunks {
                let c = u16::from_le_bytes([chunk[0], chunk[1]]);
                if let Some(ch) = char::from_u32(c as u32) {
                    out.push(ch);
                }
            }
            out
        }
    }
}

/// Parse a `crt.field:value` tag from the cursor.
///
/// `value_term` is the byte that ends the value portion (e.g. `GROUP_SEPARATOR`
/// for sub-fields or `FIELD_SEPARATOR` / `RECORD_SEPARATOR` for the last value
/// in a record).
/// Returns `(record_type, field, value, value_start)`
/// where `value_start` is the byte offset where the value began. Callers
/// need this for the binary `999` field, where reading ahead looking for
/// separators is unreliable (binary data may contain them by chance).
fn read_tag(cursor: &mut Cursor<'_>) -> Result<(u8, u16, Vec<u8>, usize)> {
    let type_bytes = cursor.read_until(&[b'.']);
    if cursor.peek() != Some(b'.') {
        bail!(
            "expected '.' in tag at position {}, found {:?}",
            cursor.pos,
            cursor.peek()
        );
    }
    cursor.pos += 1; // consume the '.'
    let field_bytes = cursor.read_until(&[b':', b'.']);
    let crt: u8 = std::str::from_utf8(&type_bytes)
        .context("invalid UTF-8 in record type tag")?
        .trim()
        .parse()
        .context("record type tag is not a number")?;
    let field: u16 = std::str::from_utf8(&field_bytes)
        .context("invalid UTF-8 in field tag")?
        .trim()
        .parse()
        .context("field tag is not a number")?;
    if cursor.peek() == Some(b':') {
        cursor.pos += 1; // consume the ':'
    }
    // A value may itself contain RS (0x1E) / US (0x1F) separators — for
    // example the Type-1 `CNT` field lists `type<US>count` entries separated
    // by RS. Only GS / FS actually terminate a value inside a record.
    let value_start = cursor.pos;
    let value = cursor.read_until(&[GROUP_SEPARATOR, FIELD_SEPARATOR]);
    Ok((crt, field, value, value_start))
}

/// Parse the Type-1 transaction information record. Returns the parsed record
/// plus the charset selected by its `DCS` field.
fn read_type1(cursor: &mut Cursor<'_>) -> Result<(NistRecord, Charset)> {
    let mut record = NistRecord {
        record_type: 1,
        idc: 0,
        fields: Vec::new(),
        image_data: None,
    };
    let mut charset = Charset::DEFAULT;
    loop {
        let (crt, field, value, _value_start) = read_tag(cursor)
            .with_context(|| format!("Type-1 parsing at position {}", cursor.pos))?;
        if crt != 1 {
            bail!("expected Type-1 tag, got Type-{}", crt);
        }
        let decoded = decode_charset(&value, charset);
        record.fields.push(TextField::new(field, code_for(1, field), decoded));
        if field == 15 {
            // DCS — Directory of Character Sets
            if value.starts_with(b"002") {
                charset = Charset::UTF16;
            } else if value.starts_with(b"003") {
                charset = Charset::UTF8;
            }
        }
        match cursor.peek() {
            Some(FIELD_SEPARATOR) => {
                cursor.pos += 1;
                break;
            }
            Some(GROUP_SEPARATOR) | Some(RECORD_SEPARATOR) => {
                cursor.pos += 1;
                continue;
            }
            Some(b) => bail!("Type-1: expected separator, found 0x{:02X}", b),
            None => bail!("Type-1: unexpected EOF"),
        }
    }
    Ok((record, charset))
}

/// Returns the standard 3-letter field code for a given `(record_type, field)`.
/// Unknown fields get an empty code so they are still preserved as raw text.
fn code_for(record_type: u8, field: u16) -> String {
    match (record_type, field) {
        // Type-1
        (1, 1) => "LEN",
        (1, 2) => "VER",
        (1, 3) => "CNT",
        (1, 4) => "TOT",
        (1, 5) => "DAT",
        (1, 6) => "PRY",
        (1, 7) => "DAI",
        (1, 8) => "ORI",
        (1, 9) => "TCN",
        (1, 10) => "TCR",
        (1, 11) => "NSR",
        (1, 12) => "NTR",
        (1, 13) => "DOM",
        (1, 14) => "GMT",
        (1, 15) => "DCS",
        (1, 16) => "APS",
        (1, 17) => "ANM",
        (1, 18) => "GNS",
        // Type-2
        (2, _) => "UTF",
        // Type-9
        (9, 1) => "LEN",
        (9, 2) => "IDC",
        (9, 3) => "IMP",
        (9, 4) => "FGP",
        (9, 12) => "ISR",
        (9, 13) => "HLL",
        (9, 14) => "VLL",
        (9, 20) => "COM",
        (9, 999) => "DATA",
        // Type-10
        (10, 1) => "LEN",
        (10, 2) => "IDC",
        (10, 3) => "IMT",
        (10, 4) => "SRC",
        (10, 5) => "PHD",
        (10, 6) => "HLL",
        (10, 7) => "VLL",
        (10, 8) => "SLC",
        (10, 9) => "HPS",
        (10, 10) => "VPS",
        (10, 11) => "CGA",
        (10, 12) => "CSP",
        (10, 13) => "SAP",
        (10, 14) => "FIP",
        (10, 20) => "POS",
        (10, 21) => "POA",
        (10, 38) => "COM",
        (10, 39) => "T10",
        (10, 40) => "SMT",
        (10, 999) => "DATA",
        // Type-13/14/15 (variable-resolution images)
        (13..=15, 1) => "LEN",
        (13..=15, 2) => "IDC",
        (13..=15, 3) => "IMP",
        (13..=15, 4) => "SRC",
        (13..=15, 5) => "FCD",
        (13..=15, 6) => "HLL",
        (13..=15, 7) => "VLL",
        (13..=15, 8) => "SLC",
        (13..=15, 9) => "HPS",
        (13..=15, 10) => "VPS",
        (13..=15, 11) => "CGA",
        (13..=15, 12) => "BPX",
        (13..=15, 13) => "FGP",
        (13, 22) => "NQM",
        (13, 999) => "DATA",
        (14, 22) => "NQM",
        (14, 999) => "DATA",
        (15, 999) => "DATA",
        _ => "",
    }
    .to_string()
}

/// Parse a text-style record (everything except Types 3-8). The first tag is
/// always `LEN` whose value gives the total byte length of the record.
fn read_text_record(cursor: &mut Cursor<'_>, record_type: u8, charset: Charset) -> Result<NistRecord> {
    // First tag must be LEN (field 1)
    let record_start = cursor.pos;
    let (crt, field, value, _value_start) = read_tag(cursor)
        .with_context(|| format!("Type-{} at position {}", record_type, cursor.pos))?;
    if crt != record_type {
        bail!("expected Type-{}, got Type-{}", record_type, crt);
    }
    if field != 1 {
        bail!("first field of a text record must be LEN (1), got {}", field);
    }
    let length: usize = std::str::from_utf8(&value)
        .context("LEN field is not valid UTF-8")?
        .trim()
        .parse()
        .context("LEN field is not an integer")?;
    let mut record = NistRecord {
        record_type,
        idc: 0,
        fields: vec![TextField::new(1, "LEN", length.to_string())],
        image_data: None,
    };
    // Now consume fields until we see RS or hit length.
    loop {
        match cursor.peek() {
            Some(RECORD_SEPARATOR) => {
                cursor.pos += 1;
                return Ok(record);
            }
            Some(b) if is_separator(b) => {
                // FS ends the record; GS is followed by another tag.
                if b == FIELD_SEPARATOR {
                    cursor.pos += 1;
                    return Ok(record);
                }
                cursor.pos += 1;
            }
            None => return Ok(record),
            Some(_) => {}
        }
        // Parse next tag. The value slice is only trustworthy for text
        // fields; binary image data may contain separator bytes.
        let (crt, field, value, value_start) = match read_tag(cursor) {
            Ok(t) => t,
            Err(_) => {
                // Truncated at end of record — bail gracefully.
                return Ok(record);
            }
        };
        if crt != record_type {
            bail!(
                "record type changed mid-record at pos {}: expected {}, got {}",
                value_start,
                record_type,
                crt
            );
        }
        // If this is the DATA field (999), grab exactly the bytes the
        // declared record `length` says remain — the image payload extends
        // to the end of the record and may contain separator-like bytes.
        if field == 999 {
            let data_len = length.saturating_sub(value_start - record_start);
            let end = (value_start + data_len).min(cursor.bytes.len());
            let mut data = cursor.bytes[value_start..end].to_vec();
            // Some writers append a record-terminating separator inside the
            // declared length; strip it so the payload starts/ends cleanly.
            while data.last().is_some_and(|b| is_separator(*b)) {
                data.pop();
            }
            cursor.pos = end;
            record.fields.push(TextField::new(999, "DATA", String::new()));
            record.image_data = Some(data);
            return Ok(record);
        } else {
            let decoded = decode_charset(&value, charset);
            if field == 2 {
                // Field 2 is always the IDC for text records.
                if let Ok(idc) = decoded.trim().parse::<u8>() {
                    record.idc = idc;
                }
            }
            record.fields.push(TextField::new(field, code_for(record_type, field), decoded));
        }
    }
}

/// Parse a binary fingerprint/image record (Types 3-8). The first 4 bytes are
/// a big-endian length, followed by a fixed-size header and then `length - 18`
/// bytes of image data. See ANSI/NIST-ITL 1-2011 Table 4.
fn read_binary_record(cursor: &mut Cursor<'_>, record_type: u8) -> Result<NistRecord> {
    let len_bytes = cursor.read_n(4)?;
    let length = u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    // Header is 18 bytes total: LEN(4) IDC(1) IMP(1) FGP(6) ISR(1) HLL(2)
    // VLL(2) GCA(1) — see ANSI/NIST-ITL 1-2011, Table 4.
    if length < 18 {
        bail!(
            "Type-{}: invalid record length {} (must be >= 18)",
            record_type,
            length
        );
    }
    let idc = cursor.read_n(1)?[0];
    let imp = cursor.read_n(1)?[0];
    let _fgp = cursor.read_n(6)?;
    let isr = cursor.read_n(1)?[0];
    let hll_bytes = cursor.read_n(2)?;
    let hll = u16::from_be_bytes([hll_bytes[0], hll_bytes[1]]);
    let vll_bytes = cursor.read_n(2)?;
    let vll = u16::from_be_bytes([vll_bytes[0], vll_bytes[1]]);
    let gca = cursor.read_n(1)?[0];

    let payload_len = length.saturating_sub(18);
    let payload = if cursor.pos + payload_len <= cursor.bytes.len() {
        let data = cursor.bytes[cursor.pos..cursor.pos + payload_len].to_vec();
        cursor.pos += payload_len;
        data
    } else {
        let data = cursor.bytes[cursor.pos..].to_vec();
        cursor.pos = cursor.bytes.len();
        data
    };

    // The 1-byte GCA code (0=NONE, 1=JPEGB, 2=JPEGL) predates WSQ; sniff the
    // payload magic so WSQ/JPEG content is recognized regardless of timing
    // when the encoder was written.
    let compression = sniff_compression(&payload)
        .map(str::to_string)
        .unwrap_or_else(|| match gca {
            0 => "NONE".to_string(),
            1 => "JPEGB".to_string(),
            2 => "JPEGL".to_string(),
            other => format!("UNKNOWN({other})"),
        });

    let record = NistRecord {
        record_type,
        idc,
        fields: vec![
            TextField::new(1, "LEN", length.to_string()),
            TextField::new(2, "IDC", idc.to_string()),
            TextField::new(3, "IMP", imp.to_string()),
            TextField::new(5, "ISR", isr.to_string()),
            TextField::new(6, "HLL", hll.to_string()),
            TextField::new(7, "VLL", vll.to_string()),
            TextField::new(8, "GCA", compression),
        ],
        image_data: Some(payload),
    };
    Ok(record)
}

/// Read the next record from the buffer, given the current cursor.
///
/// `expected_type` is `Some(t)` after a Type-1 has been seen and we know the
/// next record's type from the `CNT` field. `None` means "discover it from the
/// file" — the first byte of a record is its tag (the record type number).
fn read_next_record(
    cursor: &mut Cursor<'_>,
    charset: Charset,
    expected_type: Option<u8>,
) -> Result<Option<NistRecord>> {
    // Skip any leftover whitespace/separators between records.
    while matches!(cursor.peek(), Some(b) if b == RECORD_SEPARATOR || b == FIELD_SEPARATOR) {
        cursor.pos += 1;
    }
    if cursor.remaining() == 0 {
        return Ok(None);
    }
    // Discover the record type from the first tag (read the digits before the '.').
    let start_pos = cursor.pos;
    let mut type_buf = Vec::new();
    while let Some(b) = cursor.peek() {
        if b.is_ascii_digit() {
            type_buf.push(b);
            cursor.pos += 1;
        } else {
            break;
        }
    }
    let discovered_type: u8 = if type_buf.is_empty() {
        // No digits? Bail out gracefully.
        return Ok(None);
    } else {
        let s = std::str::from_utf8(&type_buf).unwrap_or("0");
        s.parse().unwrap_or(0)
    };
    let actual_type = expected_type.unwrap_or(discovered_type);
    cursor.pos = start_pos;

    if (3..=8).contains(&actual_type) {
        let rec = read_binary_record(cursor, actual_type)?;
        Ok(Some(rec))
    } else {
        let rec = read_text_record(cursor, actual_type, charset)?;
        Ok(Some(rec))
    }
}

/// Decode an entire NIST file from a byte buffer.
pub fn decode(bytes: &[u8]) -> Result<NistFile> {
    let mut cursor = Cursor::new(bytes);
    let (transaction, charset) = read_type1(&mut cursor)
        .context("failed to parse Type-1 transaction information record")?;
    // The CNT field (1.003) lists `type<US>count` entries separated by RS.
    // We use it only as a fallback for binary records (Types 3-8) whose tags
    // cannot be discovered by scanning, because they begin with a binary
    // 4-byte length instead of an ASCII tag.
    let mut binary_queue: Vec<u8> = transaction
        .field_value("CNT")
        .map(|cnt| parse_cnt_types(cnt))
        .unwrap_or_default()
        .into_iter()
        .filter(|t| (3..=8).contains(t))
        .collect();

    let mut records = Vec::new();
    loop {
        let start_pos = cursor.pos;
        match read_next_record(&mut cursor, charset, None) {
            Ok(Some(rec)) => {
                let rt = rec.record_type;
                records.push(rec);
                if let Some(idx) = binary_queue.iter().position(|t| *t == rt) {
                    binary_queue.remove(idx);
                }
            }
            Ok(None) => {
                // No ASCII tag found — maybe a binary record with no tag.
                cursor.pos = start_pos;
                if cursor.remaining() > 0 && !binary_queue.is_empty() {
                    let binary_type = binary_queue[0];
                    match read_binary_record(&mut cursor, binary_type) {
                        Ok(rec) => {
                            records.push(rec);
                            binary_queue.remove(0);
                            continue;
                        }
                        Err(_) => break,
                    }
                }
                break;
            }
            Err(e) => {
                // Tolerate parse errors on later records so the viewer still
                // shows what it could read.
                eprintln!(
                    "warning: stopping early at byte {}: {}",
                    start_pos, e
                );
                break;
            }
        }
    }

    Ok(NistFile {
        transaction,
        records,
        bytes_read: cursor.pos,
    })
}

/// Sniffs the image payload's magic bytes to determine the compression.
/// Returns e.g. `"WSQ20"`, `"JPEG"`, `"PNG"`, or `None` when unrecognized.
pub fn sniff_compression(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0xFF, 0xA0]) {
        Some("WSQ20") // WSQ Start-Of-Image marker
    } else if data.starts_with(&[0xFF, 0xD8]) {
        Some("JPEG") // JPEG Start-Of-Image marker
    } else if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("PNG")
    } else {
        None
    }
}

/// Parses the Type-1 `CNT` field into the expanded list of record types.
/// Format: `type1<US>count1<RS>type2<US>count2...`.
fn parse_cnt_types(cnt: &str) -> Vec<u8> {
    let mut types = Vec::new();
    for entry in cnt.split('\u{1E}') {
        let mut parts = entry.split('\u{1F}');
        let Some(t) = parts.next().and_then(|p| p.trim().parse::<u8>().ok()) else {
            continue;
        };
        let count: usize = parts
            .next()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(1);
        for _ in 0..count.min(255) {
            types.push(t);
        }
    }
    types
}
