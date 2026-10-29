//! Image replacement with record-specific encoding and metadata updates.

use std::io::Cursor;

use anyhow::{Context, Result, ensure};
use image::{ColorType, DynamicImage, GrayImage, ImageFormat, Rgb, RgbImage};

use super::parser::NistFile;
use super::records::NistRecord;
use super::writer::set_field;

impl NistRecord {
    /// Whether this record has an image in a layout supported for replacement.
    pub fn can_replace_image(&self) -> bool {
        self.image_data.is_some() && matches!(self.record_type, 3..=6 | 8 | 10 | 13..=17)
    }
}

impl NistFile {
    /// Replace an image, update its metadata and recalculate LEN/CNT atomically.
    /// `record_index` counts Type-1 as zero. Invalid input leaves this file intact.
    pub fn replace_image(&mut self, record_index: usize, bytes: &[u8]) -> Result<()> {
        ensure!(
            self.warnings.is_empty(),
            "Cannot edit an incompletely parsed file"
        );
        let record = self
            .all_records()
            .nth(record_index)
            .context("No record selected")?;
        ensure!(
            record.can_replace_image(),
            "Image replacement is not supported for Type-{}",
            record.record_type
        );
        let imported = ImportedImage::decode(bytes)?;
        let mut candidate = self.clone();
        // Type-1 cannot pass can_replace_image, so subtraction is safe.
        replace_record_image(&mut candidate.records[record_index - 1], imported, bytes)?;
        candidate.recalculate()?;
        *self = candidate;
        Ok(())
    }
}

struct ImportedImage {
    pixels: DynamicImage,
    format: Option<ImageFormat>,
    wsq: bool,
}

impl ImportedImage {
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.starts_with(&[0xff, 0xa0]) {
            let wsq = crate::wsq::decode(bytes).context("Invalid WSQ replacement image")?;
            let pixels = GrayImage::from_raw(wsq.width, wsq.height, wsq.pixels)
                .context("Invalid WSQ pixel dimensions")?;
            return Ok(Self {
                pixels: pixels.into(),
                format: None,
                wsq: true,
            });
        }
        let format =
            image::guess_format(bytes).context("Choose a JPEG, PNG, WSQ, BMP, or TIFF image")?;
        ensure!(
            matches!(
                format,
                ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Bmp | ImageFormat::Tiff
            ),
            "Choose a JPEG, PNG, WSQ, BMP, or TIFF image"
        );
        let pixels = image::load_from_memory_with_format(bytes, format)
            .context("Cannot decode replacement image")?;
        ensure!(
            pixels.width() > 0 && pixels.height() > 0,
            "Image dimensions must be positive"
        );
        Ok(Self {
            pixels,
            format: Some(format),
            wsq: false,
        })
    }
}

fn replace_record_image(
    record: &mut NistRecord,
    imported: ImportedImage,
    original: &[u8],
) -> Result<()> {
    let (width, height) = (imported.pixels.width(), imported.pixels.height());
    let binary = (3..=8).contains(&record.record_type);
    if binary {
        ensure!(
            width <= u16::MAX as u32 && height <= u16::MAX as u32,
            "Legacy binary records support dimensions of at most 65535 pixels"
        );
    }
    let force_gray = matches!(record.record_type, 3..=6 | 8 | 13..=15)
        || (record.record_type == 17
            && record
                .fields
                .iter()
                .any(|f| f.field == 25 && f.value == "NIR"));
    let source_gray = matches!(
        imported.pixels.color(),
        ColorType::L8 | ColorType::La8 | ColorType::L16 | ColorType::La16
    );
    let gray = force_gray || source_gray;
    let has_alpha = imported.pixels.color().has_alpha();
    let normalized = if gray {
        if has_alpha {
            DynamicImage::ImageRgb8(on_white(&imported.pixels))
                .to_luma8()
                .into()
        } else {
            DynamicImage::ImageLuma8(imported.pixels.to_luma8())
        }
    } else {
        DynamicImage::ImageRgb8(on_white(&imported.pixels))
    };
    let bitonal = matches!(record.record_type, 5 | 6 | 8);
    let keep_wsq = imported.wsq && matches!(record.record_type, 3 | 4 | 13..=16);
    let (payload, compression) = if keep_wsq {
        (original.to_vec(), "WSQ20")
    } else if binary {
        let pixels = normalized.to_luma8();
        (
            if bitonal {
                pack_bitonal(&pixels)
            } else {
                pixels.into_raw()
            },
            "NONE",
        )
    } else {
        // Preserve compatible 8-bit JPEG/PNG streams. Normalize other formats,
        // transparency, precision, and color conversions to a lossless PNG.
        let unchanged_color = !has_alpha
            && matches!(imported.pixels.color(), ColorType::L8 | ColorType::Rgb8)
            && (!force_gray || source_gray);
        let keep_png = imported.format == Some(ImageFormat::Png)
            && unchanged_color
            && original.get(24) == Some(&8)
            && matches!(original.get(25), Some(0 | 2));
        let keep_jpeg = imported.format == Some(ImageFormat::Jpeg)
            && unchanged_color
            && baseline_jpeg(original);
        if keep_png {
            (original.to_vec(), "PNG")
        } else if keep_jpeg {
            (original.to_vec(), "JPEGB")
        } else {
            let mut encoded = Cursor::new(Vec::new());
            normalized
                .write_to(&mut encoded, ImageFormat::Png)
                .context("Encoding replacement PNG")?;
            (encoded.into_inner(), "PNG")
        }
    };
    set_field(record, 6, "HLL", width.to_string());
    set_field(record, 7, "VLL", height.to_string());
    if binary {
        set_field(record, 8, "GCA", compression.into());
        if record.record_type == 8 {
            // SRT 0 = scanned, uncompressed; ISR describes resolution, not encoding.
            set_field(record, 4, "SRT", "0".into());
        } else {
            let header = record
                .binary_header
                .as_mut()
                .context("Missing binary image header")?;
            ensure!(header.len() == 18, "Invalid binary image header");
            header[17] = u8::from(keep_wsq);
        }
    } else {
        set_field(record, 11, "CGA", compression.into());
        if record.record_type == 10 {
            set_field(record, 12, "CSP", if gray { "GRAY" } else { "RGB" }.into());
        } else {
            set_field(record, 12, "BPX", "8".into());
            if matches!(record.record_type, 16 | 17) {
                set_field(record, 13, "CSP", if gray { "GRAY" } else { "RGB" }.into());
            }
        }
        set_field(record, 999, "DATA", String::new());
    }
    record.image_data = Some(payload);
    Ok(())
}

/// Alpha is composited onto paper white, which also suits fingerprint/signature scans.
fn on_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let pixel = rgba.get_pixel(x, y);
        let alpha = u32::from(pixel[3]);
        Rgb([0, 1, 2].map(|channel| {
            ((u32::from(pixel[channel]) * alpha + 255 * (255 - alpha) + 127) / 255) as u8
        }))
    })
}

fn pack_bitonal(image: &GrayImage) -> Vec<u8> {
    let row_bytes = (image.width() as usize).div_ceil(8);
    let mut bytes = vec![0; row_bytes * image.height() as usize];
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[0] < 128 {
            bytes[y as usize * row_bytes + x as usize / 8] |= 1 << (7 - x % 8);
        }
    }
    bytes
}

/// Baseline JPEG is CGA JPEGB. Other JPEG variants are normalized to PNG.
fn baseline_jpeg(bytes: &[u8]) -> bool {
    let mut position = 2;
    while position + 4 <= bytes.len() {
        if bytes[position] != 0xff {
            return false;
        }
        position += 1;
        while bytes.get(position) == Some(&0xff) {
            position += 1;
        }
        let Some(&marker) = bytes.get(position) else {
            return false;
        };
        position += 1;
        if marker == 0xda || marker == 0xd9 {
            return false;
        }
        let Some(length) = bytes.get(position..position + 2) else {
            return false;
        };
        let length = u16::from_be_bytes([length[0], length[1]]) as usize;
        if length < 2 || position + length > bytes.len() {
            return false;
        }
        if marker == 0xc0 {
            return length >= 8 && bytes[position + 2] == 8 && matches!(bytes[position + 7], 1 | 3);
        }
        position += length;
    }
    false
}
