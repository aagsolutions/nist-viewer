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

//! Bridge between raw decoded pixels (grayscale bytes or RGB) and egui textures.
//!
//! We use the `image` crate to do the heavy lifting (color conversion, alpha
//! compositing, format detection) and then upload the result into an
//! [`egui::ColorImage`] that we can paint with [`egui::Ui::image`].

use anyhow::{Context, Result};
use eframe::egui::{self, ColorImage};
use image::{DynamicImage, GrayImage, RgbImage};

use crate::wsq::DecodedWsq;

/// Load an 8-bit grayscale WSQ buffer and return an [`egui::ColorImage`].
pub fn wsq_to_color_image(bytes: &[u8]) -> Result<(ColorImage, DecodedWsq)> {
    let decoded = crate::wsq::decode(bytes).context("WSQ decode failed")?;
    let img = GrayImage::from_raw(decoded.width, decoded.height, decoded.pixels.clone())
        .context("WSQ decoded image has inconsistent dimensions")?;
    let dynamic = DynamicImage::ImageLuma8(img).to_rgb8();
    Ok((rgb_to_color_image(&dynamic), decoded))
}

/// Convert an `image::RgbImage` into an `egui::ColorImage`.
pub fn rgb_to_color_image(rgb: &RgbImage) -> ColorImage {
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let mut pixels = Vec::with_capacity(w * h);
    for px in rgb.pixels() {
        pixels.push(egui::Color32::from_rgb(px[0], px[1], px[2]));
    }
    ColorImage::new([w, h], pixels)
}

/// Try to decode `bytes` as a standard format (PNG, JPEG, BMP, …) using the
/// `image` crate. Returns `None` if no format matched.
pub fn try_standard_image(bytes: &[u8]) -> Option<RgbImage> {
    image::load_from_memory(bytes).ok().map(|d| d.to_rgb8())
}

/// Scales an image so that the longest side fits within `max_dim` while
/// preserving the aspect ratio.
pub fn scaled_size(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let long = width.max(height);
    if long <= max_dim {
        return (width, height);
    }
    let scale = max_dim as f32 / long as f32;
    (
        ((width as f32) * scale).round().max(1.0) as u32,
        ((height as f32) * scale).round().max(1.0) as u32,
    )
}
