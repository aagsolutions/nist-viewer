//! Serialization and recalculation of ANSI/NIST-ITL traditional files.

use anyhow::{Context, Result, anyhow, bail, ensure};

use super::format::{FIELD_SEPARATOR, GROUP_SEPARATOR};
use super::parser::{Charset, NistFile, decode_charset};
use super::records::{NistRecord, TextField};

impl NistFile {
    /// Recalculate every LEN, the Type-1 CNT directory, and record IDC values.
    /// Leaves the file unchanged if any value cannot be serialized.
    pub fn recalculate(&mut self) -> Result<()> {
        self.to_bytes().map(drop)
    }

    /// Serialize the file, updating calculated fields only after validation succeeds.
    /// Binary image payloads and uninterpreted header bytes are preserved.
    pub fn to_bytes(&mut self) -> Result<Vec<u8>> {
        ensure!(
            self.warnings.is_empty(),
            "Cannot save an incompletely parsed file: {}",
            self.warnings.join("; ")
        );
        let mut candidate = self.clone();
        let bytes = serialize(&mut candidate)?;
        candidate.bytes_read = bytes.len();
        *self = candidate;
        Ok(bytes)
    }
}

pub(super) fn set_field(record: &mut NistRecord, number: u16, code: &str, value: String) {
    if let Some(field) = record.fields.iter_mut().find(|f| f.field == number) {
        field.value = value;
        field.code = code.into();
    } else {
        let index = record
            .fields
            .iter()
            .position(|f| f.field > number)
            .unwrap_or(record.fields.len());
        record
            .fields
            .insert(index, TextField::new(number, code, value));
    }
}

fn serialize(file: &mut NistFile) -> Result<Vec<u8>> {
    ensure!(
        file.transaction.record_type == 1,
        "The first record must be Type-1"
    );
    for record in &mut file.records {
        ensure!(record.record_type != 1, "Only one Type-1 record is allowed");
        record.idc = numeric_field(record, 2)?;
    }
    let mut cnt = format!("1\u{1f}{}", file.records.len());
    for record in &file.records {
        cnt.push_str(&format!(
            "\u{1e}{}\u{1f}{:02}",
            record.record_type, record.idc
        ));
    }
    set_field(&mut file.transaction, 3, "CNT", cnt);
    let charset = match file.transaction.field_value("DCS") {
        Some(value) if value.starts_with("003") => Charset::UTF8,
        Some(value) if value.starts_with("002") => Charset::UTF16,
        Some(value) if !value.starts_with("000") => bail!("Unsupported character set: {value}"),
        _ => Charset::DEFAULT,
    };
    let mut bytes = tagged_record(&mut file.transaction, Charset::DEFAULT)?;
    for record in &mut file.records {
        let encoded = if (3..=8).contains(&record.record_type) {
            binary_record(record)
        } else {
            tagged_record(record, charset)
        }
        .with_context(|| format!("Type-{} IDC {}", record.record_type, record.idc))?;
        bytes.extend(encoded);
    }
    Ok(bytes)
}

fn encode_value(field: &TextField, charset: Charset) -> Result<Vec<u8>> {
    ensure!(
        !field.value.contains(['\u{1c}', '\u{1d}']),
        "Field {} contains a reserved FS or GS separator",
        field.field
    );
    if let Some((original, raw)) = &field.original_value {
        if original == &field.value && decode_charset(raw, charset) == field.value {
            return Ok(raw.clone());
        }
    }
    match charset {
        Charset::DEFAULT => field
            .value
            .chars()
            .map(|ch| {
                u8::try_from(ch as u32).map_err(|_| {
                    anyhow!(
                        "Field {} contains characters outside Latin-1; use a UTF-8 file",
                        field.field
                    )
                })
            })
            .collect(),
        Charset::UTF8 => Ok(field.value.as_bytes().to_vec()),
        Charset::UTF16 => Ok(field
            .value
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect()),
    }
}

fn tagged_record(record: &mut NistRecord, charset: Charset) -> Result<Vec<u8>> {
    set_field(record, 1, "LEN", "0".into());
    ensure!(
        record.fields.first().is_some_and(|f| f.field == 1),
        "LEN must be the first field"
    );
    let mut seen = std::collections::HashSet::new();
    let mut body = Vec::new();
    for field in record.fields.iter().skip(1) {
        ensure!(
            (2..=999).contains(&field.field) && seen.insert(field.field),
            "Invalid or duplicate field {}",
            field.field
        );
        if field.field == 999 {
            ensure!(
                record.image_data.is_some(),
                "DATA field has no binary payload"
            );
            continue;
        }
        if record.record_type == 1 {
            ensure!(
                field.value.is_ascii(),
                "Type-1 field {} must contain ASCII characters",
                field.field
            );
        }
        body.push(GROUP_SEPARATOR);
        body.extend(format!("{}.{:03}:", record.record_type, field.field).bytes());
        // IDC is ASCII even when descriptive text uses another character set.
        body.extend(encode_value(
            field,
            if field.field == 2 {
                Charset::DEFAULT
            } else {
                charset
            },
        )?);
    }
    if let Some(image) = &record.image_data {
        body.push(GROUP_SEPARATOR);
        body.extend(format!("{}.999:", record.record_type).bytes());
        body.extend(image);
    }
    body.push(FIELD_SEPARATOR);
    let prefix = format!("{}.001:", record.record_type);
    let mut length = prefix.len() + 1 + body.len();
    loop {
        let next = prefix.len() + length.to_string().len() + body.len();
        if next == length {
            break;
        }
        length = next;
    }
    set_field(record, 1, "LEN", length.to_string());
    let mut bytes = format!("{prefix}{length}").into_bytes();
    bytes.extend(body);
    Ok(bytes)
}

fn numeric_field<T: std::str::FromStr>(record: &NistRecord, number: u16) -> Result<T> {
    record
        .fields
        .iter()
        .find(|f| f.field == number)
        .ok_or_else(|| anyhow!("Missing field {number}"))?
        .value
        .trim()
        .parse()
        .map_err(|_| {
            anyhow!("Field {number} must be a valid unsigned integer within the header's range")
        })
}

fn binary_record(record: &mut NistRecord) -> Result<Vec<u8>> {
    let mut header = record
        .binary_header
        .clone()
        .context("Missing original binary header")?;
    let size = match record.record_type {
        7 => 5,
        8 => 12,
        _ => 18,
    };
    ensure!(header.len() == size, "Invalid binary header size");
    let payload = record
        .image_data
        .as_ref()
        .context("Missing binary payload")?;
    let length = u32::try_from(size + payload.len()).context("Binary record is too large")?;
    header[..4].copy_from_slice(&length.to_be_bytes());
    header[4] = record.idc;
    if record.record_type == 8 {
        header[5] = numeric_field(record, 3)?;
        header[6] = numeric_field(record, 4)?;
        header[7] = numeric_field(record, 5)?;
        header[8..10].copy_from_slice(&numeric_field::<u16>(record, 6)?.to_be_bytes());
        header[10..12].copy_from_slice(&numeric_field::<u16>(record, 7)?.to_be_bytes());
    } else if record.record_type != 7 {
        header[5] = numeric_field(record, 3)?;
        let fgp = record
            .fields
            .iter()
            .find(|f| f.field == 4)
            .context("Missing FGP field")?;
        let positions = fgp
            .value
            .split('\u{1e}')
            .map(|s| s.trim().parse::<u8>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("FGP must contain six unsigned bytes separated by RS")?;
        ensure!(
            positions.len() == 6,
            "FGP must contain exactly six positions"
        );
        header[6..12].copy_from_slice(&positions);
        header[12] = numeric_field(record, 5)?;
        header[13..15].copy_from_slice(&numeric_field::<u16>(record, 6)?.to_be_bytes());
        header[15..17].copy_from_slice(&numeric_field::<u16>(record, 7)?.to_be_bytes());
        // Preserve the GCA byte, which image replacement updates explicitly.
    }
    let mut bytes = header.clone();
    bytes.extend(payload);
    record.binary_header = Some(header);
    set_field(record, 1, "LEN", length.to_string());
    Ok(bytes)
}
