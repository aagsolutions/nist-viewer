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

use anyhow::{Context, Result, anyhow, bail};

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
    /// Problems recovered from while viewing. Saving is disabled to avoid data loss.
    pub warnings: Vec<String>,
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
pub(super) enum Charset {
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
pub(super) fn decode_charset(bytes: &[u8], charset: Charset) -> String {
    match charset {
        Charset::DEFAULT => bytes.iter().map(|&b| char::from(b)).collect(),
        Charset::UTF8 => String::from_utf8_lossy(bytes).into_owned(),
        Charset::UTF16 => {
            // UTF-16 LE with optional BOM. If we have an odd number of bytes we
            // simply drop the trailing one — better than crashing.
            let mut slice = bytes;
            if slice.starts_with(&[0xFF, 0xFE]) {
                slice = &slice[2..];
            }
            let units: Vec<u16> = slice
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
    }
}

/// Parse a `crt.field:value` tag from the cursor.
///
/// Returns `(record_type, field, value, value_start)`
/// where `value_start` is the byte offset where the value began. Callers
/// need this for the binary `999` field, where reading ahead looking for
/// separators is unreliable (binary data may contain them by chance).
fn read_tag(cursor: &mut Cursor<'_>, charset: Charset) -> Result<(u8, u16, Vec<u8>, usize)> {
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
    if cursor.peek() != Some(b':') {
        bail!("expected ':' in field tag at byte {}", cursor.pos);
    }
    cursor.pos += 1;
    // A value may itself contain RS (0x1E) / US (0x1F) separators — for
    // example the Type-1 `CNT` field lists `type<US>count` entries separated
    // by RS. Only GS / FS actually terminate a value inside a record.
    let value_start = cursor.pos;
    let value = if field == 999 {
        // The caller extracts binary data by LEN, never by scanning its bytes.
        Vec::new()
    } else if matches!(charset, Charset::UTF16) && crt != 1 && field > 2 {
        let next_tag = format!("{crt}.");
        while cursor.pos < cursor.bytes.len() {
            let rest = &cursor.bytes[cursor.pos..];
            if rest == [FIELD_SEPARATOR]
                || (rest[0] == GROUP_SEPARATOR && rest[1..].starts_with(next_tag.as_bytes()))
            {
                break;
            }
            // Advance by code units so separator-valued bytes within a UTF-16
            // character cannot split a field.
            cursor.read_n(2)?;
        }
        cursor.bytes[value_start..cursor.pos].to_vec()
    } else {
        cursor.read_until(&[GROUP_SEPARATOR, FIELD_SEPARATOR])
    };
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
        binary_header: None,
    };
    let mut charset = Charset::DEFAULT;
    loop {
        let (crt, field, value, _value_start) = read_tag(cursor, Charset::DEFAULT)
            .with_context(|| format!("Type-1 parsing at position {}", cursor.pos))?;
        if crt != 1 {
            bail!("expected Type-1 tag, got Type-{}", crt);
        }
        // Type-1 values remain ASCII, including the directory declaring the
        // character set used in subsequent records.
        let decoded = decode_charset(&value, Charset::DEFAULT);
        if field == 15 {
            // DCS — Directory of Character Sets
            if value.starts_with(b"002") {
                charset = Charset::UTF16;
            } else if value.starts_with(b"003") {
                charset = Charset::UTF8;
            }
        }
        record
            .fields
            .push(TextField::parsed(field, code_for(1, field), decoded, value));
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
        (2, 1) => "LEN",
        (2, 2) => "IDC",
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
fn read_text_record(
    cursor: &mut Cursor<'_>,
    record_type: u8,
    charset: Charset,
) -> Result<NistRecord> {
    let record_start = cursor.pos;
    let (crt, field, value, _) = read_tag(cursor, Charset::DEFAULT)?;
    if crt != record_type || field != 1 {
        bail!("expected Type-{record_type} LEN as the first field");
    }
    let length: usize = std::str::from_utf8(&value)?
        .parse()
        .context("LEN field is not an integer")?;
    let record_end = record_start
        .checked_add(length)
        .filter(|&end| end <= cursor.bytes.len() && end > cursor.pos)
        .ok_or_else(|| {
            anyhow!("Type-{record_type}: invalid or truncated record length {length}")
        })?;
    if cursor.bytes[record_end - 1] != FIELD_SEPARATOR {
        bail!("Type-{record_type}: record does not end with FS at its declared length");
    }
    let mut local = Cursor {
        bytes: &cursor.bytes[record_start..record_end],
        pos: cursor.pos - record_start,
    };
    let mut record = NistRecord {
        record_type,
        idc: 0,
        fields: vec![TextField::new(1, "LEN", length.to_string())],
        image_data: None,
        binary_header: None,
    };
    loop {
        match local.peek() {
            Some(FIELD_SEPARATOR) if local.pos == length - 1 => break,
            Some(GROUP_SEPARATOR) => local.pos += 1,
            _ => bail!(
                "Type-{record_type}: expected field separator at byte {}",
                local.pos
            ),
        }
        let (crt, field, value, value_start) = read_tag(&mut local, charset)?;
        if crt != record_type {
            bail!("record type changed mid-record: expected {record_type}, got {crt}");
        }
        if field == 999 {
            // Only the final FS belongs to the framing. Every preceding byte,
            // including any separator-like bytes, belongs to the image.
            record.image_data = Some(local.bytes[value_start..length - 1].to_vec());
            record.fields.push(TextField::new(999, "DATA", ""));
            break;
        }
        let decoded = if field == 2 {
            String::from_utf8_lossy(&value).into_owned()
        } else {
            decode_charset(&value, charset)
        };
        if field == 2 {
            record.idc = decoded
                .trim()
                .parse()
                .context("IDC is not an unsigned byte")?;
        }
        record.fields.push(TextField::parsed(
            field,
            code_for(record_type, field),
            decoded,
            value,
        ));
    }
    cursor.pos = record_end;
    Ok(record)
}

/// Parse a binary fingerprint/image record (Types 3-8). The first 4 bytes are
/// a big-endian length, followed by a fixed-size header and then `length - 18`
/// bytes of image data. See ANSI/NIST-ITL 1-2011 Table 4.
fn read_binary_record(cursor: &mut Cursor<'_>, record_type: u8) -> Result<NistRecord> {
    if record_type == 8 {
        return read_signature_record(cursor);
    }
    let start = cursor.pos;
    let len_bytes = cursor.read_n(4)?;
    let length =
        u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    if record_type == 7 {
        if length < 5 {
            bail!("Type-7: record length must be at least 5 bytes");
        }
        let idc = cursor.read_n(1)?[0];
        let header = cursor.bytes[start..cursor.pos].to_vec();
        let payload = cursor.read_n(length - 5)?.to_vec();
        return Ok(NistRecord {
            record_type,
            idc,
            fields: vec![
                TextField::new(1, "LEN", length.to_string()),
                TextField::new(2, "IDC", idc.to_string()),
            ],
            image_data: Some(payload),
            binary_header: Some(header),
        });
    }
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
    let fgp = cursor.read_n(6)?.to_vec();
    let isr = cursor.read_n(1)?[0];
    let hll_bytes = cursor.read_n(2)?;
    let hll = u16::from_be_bytes([hll_bytes[0], hll_bytes[1]]);
    let vll_bytes = cursor.read_n(2)?;
    let vll = u16::from_be_bytes([vll_bytes[0], vll_bytes[1]]);
    let gca = cursor.read_n(1)?[0];

    let header = cursor.bytes[start..cursor.pos].to_vec();
    let payload = cursor.read_n(length - 18)?.to_vec();

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
            TextField::new(
                4,
                "FGP",
                fgp.iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join("\u{1e}"),
            ),
            TextField::new(5, "ISR", isr.to_string()),
            TextField::new(6, "HLL", hll.to_string()),
            TextField::new(7, "VLL", vll.to_string()),
            TextField::new(8, "GCA", compression),
        ],
        image_data: Some(payload),
        binary_header: Some(header),
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
    if let Some(record_type @ 3..=8) = expected_type {
        return read_binary_record(cursor, record_type).map(Some);
    }
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
    let (transaction, charset) =
        read_type1(&mut cursor).context("failed to parse Type-1 transaction information record")?;
    let mut warnings = Vec::new();
    if transaction
        .field_value("LEN")
        .and_then(|s| s.parse::<usize>().ok())
        != Some(cursor.pos)
    {
        warnings.push("Type-1 length does not match the record bytes".into());
    }
    let types = transaction
        .field_value("CNT")
        .map(parse_cnt_types)
        .unwrap_or_default();
    let mut records = Vec::new();
    while cursor.remaining() > 0 {
        let start = cursor.pos;
        match read_next_record(&mut cursor, charset, types.get(records.len()).copied()) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {
                cursor.pos = start;
                break;
            }
            Err(error) => {
                warnings.push(format!("Stopped at byte {start}: {error:#}"));
                cursor.pos = start;
                break;
            }
        }
    }
    if cursor.pos != bytes.len() {
        warnings.push(format!(
            "{} input bytes could not be parsed",
            bytes.len() - cursor.pos
        ));
    }
    let declared_count = transaction
        .field_value("CNT")
        .and_then(|cnt| cnt.split('\u{1e}').next())
        .and_then(|entry| entry.split('\u{1f}').nth(1))
        .and_then(|count| count.parse::<usize>().ok());
    if declared_count != Some(records.len()) || types.len() != records.len() {
        warnings.push("CNT does not match the number of parsed records".into());
    }
    Ok(NistFile {
        transaction,
        records,
        bytes_read: cursor.pos,
        warnings,
    })
}

/// Parse a Type-8 signature record. Its binary header differs from the
/// Type-3..6 fingerprint layout: LEN(4) IDC(1) SIG(1) SRT(1) ISR(1) HLL(2)
/// VLL(2), followed by `LEN - 12` payload bytes. Uncompressed signatures
/// (ISR = 1) store one **bit** per pixel, MSB-first per byte.
fn read_signature_record(cursor: &mut Cursor<'_>) -> Result<NistRecord> {
    const HEADER_SIZE: usize = 12;
    let start = cursor.pos;
    let len_bytes = cursor.read_n(4)?;
    let length =
        u32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]) as usize;
    if length < HEADER_SIZE {
        bail!("Type-8: invalid record length {length}");
    }
    let idc = cursor.read_n(1)?[0];
    let sig = cursor.read_n(1)?[0];
    let srt = cursor.read_n(1)?[0];
    let isr = cursor.read_n(1)?[0];
    let hll_bytes = cursor.read_n(2)?;
    let hll = u16::from_be_bytes([hll_bytes[0], hll_bytes[1]]);
    let vll_bytes = cursor.read_n(2)?;
    let vll = u16::from_be_bytes([vll_bytes[0], vll_bytes[1]]);

    let header = cursor.bytes[start..cursor.pos].to_vec();
    let payload = cursor.read_n(length - HEADER_SIZE)?.to_vec();

    let compression = sniff_compression(&payload)
        .map(str::to_string)
        .unwrap_or_else(|| "NONE".into());

    let record = NistRecord {
        record_type: 8,
        idc,
        fields: vec![
            TextField::new(1, "LEN", length.to_string()),
            TextField::new(2, "IDC", idc.to_string()),
            TextField::new(3, "SIG", sig.to_string()),
            TextField::new(4, "SRT", srt.to_string()),
            TextField::new(5, "ISR", isr.to_string()),
            TextField::new(6, "HLL", hll.to_string()),
            TextField::new(7, "VLL", vll.to_string()),
            TextField::new(8, "GCA", compression),
        ],
        image_data: Some(payload),
        binary_header: Some(header),
    };
    Ok(record)
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

/// Each CNT subfield after the first describes one record as type<US>IDC.
fn parse_cnt_types(cnt: &str) -> Vec<u8> {
    cnt.split('\u{1e}')
        .skip(1)
        .filter_map(|entry| entry.split('\u{1f}').next()?.trim().parse().ok())
        .collect()
}
