//! Pure Rust encoding, round trips, and independently generated reference images.

use nist_viewer::wsq::{decode, encoder};

fn assert_round_trip(pixels: &[u8], width: u32, height: u32, ppi: Option<u32>) {
    let bytes = encoder::encode(pixels, width, height, ppi, 0.75).unwrap();
    assert!(bytes.starts_with(&[0xff, 0xa0]));
    assert!(bytes.ends_with(&[0xff, 0xa1]));
    let decoded = decode(&bytes).unwrap();
    assert_eq!((decoded.width, decoded.height), (width, height));
    assert_eq!(decoded.ppi, ppi);
    assert_eq!(decoded.pixels.len(), pixels.len());
}

#[test]
fn encoded_resampled_fingerprint_is_valid_wsq() {
    let original = decode(&std::fs::read("test_data/sample.wsq").unwrap()).unwrap();
    let pixels =
        image::GrayImage::from_raw(original.width, original.height, original.pixels).unwrap();
    for (width, height) in [(128, 128), (256, 256), (257, 193)] {
        let resized = image::imageops::resize(
            &pixels,
            width,
            height,
            image::imageops::FilterType::Lanczos3,
        );
        assert_round_trip(resized.as_raw(), width, height, original.ppi);
        let encoded = encoder::encode(resized.as_raw(), width, height, original.ppi, 0.75).unwrap();
        let reference = image::open(format!(
            "test_data/wsq_encoder/fingerprint-{width}x{height}-reference.png"
        ))
        .unwrap()
        .to_luma8();
        let decoded = decode(&encoded).unwrap();
        assert_reference_pixels(&decoded.pixels, reference.as_raw());
        let mse = resized
            .as_raw()
            .iter()
            .zip(&decoded.pixels)
            .map(|(input, output)| (f64::from(*input) - f64::from(*output)).powi(2))
            .sum::<f64>()
            / decoded.pixels.len() as f64;
        // Independently measured NBIS 5 error on the same resized input at 0.75 bpp.
        let reference_mse = match (width, height) {
            (128, 128) => 287.549133,
            (256, 256) => 212.184982,
            (257, 193) => 241.971230,
            _ => unreachable!(),
        };
        assert!(
            mse <= reference_mse * 1.02,
            "{width}x{height}: fingerprint MSE {mse} exceeds reference {reference_mse}"
        );
    }
}

fn assert_reference_pixels(decoded: &[u8], reference: &[u8]) {
    assert_eq!(decoded.len(), reference.len());
    let mut count = 0;
    let mut maximum = 0;
    for (actual, expected) in decoded.iter().zip(reference) {
        if actual != expected {
            count += 1;
        }
        maximum = maximum.max(actual.abs_diff(*expected));
    }
    // Independent float implementations can round a boundary value differently.
    assert!(
        maximum <= 1 && count <= decoded.len() / 1000 + 1,
        "{count} reference pixels differ, maximum error {maximum}"
    );
}

#[test]
fn rust_wsq_streams_match_independent_decoder_fixtures() {
    for (width, height) in [(128, 128), (256, 256), (257, 193)] {
        let bytes = std::fs::read(format!(
            "test_data/wsq_encoder/fingerprint-{width}x{height}.wsq"
        ))
        .unwrap();
        let reference = image::open(format!(
            "test_data/wsq_encoder/fingerprint-{width}x{height}-reference.png"
        ))
        .unwrap()
        .to_luma8();
        let decoded = decode(&bytes).unwrap();
        assert_eq!((decoded.width, decoded.height), (width, height));
        assert_reference_pixels(&decoded.pixels, reference.as_raw());
    }
}

#[test]
fn flat_images_encode_without_non_finite_scaling() {
    for value in [0, 128, 255] {
        let pixels = vec![value; 128 * 128];
        assert_round_trip(&pixels, 128, 128, None);
        let bytes = encoder::encode(&pixels, 128, 128, Some(500), 0.75).unwrap();
        assert!(
            decode(&bytes)
                .unwrap()
                .pixels
                .iter()
                .all(|pixel| *pixel == value)
        );
    }
}

#[test]
fn low_entropy_gradients_keep_their_structure() {
    let pixels: Vec<u8> = (0..128 * 128)
        .map(|index| ((index % 128 + index / 128) % 256) as u8)
        .collect();
    assert_round_trip(&pixels, 128, 128, Some(500));
    let bytes = encoder::encode(&pixels, 128, 128, Some(500), 0.75).unwrap();
    let decoded = decode(&bytes).unwrap();
    let squared_error: f64 = pixels
        .iter()
        .zip(&decoded.pixels)
        .map(|(a, b)| (f64::from(*a) - f64::from(*b)).powi(2))
        .sum();
    // WSQ is lossy; RMS <= 8 levels still distinguishes a retained gradient
    // from a collapsed/overflowed coefficient stream (RMS > 50 levels).
    assert!(
        squared_error / pixels.len() as f64 <= 64.0,
        "gradient MSE {}; sample input/output {:?}",
        squared_error / pixels.len() as f64,
        [0, 127, 128 * 64 + 64, 128 * 128 - 1].map(|index| (pixels[index], decoded.pixels[index]))
    );
}

#[test]
fn invalid_encoder_input_is_rejected() {
    for (width, height) in [(0, 128), (127, 128), (128, 127), (65536, 128)] {
        assert!(encoder::encode(&[], width, height, None, 0.75).is_err());
    }
    assert!(encoder::encode(&[0], 128, 128, None, 0.75).is_err());
    for bitrate in [0.0, -1.0, f32::NAN, f32::INFINITY, 3.0] {
        assert!(encoder::encode(&vec![0; 128 * 128], 128, 128, None, bitrate).is_err());
    }
}

#[test]
fn source_bitrate_is_preserved_when_present_and_valid() {
    for (text, expected) in [
        ("WSQ_BITRATE 1.5", 1.5),
        ("WSQ_BITRATE NaN", 0.75),
        ("WSQ_BITRATE 0", 0.75),
        ("OTHER 1", 0.75),
    ] {
        let mut bytes = vec![0xff, 0xa0, 0xff, 0xa8];
        bytes.extend(((text.len() + 2) as u16).to_be_bytes());
        bytes.extend(text.as_bytes());
        assert_eq!(encoder::source_bitrate(&bytes), expected);
    }
    assert_eq!(
        encoder::source_bitrate(&[0xff, 0xa0, 0xff, 0xa8, 0, 100]),
        0.75
    );
}
