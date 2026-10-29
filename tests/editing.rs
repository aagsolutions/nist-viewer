//! Editing must preserve image bytes and produce self-consistent records.

use nist_viewer::nist::{NistFile, NistRecord, parser::decode};

fn set(record: &mut NistRecord, field: u16, value: &str) {
    record
        .fields
        .iter_mut()
        .find(|f| f.field == field)
        .unwrap()
        .value = value.into();
}

fn tagged(record_type: u8, fields: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (number, value) in fields {
        body.extend(format!("\x1d{record_type}.{number:03}:").bytes());
        body.extend(value);
    }
    body.push(0x1c);
    let mut length = 0;
    loop {
        let header = format!("{record_type}.001:{length}");
        let next = header.len() + body.len();
        if next == length {
            let mut bytes = header.into_bytes();
            bytes.extend(body);
            return bytes;
        }
        length = next;
    }
}

fn transaction(records: &[(u8, u8, Vec<u8>)], dcs: Option<&str>) -> Vec<u8> {
    let mut cnt = format!("1\x1f{}", records.len());
    for (kind, idc, _) in records {
        cnt.push_str(&format!("\x1e{kind}\x1f{idc:02}"));
    }
    let mut fields = vec![
        (2, b"0400".to_vec()),
        (3, cnt.into_bytes()),
        (9, b"transaction".to_vec()),
    ];
    if let Some(dcs) = dcs {
        fields.push((15, dcs.as_bytes().to_vec()));
    }
    let mut bytes = tagged(1, &fields);
    for (_, _, record) in records {
        bytes.extend(record);
    }
    bytes
}

fn check_lengths(file: &NistFile, bytes: &[u8]) {
    let mut offset = 0;
    for record in file.all_records() {
        let length: usize = record
            .fields
            .iter()
            .find(|f| f.field == 1)
            .unwrap()
            .value
            .parse()
            .unwrap();
        if (3..=8).contains(&record.record_type) {
            assert_eq!(
                length,
                u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
            );
        } else {
            assert!(
                bytes[offset..]
                    .starts_with(format!("{}.001:{length}", record.record_type).as_bytes())
            );
            assert_eq!(bytes[offset + length - 1], 0x1c);
        }
        offset += length;
    }
    assert_eq!(offset, bytes.len());
    assert_eq!(file.bytes_read, bytes.len());
    assert!(file.warnings.is_empty(), "{:?}", file.warnings);
}

#[test]
fn bundled_files_round_trip_without_changing_a_byte() {
    for name in [
        "type-3.an2",
        "type-9-4-iafis.an2",
        "type-8-sig.an2",
        "type-10-tattoo-face-sap20.an2",
        "type-14-amp-nqm-utf8.an2",
    ] {
        let original = std::fs::read(format!("test_data/{name}")).unwrap();
        let mut file = decode(&original).unwrap();
        assert!(file.warnings.is_empty(), "{name}: {:?}", file.warnings);
        let saved = file.to_bytes().unwrap();
        assert_eq!(
            saved, original,
            "{name}: unchanged files must round trip exactly"
        );
        let reopened = decode(&saved).unwrap();
        check_lengths(&reopened, &saved);
    }
}

#[test]
fn editing_utf8_and_idc_recalculates_byte_lengths_and_cnt() {
    let original = std::fs::read("test_data/type-14-amp-nqm-utf8.an2").unwrap();
    let mut file = decode(&original).unwrap();
    let images: Vec<_> = file.all_records().map(|r| r.image_data.clone()).collect();
    let text = "Élodie — 東京 🖐\x1esecond subfield\x1fsecond item".repeat(50);
    let field = file.records[0]
        .fields
        .iter()
        .find(|f| f.field > 2)
        .unwrap()
        .field;
    set(&mut file.records[0], field, &text);
    set(&mut file.records[1], 2, "07");
    file.recalculate().unwrap();
    let saved = file.to_bytes().unwrap();
    let reopened = decode(&saved).unwrap();
    check_lengths(&reopened, &saved);
    assert_eq!(
        reopened.records[0]
            .fields
            .iter()
            .find(|f| f.field == field)
            .unwrap()
            .value,
        text
    );
    assert_eq!(reopened.records[1].idc, 7);
    assert_eq!(
        reopened.transaction.field_value("CNT"),
        Some("1\x1f4\x1e2\x1f00\x1e14\x1f07\x1e14\x1f02\x1e14\x1f03")
    );
    assert_eq!(
        reopened
            .all_records()
            .map(|r| r.image_data.clone())
            .collect::<Vec<_>>(),
        images
    );
}

#[test]
fn tagged_image_payload_may_end_with_any_separator_bytes() {
    let payload = vec![0xff, 0xd8, 0, 0x1c, 0x1d, 0x1e, 0x1f, 0x1c];
    let record = tagged(14, &[(2, b"00".to_vec()), (999, payload.clone())]);
    let bytes = transaction(&[(14, 0, record)], None);
    let mut file = decode(&bytes).unwrap();
    assert_eq!(file.records[0].image_data.as_ref().unwrap(), &payload);
    assert_eq!(file.to_bytes().unwrap(), bytes);
}

#[test]
fn binary_directory_uses_each_idc_as_an_identifier_including_zero() {
    let mut records = Vec::new();
    for (kind, idc) in [(4, 0), (3, 0), (6, 5), (8, 9), (7, 1)] {
        let header_size = match kind {
            7 => 5,
            8 => 12,
            _ => 18,
        };
        let mut bytes = vec![0; header_size];
        bytes[..4].copy_from_slice(&((header_size + 3) as u32).to_be_bytes());
        bytes[4] = idc;
        if header_size == 18 {
            bytes[6..12].copy_from_slice(&[1, 2, 3, 4, 5, 255]);
            bytes[17] = 42;
        }
        bytes.extend([0x1c, 0x1d, 0x1e]);
        records.push((kind, idc, bytes));
    }
    let original = transaction(&records, None);
    let mut file = decode(&original).unwrap();
    assert_eq!(file.records.len(), records.len());
    assert_eq!(file.to_bytes().unwrap(), original);
    set(&mut file.records[0], 2, "7");
    set(&mut file.records[0], 6, "512");
    let saved = file.to_bytes().unwrap();
    let reopened = decode(&saved).unwrap();
    check_lengths(&reopened, &saved);
    assert_eq!(reopened.records[0].idc, 7);
    assert_eq!(reopened.records[0].field_value("HLL"), Some("512"));
    let start = reopened
        .transaction
        .field_value("LEN")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert_eq!(&saved[start + 6..start + 12], &[1, 2, 3, 4, 5, 255]);
    assert_eq!(saved[start + 17], 42);
}

#[test]
fn invalid_edits_fail_without_partially_updating_calculated_fields() {
    let original = std::fs::read("test_data/type-3.an2").unwrap();
    let mut file = decode(&original).unwrap();
    let old_len = file.transaction.field_value("LEN").unwrap().to_string();
    let old_cnt = file.transaction.field_value("CNT").unwrap().to_string();
    set(&mut file.records[1], 2, "999");
    assert!(file.recalculate().is_err());
    assert_eq!(file.transaction.field_value("LEN"), Some(old_len.as_str()));
    assert_eq!(file.transaction.field_value("CNT"), Some(old_cnt.as_str()));
    set(&mut file.records[1], 2, "1");
    set(&mut file.transaction, 9, "bad\x1dvalue");
    assert!(file.to_bytes().is_err());
}

#[test]
fn incomplete_files_are_viewable_but_cannot_be_saved() {
    let mut original = std::fs::read("test_data/type-3.an2").unwrap();
    original.truncate(original.len() - 10);
    let mut file = decode(&original).unwrap();
    assert!(!file.warnings.is_empty());
    assert!(file.to_bytes().is_err());
}

#[test]
fn unchanged_invalid_utf8_is_preserved_when_another_field_is_edited() {
    let record = tagged(
        2,
        &[
            (2, b"00".to_vec()),
            (3, vec![0xff, 0xfe]),
            (4, b"old".to_vec()),
        ],
    );
    let original = transaction(&[(2, 0, record)], Some("003\x1fUTF-8"));
    let mut file = decode(&original).unwrap();
    set(&mut file.records[0], 4, "new");
    let saved = file.to_bytes().unwrap();
    assert!(saved.windows(2).any(|w| w == [0xff, 0xfe]));
    check_lengths(&decode(&saved).unwrap(), &saved);
}

#[test]
fn utf16_edits_preserve_code_units_surrogates_and_ascii_transaction_fields() {
    let original_text = "Name ĝĜ😀";
    let original_value = original_text
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let record = tagged(2, &[(2, b"00".to_vec()), (3, original_value)]);
    let original = transaction(&[(2, 0, record)], Some("002\x1fUTF-16"));
    let mut file = decode(&original).unwrap();
    assert!(file.warnings.is_empty(), "{:?}", file.warnings);
    assert_eq!(file.records[0].fields[2].value, original_text);
    assert_eq!(file.to_bytes().unwrap(), original);
    let edited = "Updated Ĝĝ 東京 😀";
    set(&mut file.records[0], 3, edited);
    let saved = file.to_bytes().unwrap();
    let reopened = decode(&saved).unwrap();
    check_lengths(&reopened, &saved);
    assert_eq!(reopened.records[0].fields[2].value, edited);
    assert_eq!(reopened.transaction.field_value("TCN"), Some("transaction"));
}

#[test]
fn latin1_text_round_trips_and_unrepresentable_edits_are_rejected() {
    let record = tagged(2, &[(2, b"00".to_vec()), (3, vec![b'R', 0xe9, b'm', b'y'])]);
    let original = transaction(&[(2, 0, record)], None);
    let mut file = decode(&original).unwrap();
    assert_eq!(file.records[0].fields[2].value, "Rémy");
    assert_eq!(file.to_bytes().unwrap(), original);
    set(&mut file.records[0], 3, "André");
    let saved = file.to_bytes().unwrap();
    assert_eq!(decode(&saved).unwrap().records[0].fields[2].value, "André");
    set(&mut file.records[0], 3, "東京");
    assert!(file.to_bytes().is_err());
}
