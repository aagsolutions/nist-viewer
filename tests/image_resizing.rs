//! Resizing changes embedded pixels and remains valid after saving and reopening.

use std::io::Cursor;

use image::{DynamicImage, GrayImage, ImageFormat, Luma, Rgb, RgbImage};
use nist_viewer::nist::{NistFile, parser::decode};

fn fixture(name: &str) -> NistFile {
    decode(&std::fs::read(format!("test_data/{name}")).unwrap()).unwrap()
}

fn image_index(file: &NistFile) -> usize {
    file.all_records()
        .position(|record| record.can_replace_image())
        .unwrap()
}

fn png(pixels: impl Into<DynamicImage>) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    pixels
        .into()
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn reopen(file: &mut NistFile) -> NistFile {
    let bytes = file.to_bytes().unwrap();
    let reopened = decode(&bytes).unwrap();
    assert!(reopened.warnings.is_empty(), "{:?}", reopened.warnings);
    assert_eq!(reopened.bytes_read, bytes.len());
    assert_eq!(
        reopened
            .all_records()
            .map(|record| record.field_value("LEN").unwrap().parse::<usize>().unwrap())
            .sum::<usize>(),
        bytes.len()
    );
    reopened
}

#[test]
fn bundled_images_resize_and_reopen_with_correct_pixels_metadata_and_lengths() {
    for name in [
        "type-3.an2",
        "type-9-4-iafis.an2",
        "type-8-sig.an2",
        "type-10-tattoo-face-sap20.an2",
        "type-14-amp-nqm-utf8.an2",
    ] {
        let mut file = fixture(name);
        let index = image_index(&file);
        let original_images: Vec<_> = file
            .all_records()
            .map(|record| record.image_data.clone())
            .collect();
        let original_cnt = file.transaction.field_value("CNT").unwrap().to_string();
        file.resize_image(index, 13, 7).unwrap();
        let saved = reopen(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        let pixels = record.decoded_image().unwrap();
        assert_eq!((pixels.width(), pixels.height()), (13, 7), "{name}");
        assert_eq!(record.field_value("HLL"), Some("13"));
        assert_eq!(record.field_value("VLL"), Some("7"));
        assert_eq!(
            record.compression_algorithm(),
            Some(if record.record_type <= 8 {
                "NONE"
            } else {
                "PNG"
            })
        );
        if record.record_type == 8 {
            assert_eq!(record.field_value("SRT"), Some("0"));
            assert_eq!(record.image_data.as_ref().unwrap().len(), 2 * 7);
        }
        if matches!(record.record_type, 3 | 4) {
            assert_eq!(record.image_data.as_ref().unwrap().len(), 13 * 7);
        }
        if (13..=17).contains(&record.record_type) {
            assert_eq!(record.field_value("BPX"), Some("8"));
        }
        assert_eq!(
            saved.transaction.field_value("CNT"),
            Some(original_cnt.as_str())
        );
        for (i, record) in saved.all_records().enumerate() {
            if i != index {
                assert_eq!(
                    record.image_data, original_images[i],
                    "{name}: another image changed"
                );
            }
        }
    }
}

#[test]
fn upscaling_retains_color_and_updates_the_actual_saved_image() {
    let mut file = fixture("type-10-tattoo-face-sap20.an2");
    let index = image_index(&file);
    let source = RgbImage::from_pixel(2, 3, Rgb([20, 100, 230]));
    file.replace_image(index, &png(source)).unwrap();
    file.resize_image(index, 20, 30).unwrap();
    let saved = reopen(&mut file);
    let record = saved.all_records().nth(index).unwrap();
    assert_eq!(record.field_value("CSP"), Some("RGB"));
    let pixels = image::load_from_memory(record.image_data.as_ref().unwrap())
        .unwrap()
        .to_rgb8();
    assert_eq!(pixels.dimensions(), (20, 30));
    assert!(pixels.pixels().all(|pixel| pixel.0 == [20, 100, 230]));
}

#[test]
fn resized_wsq_uses_png_or_legacy_raw_and_updates_the_binary_compression_byte() {
    let wsq = std::fs::read("test_data/sample.wsq").unwrap();
    for name in ["type-3.an2", "type-14-amp-nqm-utf8.an2"] {
        let mut file = fixture(name);
        let index = image_index(&file);
        file.replace_image(index, &wsq).unwrap();
        file.resize_image(index, 40, 50).unwrap();
        let bytes = file.to_bytes().unwrap();
        let saved = reopen(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        assert_eq!(
            record.decoded_image().unwrap().to_luma8().dimensions(),
            (40, 50)
        );
        if record.record_type == 3 {
            let offset: usize = saved
                .all_records()
                .take(index)
                .map(|r| r.field_value("LEN").unwrap().parse::<usize>().unwrap())
                .sum();
            assert_eq!(bytes[offset + 17], 0);
            assert_eq!(record.compression_algorithm(), Some("NONE"));
        } else {
            assert_eq!(record.compression_algorithm(), Some("PNG"));
        }
    }
}

#[test]
fn bitonal_resizing_preserves_ink_polarity_and_row_padding() {
    for kind in [5, 6, 8] {
        let mut file = fixture(if kind == 8 {
            "type-8-sig.an2"
        } else {
            "type-3.an2"
        });
        let index = image_index(&file);
        file.records[index - 1].record_type = kind;
        let source =
            GrayImage::from_fn(9, 2, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        file.replace_image(index, &png(source)).unwrap();
        file.resize_image(index, 18, 4).unwrap();
        let saved = reopen(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        assert_eq!(record.image_data.as_ref().unwrap().len(), 3 * 4);
        let pixels = record.decoded_image().unwrap().to_luma8();
        for (x, y, pixel) in pixels.enumerate_pixels() {
            assert_eq!(pixel[0], if (x / 2 + y / 2) % 2 == 0 { 0 } else { 255 });
        }
    }
}

#[test]
fn unchanged_dimensions_preserve_the_original_encoding_and_bytes() {
    let mut file = fixture("type-14-amp-nqm-utf8.an2");
    let index = image_index(&file);
    let before = file.to_bytes().unwrap();
    let image = file
        .all_records()
        .nth(index)
        .unwrap()
        .decoded_image()
        .unwrap();
    file.resize_image(index, image.width(), image.height())
        .unwrap();
    assert_eq!(file.to_bytes().unwrap(), before);
}

#[test]
fn invalid_sizes_and_unsupported_records_leave_the_file_unchanged() {
    let mut file = fixture("type-3.an2");
    let index = image_index(&file);
    let before = file.to_bytes().unwrap();
    for (width, height) in [(0, 10), (10, 0), (65536, 1), (u32::MAX, u32::MAX)] {
        assert!(file.resize_image(index, width, height).is_err());
        assert_eq!(file.to_bytes().unwrap(), before);
    }
    assert!(file.resize_image(0, 10, 10).is_err());
    assert!(file.resize_image(usize::MAX, 10, 10).is_err());
    let tagged = fixture("type-10-tattoo-face-sap20.an2");
    assert!(
        tagged
            .all_records()
            .nth(image_index(&tagged))
            .unwrap()
            .validate_resize(u32::MAX, u32::MAX)
            .is_err()
    );
}

#[test]
fn decoding_failure_keeps_pixels_and_calculated_fields_unchanged() {
    let mut file = fixture("type-3.an2");
    let index = image_index(&file);
    file.records[index - 1].image_data = Some(vec![0xff, 0xa0]);
    let before = file.to_bytes().unwrap();
    assert!(file.resize_image(index, 10, 10).is_err());
    assert_eq!(file.to_bytes().unwrap(), before);
}
