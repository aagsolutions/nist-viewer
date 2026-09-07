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

//! The main `eframe` application for the NIST viewer.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use anyhow::{Context as _, Result};
use eframe::egui::{self, CentralPanel, Color32, Panel, RichText, ScrollArea, Ui, Vec2};
use rfd::FileDialog;

use crate::nist::{NistFile, NistRecord, RecordType};
use crate::ui::image_render::{rgb_to_color_image, scaled_size, try_standard_image};

/// Holds everything the egui app needs between frames.
pub struct NistViewerApp {
    /// Currently loaded NIST file (None until the user opens one).
    file: Option<LoadedFile>,
    /// Index into `file.records` for the currently selected record (0 = Type-1).
    selected_record: usize,
    /// User-controlled zoom factor (1.0 = original pixels).
    zoom: f32,
    /// Background task result for decoding the selected image.
    pending_decode: Option<Receiver<Result<DecodedPixels>>>,
    /// Last error message to show in the status bar.
    status: Option<String>,
}

/// A decoded image ready for rendering.
struct DecodedImage {
    /// Original width of the decoded image.
    width: u32,
    /// Original height of the decoded image.
    height: u32,
    /// Display-sized image bytes (RGB) cached as an egui texture.
    texture: egui::TextureHandle,
    /// Algorithm used (e.g. `"WSQ"`, `"JPEG"`, `"PNG"`, `"raw"`).
    source: String,
    /// PPI metadata if known.
    ppi: Option<u32>,
}

/// Result of decoding image data on a worker thread — RGB pixels only, no egui
/// texture (the texture has to be created on the main thread with the live
/// `egui::Context`).
struct DecodedPixels {
    width: u32,
    height: u32,
    rgb: image::RgbImage,
    source: String,
    ppi: Option<u32>,
}

/// A loaded NIST file plus any cached per-record state.
struct LoadedFile {
    path: PathBuf,
    nist: NistFile,
    /// Cached image textures for already-decoded records.
    image_cache: Vec<Option<DecodedImage>>,
    /// The size in bytes of the file on disk.
    file_size: u64,
}

impl LoadedFile {
    fn new(path: PathBuf, nist: NistFile, file_size: u64) -> Self {
        let n = nist.all_records().count();
        Self {
            path,
            nist,
            image_cache: (0..n).map(|_| None).collect(),
            file_size,
        }
    }

    fn record(&self, index: usize) -> Option<&NistRecord> {
        self.nist.all_records().nth(index)
    }
}

impl NistViewerApp {
    /// Builds a fresh, empty application instance.
    pub fn new() -> Self {
        Self {
            file: None,
            selected_record: 0,
            zoom: 1.0,
            pending_decode: None,
            status: None,
        }
    }

    /// Convenience wrapper that calls `eframe::run_native` with our app.
    pub fn run() -> Result<()> {
        let viewport = egui::ViewportBuilder::default()
            .with_title("NIST Biometric Viewer")
            .with_inner_size([1200.0, 800.0]);
        let options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };
        eframe::run_native(
            "NIST Biometric Viewer",
            options,
            Box::new(|_cc| Ok(Box::new(NistViewerApp::new()) as Box<dyn eframe::App>)),
        )
        .map_err(|e| anyhow::anyhow!("failed to start eframe: {e}"))
    }

    /// Opens the file dialog and loads the chosen NIST file.
    fn open_file_dialog(&mut self, _ui: &mut Ui) {
        let mut dialog = FileDialog::new()
            .set_title("Open NIST biometric file")
            .add_filter(
                "NIST files (*.an2, *.nst, *.eft, *.nist)",
                &["an2", "nst", "eft", "nist"],
            );
        if let Some(parent) = self.file.as_ref().and_then(|f| f.path.parent()) {
            dialog = dialog.set_directory(parent);
        }
        if let Some(path) = dialog.pick_file() {
            self.load_file(path);
        }
    }

    /// Loads a NIST file from `path`, replacing any previously loaded file.
    fn load_file(&mut self, path: PathBuf) {
        self.status = None;
        self.selected_record = 0;
        self.zoom = 1.0;
        match load_nist_file(&path) {
            Ok((nist, size)) => {
                self.file = Some(LoadedFile::new(path.clone(), nist, size));
                self.status = Some(format!("Loaded {}", path.display()));
            }
            Err(e) => {
                self.file = None;
                self.status = Some(format!("Failed to open {}: {:#}", path.display(), e));
            }
        }
    }

    /// Triggers an async decode of the currently selected record.
    fn decode_selected(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let Some(record) = file.record(self.selected_record) else {
            return;
        };
        let Some(image_data) = record.image_data.clone() else {
            self.status = Some("Selected record has no image data".into());
            return;
        };
        let compression = record
            .compression_algorithm()
            .unwrap_or("NONE")
            .to_string();
        let (hll, vll) = record_dims(record);
        let (tx, rx) = channel();
        // We do the actual decode on a background thread to keep the UI smooth.
        std::thread::spawn(move || {
            let result = decode_image(&image_data, &compression, hll, vll);
            let _ = tx.send(result);
        });
        self.pending_decode = Some(rx);
    }

    /// Stores the result of a pending decode into the per-record cache.
    fn finish_pending_decode(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.pending_decode.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(pixels)) => {
                let texture = ctx.load_texture(
                    format!("nist-image-{}", self.selected_record),
                    rgb_to_color_image(&pixels.rgb),
                    egui::TextureOptions::LINEAR,
                );
                let image = DecodedImage {
                    width: pixels.width,
                    height: pixels.height,
                    texture,
                    source: pixels.source,
                    ppi: pixels.ppi,
                };
                if let Some(file) = self.file.as_mut() {
                    if let Some(slot) = file.image_cache.get_mut(self.selected_record) {
                        *slot = Some(image);
                    }
                }
            }
            Ok(Err(e)) => {
                self.status = Some(format!("Image decode failed: {:#}", e));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Not ready yet — put the receiver back.
                self.pending_decode = Some(rx);
                ctx.request_repaint();
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.status = Some("Image decode worker disappeared".into());
            }
        }
    }

    /// Saves the currently decoded image to disk.
    ///
    /// We re-decode from the raw record payload so the saved PNG always
    /// contains the full-resolution image, regardless of texture state.
    fn save_current_image(&mut self) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let Some(record) = file.record(self.selected_record) else {
            return;
        };
        let Some(image_data) = record.image_data.clone() else {
            self.status = Some("Selected record has no image data".into());
            return;
        };
        let compression = record
            .compression_algorithm()
            .unwrap_or("NONE")
            .to_string();
        let mut dialog = FileDialog::new()
            .set_title("Save decoded image as PNG")
            .add_filter("PNG image", &["png"])
            .set_file_name("decoded.png");
        if let Some(parent) = file.path.parent() {
            dialog = dialog.set_directory(parent);
        }
        let (hll, vll) = record_dims(record);
        if let Some(target) = dialog.save_file() {
            let result = decode_image(&image_data, &compression, hll, vll)
                .and_then(|px| {
                    px.rgb
                        .save(&target)
                        .with_context(|| format!("writing {}", target.display()))
                });
            match result {
                Ok(_) => self.status = Some(format!("Saved image to {}", target.display())),
                Err(e) => self.status = Some(format!("Save failed: {:#}", e)),
            }
        }
    }
}

impl Default for NistViewerApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for NistViewerApp {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.finish_pending_decode(&ctx);
        self.render_top_bar(ui);
        self.render_status_bar(ui);
        self.render_left_panel(ui);
        CentralPanel::default().show(ui, |ui| {
            self.render_central(ui);
        });
        self.handle_shortcuts(ui, &ctx);

        // Request repaint while a decode is in flight.
        if self.pending_decode.is_some() {
            ctx.request_repaint();
        }
        let _ = frame; // currently unused
    }
}

impl NistViewerApp {
    fn render_top_bar(&mut self, ui: &mut Ui) {
        Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open…").clicked() {
                        self.open_file_dialog(ui);
                        ui.close();
                    }
                    let has_image = self
                        .file
                        .as_ref()
                        .and_then(|f| f.image_cache.get(self.selected_record))
                        .and_then(|s| s.as_ref())
                        .is_some();
                    if ui
                        .add_enabled(has_image, egui::Button::new("Save image as PNG…"))
                        .clicked()
                    {
                        self.save_current_image();
                        ui.close();
                    }
                    if ui.button("Quit").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Image", |ui| {
                    let has_image_record = self
                        .file
                        .as_ref()
                        .and_then(|f| f.record(self.selected_record))
                        .map(|r| r.image_data.is_some())
                        .unwrap_or(false);
                    if ui
                        .add_enabled(has_image_record, egui::Button::new("Decode selected"))
                        .clicked()
                    {
                        self.decode_selected();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Zoom in").clicked() {
                        self.zoom = (self.zoom * 1.25).min(8.0);
                    }
                    if ui.button("Zoom out").clicked() {
                        self.zoom = (self.zoom / 1.25).max(0.05);
                    }
                    if ui.button("Fit").clicked() {
                        self.zoom = 1.0;
                    }
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About").clicked() {
                        self.status = Some(format!(
                            "NIST Biometric Viewer v{} — {}",
                            env!("CARGO_PKG_VERSION"),
                            "ANSI/NIST-ITL parser with WSQ & JPEG decoding"
                        ));
                        ui.close();
                    }
                });

                if let Some(file) = &self.file {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(format!(
                            "{} ({:.1} KB, {} records)",
                            file.path
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "(unnamed)".into()),
                            file.file_size as f32 / 1024.0,
                            file.nist.all_records().count(),
                        ));
                    });
                }
            });
        });
    }

    fn render_status_bar(&mut self, ui: &mut Ui) {
        Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(
                        self.status
                            .as_deref()
                            .unwrap_or("Ready — use File ▸ Open… to load a NIST file"),
                    )
                    .small()
                    .color(if self.status.is_some() {
                        Color32::LIGHT_GREEN
                    } else {
                        Color32::GRAY
                    }),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.pending_decode.is_some() {
                        ui.label(RichText::new("Decoding…").small().color(Color32::YELLOW));
                    } else {
                        ui.label(format!("Zoom: {:.0}%", self.zoom * 100.0));
                    }
                });
            });
        });
    }

    fn render_left_panel(&mut self, ui: &mut Ui) {
        Panel::left("records")
            .min_size(220.0)
            .max_size(360.0)
            .show(ui, |ui| {
                ui.heading("Records");
                if let Some(file) = &self.file {
                    ScrollArea::vertical().show(ui, |ui| {
                        for (i, rec) in file.nist.all_records().enumerate() {
                            let label =
                                format!("{} #{}", RecordType(rec.record_type).name(), rec.idc);
                            let selected = i == self.selected_record;
                            if ui.selectable_label(selected, label).clicked() {
                                self.selected_record = i;
                            }
                            if rec.image_data.is_some() {
                                ui.label(
                                    RichText::new(format!(
                                        "   {}×{}, {}",
                                        rec.field_value("HLL").unwrap_or("?"),
                                        rec.field_value("VLL").unwrap_or("?"),
                                        rec.compression_algorithm().unwrap_or("binary"),
                                    ))
                                    .small()
                                    .color(Color32::GRAY),
                                );
                            }
                        }
                    });
                } else {
                    ui.label(
                        RichText::new("No file loaded")
                            .color(Color32::GRAY)
                            .italics(),
                    );
                }
            });
    }

    fn render_central(&mut self, ui: &mut Ui) {
        let has_image = self
            .file
            .as_ref()
            .and_then(|f| f.record(self.selected_record))
            .map(|r| r.image_data.is_some());
        if let Some(has_image) = has_image {
            if has_image {
                self.render_image_view(ui);
            } else {
                self.render_text_view(ui);
            }
        } else if self.file.is_some() {
            ui.label("No record selected");
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space(80.0);
                ui.heading("NIST Biometric Viewer");
                ui.add_space(12.0);
                ui.label("Open a .an2 / .nst / .nist file to begin.");
                ui.add_space(20.0);
                if ui.button("Open file…").clicked() {
                    self.open_file_dialog(ui);
                }
            });
        }
    }

    /// Renders the central pane when the selected record carries image data.
    fn render_image_view(&mut self, ui: &mut Ui) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        let Some(rec) = file.record(self.selected_record) else {
            return;
        };
        let header = format!("{} (#{})", RecordType(rec.record_type).name(), rec.idc);
        let compression = rec
            .compression_algorithm()
            .unwrap_or("(binary)")
            .to_string();
        let native_size = match (
            rec.field_value("HLL").and_then(|s| s.parse::<u32>().ok()),
            rec.field_value("VLL").and_then(|s| s.parse::<u32>().ok()),
        ) {
            (Some(hll), Some(vll)) => Some((hll, vll)),
            _ => None,
        };

        ui.heading(header);
        ui.separator();

        let mut decode_clicked = false;
        ui.horizontal(|ui| {
            if ui.button("Decode image").clicked() {
                decode_clicked = true;
            }
            ui.label(format!("Compression: {compression}"));
            if let Some((hll, vll)) = native_size {
                ui.label(format!("Native size: {hll}×{vll}"));
            }
        });
        if decode_clicked {
            self.decode_selected();
        }

        ui.separator();

        let Some(file) = self.file.as_ref() else {
            return;
        };
        let slot = file
            .image_cache
            .get(self.selected_record)
            .and_then(|s| s.as_ref());
        if let Some(decoded) = slot {
            let (w, h) = scaled_size(decoded.width, decoded.height, 4096);
            let size = Vec2::new(w as f32 * self.zoom, h as f32 * self.zoom);
            ScrollArea::both().show(ui, |ui| {
                ui.add(
                    egui::Image::new(&decoded.texture)
                        .fit_to_exact_size(size)
                        .texture_options(egui::TextureOptions::NEAREST),
                );
            });
            ui.label(format!(
                "Decoded {}×{} as {} (ppi: {})",
                decoded.width,
                decoded.height,
                decoded.source,
                decoded
                    .ppi
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".to_string())
            ));
        } else {
            ui.label(
                RichText::new("No image decoded yet. Press \"Decode image\" above.")
                    .italics()
                    .color(Color32::GRAY),
            );
        }

        if let Some(rec) = self
            .file
            .as_ref()
            .and_then(|f| f.record(self.selected_record))
        {
            ui.collapsing("Raw record fields", |ui| {
                render_field_table(ui, rec);
            });
        }
    }

    /// Renders the central pane for a text-style record.
    fn render_text_view(&mut self, ui: &mut Ui) {
        let Some(rec) = self
            .file
            .as_ref()
            .and_then(|f| f.record(self.selected_record))
        else {
            return;
        };
        ui.heading(format!("{} (#{})", RecordType(rec.record_type).name(), rec.idc));
        ui.separator();
        render_field_table(ui, rec);
    }

    fn handle_shortcuts(&mut self, ui: &mut Ui, _ctx: &egui::Context) {
        if ui.ctx().input(|i| i.key_pressed(egui::Key::O) && i.modifiers.command) {
            self.open_file_dialog(ui);
        }
        if ui.ctx().input(|i| i.key_pressed(egui::Key::S) && i.modifiers.command) {
            self.save_current_image();
        }
    }
}

/// Renders the table of fields for any record kind.
fn render_field_table(ui: &mut Ui, rec: &NistRecord) {
    ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("fields")
            .num_columns(3)
            .spacing([12.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Field");
                ui.strong("Code");
                ui.strong("Value");
                ui.end_row();
                for f in &rec.fields {
                    ui.label(f.field.to_string());
                    ui.label(&f.code);
                    ui.label(&f.value);
                    ui.end_row();
                }
                if let Some(img) = &rec.image_data {
                    ui.label("999");
                    ui.label("DATA");
                    ui.label(format!("<{} bytes of binary>", img.len()));
                    ui.end_row();
                }
            });
    });
}

/// Loads and parses a NIST file from disk.
fn load_nist_file(path: &std::path::Path) -> Result<(NistFile, u64)> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let size = bytes.len() as u64;
    let nist = crate::nist::parser::decode(&bytes)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok((nist, size))
}

/// Best-effort extraction of (width, height) from the record's HLL/VLL fields.
fn record_dims(record: &NistRecord) -> (Option<u32>, Option<u32>) {
    (
        record.field_value("HLL").and_then(|s| s.parse().ok()),
        record.field_value("VLL").and_then(|s| s.parse().ok()),
    )
}

/// Decode an image payload using the compression algorithm hint.
fn decode_image(
    data: &[u8],
    compression: &str,
    hll: Option<u32>,
    vll: Option<u32>,
) -> Result<DecodedPixels> {
    // Sniff the payload magic first — the declared GCA/CGA code is not always
    // trustworthy (especially the 1-byte GCA of legacy binary Type-3..8
    // records, which predates WSQ).
    let sniffed = crate::nist::parser::sniff_compression(data).unwrap_or("");
    let compression_upper = if sniffed.is_empty() {
        compression.to_ascii_uppercase()
    } else {
        sniffed.to_string()
    };
    if let (Some(w), Some(h)) = (hll, vll) {
        if w == 0 || h == 0 {
            anyhow::bail!("record has zero image dimensions (HLL x VLL)");
        }
    }
    let (rgb, source, ppi, width, height) = if compression_upper.starts_with("WSQ") {
        let (color, decoded) = crate::ui::image_render::wsq_to_color_image(data)?;
        let rgb = color_to_rgb(&color);
        (
            rgb,
            "WSQ".to_string(),
            decoded.ppi,
            decoded.width,
            decoded.height,
        )
    } else if compression_upper.starts_with("JPEG")
        || compression_upper.starts_with("JPEGB")
        || compression_upper.starts_with("JPEGL")
    {
        let img = try_standard_image(data)
            .context("JPEG payload could not be decoded by the `image` crate")?;
        let w = img.width();
        let h = img.height();
        (img, "JPEG".to_string(), None, w, h)
    } else if compression_upper.starts_with("PNG") {
        let img = try_standard_image(data)
            .context("PNG payload could not be decoded by the `image` crate")?;
        let w = img.width();
        let h = img.height();
        (img, "PNG".to_string(), None, w, h)
    } else {
        // Raw pixels (typical for NONE-compressed Type-3..8 records).
        if let (Some(w), Some(h)) = (hll, vll) {
            let needed = (w as usize) * (h as usize);
            if w > 0 && h > 0 && data.len() >= needed {
                // 8-bit grayscale, one byte per pixel.
                let mut rgb = image::RgbImage::new(w, h);
                for (i, px) in rgb.pixels_mut().enumerate() {
                    let g = data[i];
                    *px = image::Rgb([g, g, g]);
                }
                return Ok(DecodedPixels {
                    width: w,
                    height: h,
                    rgb,
                    source: format!("raw grayscale ({compression})"),
                    ppi: None,
                });
            }
            // Bi-level (1 bit per pixel, MSB-first) — used by Type-8 signature
            // records with ISR=1. Bits set to 1 are ink (black).
            let row_bytes = ((w as usize) + 7) / 8;
            if w > 0 && h > 0 && data.len() >= row_bytes * (h as usize) {
                let mut rgb = image::RgbImage::new(w, h);
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let byte = data[y * row_bytes + x / 8];
                        let bit = (byte >> (7 - (x % 8))) & 1;
                        let g = if bit == 1 { 0u8 } else { 255u8 };
                        rgb.put_pixel(x as u32, y as u32, image::Rgb([g, g, g]));
                    }
                }
                return Ok(DecodedPixels {
                    width: w,
                    height: h,
                    rgb,
                    source: format!("raw bi-level bitmap ({compression})"),
                    ppi: None,
                });
            }
        }
        // Last-ditch attempt: treat it as an image and let the `image` crate decide.
        if let Some(img) = try_standard_image(data) {
            let w = img.width();
            let h = img.height();
            (
                img,
                format!("{compression} (auto-detected)"),
                None,
                w,
                h,
            )
        } else if !data.is_empty() {
            anyhow::bail!(
                "unsupported compression {compression:?} ({} bytes); try JPEG/PNG payload",
                data.len()
            );
        } else {
            anyhow::bail!("empty image payload");
        }
    };
    Ok(DecodedPixels {
        width,
        height,
        rgb,
        source,
        ppi,
    })
}

fn color_to_rgb(color: &egui::ColorImage) -> image::RgbImage {
    let mut img = image::RgbImage::new(color.size[0] as u32, color.size[1] as u32);
    for (i, px) in color.pixels.iter().enumerate() {
        let x = (i % color.size[0]) as u32;
        let y = (i / color.size[0]) as u32;
        img.put_pixel(x, y, image::Rgb([px.r(), px.g(), px.b()]));
    }
    img
}


