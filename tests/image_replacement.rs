//! Replacement images must survive serialization with matching image metadata.

use std::io::Cursor;

use image::{DynamicImage, GrayImage, ImageFormat, Luma, Rgb, RgbImage, Rgba, RgbaImage};
use nist_viewer::nist::{NistFile, parser::decode};

fn fixture(name: &str) -> NistFile {
    decode(&std::fs::read(format!("test_data/{name}")).unwrap()).unwrap()
}

fn encoded(image: impl Into<DynamicImage>, format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image.into().write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}

fn color_image() -> RgbImage {
    RgbImage::from_fn(13, 9, |x, y| Rgb([(x * 15) as u8, (y * 25) as u8, 127]))
}

fn image_index(file: &NistFile) -> usize {
    file.all_records()
        .position(|r| r.can_replace_image())
        .unwrap()
}

fn reopened(file: &mut NistFile) -> NistFile {
    let saved = file.to_bytes().unwrap();
    let reopened = decode(&saved).unwrap();
    assert!(reopened.warnings.is_empty(), "{:?}", reopened.warnings);
    assert_eq!(reopened.bytes_read, saved.len());
    assert_eq!(
        reopened
            .all_records()
            .map(|r| r.field_value("LEN").unwrap().parse::<usize>().unwrap())
            .sum::<usize>(),
        saved.len()
    );
    reopened
}

#[test]
fn replace_color_png_updates_metadata_preserves_other_records_and_reopens() {
    let mut file = fixture("type-10-tattoo-face-sap20.an2");
    let index = image_index(&file);
    let originals: Vec<_> = file.all_records().map(|r| r.image_data.clone()).collect();
    let png = encoded(color_image(), ImageFormat::Png);
    file.replace_image(index, &png).unwrap();
    let saved = reopened(&mut file);
    let record = saved.all_records().nth(index).unwrap();
    assert_eq!(record.field_value("HLL"), Some("13"));
    assert_eq!(record.field_value("VLL"), Some("9"));
    assert_eq!(record.field_value("CGA"), Some("PNG"));
    assert_eq!(record.field_value("CSP"), Some("RGB"));
    assert_eq!(record.image_data.as_ref().unwrap(), &png);
    for (i, record) in saved.all_records().enumerate() {
        if i != index {
            assert_eq!(record.image_data, originals[i]);
        }
    }
}

#[test]
fn compatible_jpeg_is_preserved_and_bmp_tiff_are_normalized_to_png() {
    for format in [ImageFormat::Jpeg, ImageFormat::Bmp, ImageFormat::Tiff] {
        let mut file = fixture("type-10-tattoo-face-sap20.an2");
        let index = image_index(&file);
        let bytes = encoded(color_image(), format);
        file.replace_image(index, &bytes).unwrap();
        let saved = reopened(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        if format == ImageFormat::Jpeg {
            assert_eq!(record.compression_algorithm(), Some("JPEGB"));
            assert_eq!(record.image_data.as_ref().unwrap(), &bytes);
        } else {
            assert_eq!(record.compression_algorithm(), Some("PNG"));
            assert_eq!(
                image::load_from_memory(record.image_data.as_ref().unwrap())
                    .unwrap()
                    .to_rgb8(),
                color_image()
            );
        }
    }
}

#[test]
fn fingerprint_color_input_becomes_eight_bit_grayscale() {
    let mut file = fixture("type-14-amp-nqm-utf8.an2");
    let index = image_index(&file);
    let expected = DynamicImage::ImageRgb8(color_image()).to_luma8();
    file.replace_image(index, &encoded(color_image(), ImageFormat::Png))
        .unwrap();
    let saved = reopened(&mut file);
    let record = saved.all_records().nth(index).unwrap();
    assert_eq!(record.field_value("BPX"), Some("8"));
    assert_eq!(record.compression_algorithm(), Some("PNG"));
    let image = image::load_from_memory(record.image_data.as_ref().unwrap()).unwrap();
    assert_eq!(image.color(), image::ColorType::L8);
    assert_eq!(image.to_luma8(), expected);
}

#[test]
fn wsq_is_preserved_in_tagged_and_binary_fingerprints_with_matching_headers() {
    let wsq = std::fs::read("test_data/sample.wsq").unwrap();
    let dimensions = nist_viewer::wsq::decode(&wsq).unwrap();
    for name in [
        "type-3.an2",
        "type-9-4-iafis.an2",
        "type-14-amp-nqm-utf8.an2",
    ] {
        let mut file = fixture(name);
        let index = image_index(&file);
        file.replace_image(index, &wsq).unwrap();
        let saved_bytes = file.to_bytes().unwrap();
        let saved = decode(&saved_bytes).unwrap();
        assert!(saved.warnings.is_empty());
        let record = saved.all_records().nth(index).unwrap();
        assert_eq!(record.image_data.as_ref().unwrap(), &wsq);
        assert_eq!(
            record.field_value("HLL"),
            Some(dimensions.width.to_string().as_str())
        );
        assert_eq!(
            record.field_value("VLL"),
            Some(dimensions.height.to_string().as_str())
        );
        assert_eq!(record.compression_algorithm(), Some("WSQ20"));
        if record.record_type <= 4 {
            let offset: usize = saved
                .all_records()
                .take(index)
                .map(|r| r.field_value("LEN").unwrap().parse::<usize>().unwrap())
                .sum();
            assert_eq!(saved_bytes[offset + 17], 1);
        }
    }
}

#[test]
fn legacy_grayscale_replacement_resets_compression_byte_and_preserves_finger_positions() {
    let mut file = fixture("type-9-4-iafis.an2");
    let index = image_index(&file);
    let old_fgp = file
        .all_records()
        .nth(index)
        .unwrap()
        .field_value("FGP")
        .unwrap()
        .to_string();
    file.replace_image(index, &std::fs::read("test_data/sample.wsq").unwrap())
        .unwrap();
    file.replace_image(index, &encoded(color_image(), ImageFormat::Png))
        .unwrap();
    let bytes = file.to_bytes().unwrap();
    let saved = reopened(&mut file);
    let record = saved.all_records().nth(index).unwrap();
    assert_eq!(record.compression_algorithm(), Some("NONE"));
    assert_eq!(record.field_value("FGP"), Some(old_fgp.as_str()));
    assert_eq!(
        record.image_data.as_ref().unwrap(),
        &DynamicImage::ImageRgb8(color_image()).to_luma8().into_raw()
    );
    let offset: usize = saved
        .all_records()
        .take(index)
        .map(|r| r.field_value("LEN").unwrap().parse::<usize>().unwrap())
        .sum();
    assert_eq!(bytes[offset + 17], 0);
}

#[test]
fn signature_replacement_packs_rows_and_updates_representation() {
    let mut file = fixture("type-8-sig.an2");
    let index = image_index(&file);
    let pixels = GrayImage::from_fn(9, 2, |x, y| Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
    file.replace_image(index, &encoded(pixels, ImageFormat::Png))
        .unwrap();
    let saved = reopened(&mut file);
    let record = saved.all_records().nth(index).unwrap();
    assert_eq!(record.field_value("SRT"), Some("0"));
    assert_eq!(record.field_value("HLL"), Some("9"));
    assert_eq!(record.field_value("VLL"), Some("2"));
    assert_eq!(
        record.image_data.as_deref(),
        Some([0xaa, 0x80, 0x55, 0].as_slice())
    );
    assert_eq!(record.compression_algorithm(), Some("NONE"));
}

#[test]
fn binary_five_and_six_replacements_use_packed_bitonal_pixels() {
    for kind in [5, 6] {
        let mut file = fixture("type-3.an2");
        let index = image_index(&file);
        file.records[index - 1].record_type = kind;
        file.recalculate().unwrap();
        let pixels = GrayImage::from_pixel(9, 2, Luma([0]));
        file.replace_image(index, &encoded(pixels, ImageFormat::Png))
            .unwrap();
        let saved = reopened(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        assert_eq!(
            record.image_data.as_deref(),
            Some([0xff, 0x80, 0xff, 0x80].as_slice())
        );
        assert_eq!(record.compression_algorithm(), Some("NONE"));
    }
}

#[test]
fn transparency_is_composited_on_white_and_bit_depth_is_normalized() {
    let mut file = fixture("type-14-amp-nqm-utf8.an2");
    let index = image_index(&file);
    let rgba = RgbaImage::from_fn(2, 1, |x, _| Rgba([0, 0, 0, if x == 0 { 0 } else { 255 }]));
    file.replace_image(index, &encoded(rgba, ImageFormat::Png))
        .unwrap();
    let record = file.all_records().nth(index).unwrap();
    let pixels = image::load_from_memory(record.image_data.as_ref().unwrap())
        .unwrap()
        .to_luma8();
    assert_eq!(pixels.as_raw(), &[255, 0]);
}

#[test]
fn invalid_image_and_oversized_binary_dimensions_leave_file_unchanged() {
    let mut file = fixture("type-3.an2");
    let index = image_index(&file);
    let before = file.to_bytes().unwrap();
    for bytes in [
        b"invalid image".to_vec(),
        vec![0xff, 0xa0],
        encoded(
            GrayImage::from_pixel(65536, 1, Luma([255])),
            ImageFormat::Png,
        ),
    ] {
        assert!(file.replace_image(index, &bytes).is_err());
        assert_eq!(file.to_bytes().unwrap(), before);
    }
    assert!(file.replace_image(0, b"invalid").is_err());
    assert!(file.replace_image(usize::MAX, b"invalid").is_err());
}

#[test]
fn type16_and_type17_metadata_survives_reopening() {
    for kind in [16, 17] {
        let mut file = fixture("type-14-amp-nqm-utf8.an2");
        let index = image_index(&file);
        file.records[index - 1].record_type = kind;
        file.replace_image(index, &encoded(color_image(), ImageFormat::Png))
            .unwrap();
        let saved = reopened(&mut file);
        let record = saved.all_records().nth(index).unwrap();
        assert_eq!(record.field_value("HLL"), Some("13"));
        assert_eq!(record.field_value("CSP"), Some("RGB"));
        assert_eq!(record.field_value("BPX"), Some("8"));
        assert_eq!(record.compression_algorithm(), Some("PNG"));
    }
}

#[test]
fn raw_pixel_magic_does_not_change_compression_on_reopening() {
    let mut file = fixture("type-3.an2");
    let index = image_index(&file);
    let pixels = GrayImage::from_raw(8, 1, b"\x89PNG\r\n\x1a\n".to_vec()).unwrap();
    file.replace_image(index, &encoded(pixels, ImageFormat::Png))
        .unwrap();
    let saved = reopened(&mut file);
    assert_eq!(
        saved
            .all_records()
            .nth(index)
            .unwrap()
            .compression_algorithm(),
        Some("NONE")
    );
}
