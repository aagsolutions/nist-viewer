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

//! Smoke tests for the NIST parser and image decoders against the bundled
//! ANSI/NIST-ITL reference files.

#[test]
fn parse_type10_file() {
    let data = std::fs::read("test_data/type-10-tattoo-face-sap20.an2").unwrap();
    let nist = nist_viewer_parse(&data);
    assert!(nist.all_records().count() >= 2, "should contain Type-1 plus records");
}

#[test]
fn parse_type14_file_and_decode_images() {
    let data = std::fs::read("test_data/type-14-amp-nqm-utf8.an2").unwrap();
    let nist = nist_viewer_parse(&data);
    let mut decoded_any = false;
    for rec in nist.all_records() {
        if let Some(img) = &rec.image_data {
            let comp = rec.compression_algorithm().unwrap_or("NONE");
            if comp.to_ascii_uppercase().starts_with("WSQ") {
                let decoded = nist_viewer_wsq(img);
                assert!(decoded.width > 0 && decoded.height > 0);
                assert_eq!(
                    decoded.pixels.len() as u64,
                    decoded.width as u64 * decoded.height as u64
                );
                decoded_any = true;
            }
        }
    }
    assert!(decoded_any, "expected at least one WSQ image to decode");
}

fn nist_viewer_parse(data: &[u8]) -> nist_viewer::nist::NistFile {
    nist_viewer::nist::parser::decode(data).expect("parse failed")
}

fn nist_viewer_wsq(data: &[u8]) -> nist_viewer::wsq::DecodedWsq {
    nist_viewer::wsq::decode(data).expect("wsq decode failed")
}

#[test]
fn parse_type3_and_render_raw() {
    let data = std::fs::read("test_data/type-3.an2").unwrap();
    let nist = nist_viewer_parse(&data);
    let rec = nist
        .all_records()
        .find(|r| r.record_type == 3)
        .expect("expected a Type-3 record");
    let img = rec.image_data.as_ref().expect("Type-3 should carry image data");
    assert_eq!(rec.compression_algorithm(), Some("NONE"));
    let hll: usize = rec.field_value("HLL").unwrap().parse().unwrap();
    let vll: usize = rec.field_value("VLL").unwrap().parse().unwrap();
    assert_eq!(img.len(), hll * vll, "raw payload should be HLL*VLL bytes");
    let mean: u64 = img.iter().map(|p| *p as u64).sum::<u64>() / img.len() as u64;
    assert!((40..=220).contains(&mean), "mean pixel value {mean} looks wrong");
}

#[test]
fn parse_type4_and_render_raw() {
    let data = std::fs::read("test_data/type-9-4-iafis.an2").unwrap();
    let nist = nist_viewer_parse(&data);
    let rec = nist
        .all_records()
        .find(|r| r.record_type == 4)
        .expect("expected a Type-4 record");
    let img = rec.image_data.as_ref().expect("Type-4 should carry image data");
    assert_eq!(rec.compression_algorithm(), Some("NONE"));
    let hll: usize = rec.field_value("HLL").unwrap().parse().unwrap();
    let vll: usize = rec.field_value("VLL").unwrap().parse().unwrap();
    assert_eq!(img.len(), hll * vll, "raw payload should be HLL*VLL bytes");
}

/// Golden-value test: our WSQ decoder must reproduce, pixel for pixel, the
/// reference output produced by the jnbis (NBIS-compatible) decoder.
#[test]
fn wsq_matches_reference_decoder() {
    let wsq = std::fs::read("test_data/sample.wsq").unwrap();
    let reference = std::fs::read("test_data/sample.png").unwrap();
    let decoded = nist_viewer_wsq(&wsq);
    let reference_img = image::load_from_memory(&reference)
        .expect("reference png")
        .to_luma8();
    assert_eq!(decoded.width, reference_img.width());
    assert_eq!(decoded.height, reference_img.height());
    assert_eq!(decoded.pixels.len() as u64, reference_img.len() as u64);
    let mismatches = decoded
        .pixels
        .iter()
        .zip(reference_img.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(mismatches, 0, "WSQ output differs from reference decoder");
}
