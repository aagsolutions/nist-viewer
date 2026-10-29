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

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

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
    pending_decode: Option<(usize, Receiver<Result<DecodedPixels>>)>,
    /// Last error message to show in the status bar.
    status: Option<String>,
    close_confirmed: bool,
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
    dirty: bool,
    /// Drafts retain incomplete input until recalculation or saving validates it.
    field_drafts: BTreeMap<(usize, u16), String>,
}

impl LoadedFile {
    fn new(path: PathBuf, nist: NistFile, file_size: u64) -> Self {
        let n = nist.all_records().count();
        Self {
            path,
            nist,
            image_cache: (0..n).map(|_| None).collect(),
            file_size,
            dirty: false,
            field_drafts: BTreeMap::new(),
        }
    }

    fn record(&self, index: usize) -> Option<&NistRecord> {
        self.nist.all_records().nth(index)
    }

    fn prepare_save(&self) -> Result<(NistFile, Vec<u8>)> {
        let mut nist = self.nist.clone();
        for (&(index, number), value) in &self.field_drafts {
            let record = if index == 0 {
                &mut nist.transaction
            } else {
                nist.records
                    .get_mut(index - 1)
                    .context("Record no longer exists")?
            };
            let field = record
                .fields
                .iter_mut()
                .find(|f| f.field == number)
                .context("Field no longer exists")?;
            field.value = unescape_field_value(value)
                .with_context(|| format!("Record {}, field {number}", index + 1))?;
        }
        let bytes = nist.to_bytes()?;
        Ok((nist, bytes))
    }

    fn commit(&mut self, nist: NistFile) {
        if !self.field_drafts.is_empty() {
            self.image_cache.iter_mut().for_each(|slot| *slot = None);
        }
        self.nist = nist;
        self.field_drafts.clear();
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
            close_confirmed: false,
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
            if self.confirm_unsaved_changes() {
                self.load_file(path);
            }
        }
    }

    /// Loads a NIST file from `path`, replacing any previously loaded file.
    fn load_file(&mut self, path: PathBuf) {
        self.status = None;
        match load_nist_file(&path) {
            Ok((nist, size)) => {
                self.selected_record = 0;
                self.zoom = 1.0;
                self.pending_decode = None;
                let warnings = nist.warnings.join("; ");
                self.file = Some(LoadedFile::new(path.clone(), nist, size));
                self.status = Some(if warnings.is_empty() {
                    format!("Loaded {}", path.display())
                } else {
                    format!("Opened read-only: {warnings}")
                });
            }
            Err(e) => {
                self.status = Some(format!("Failed to open {}: {:#}", path.display(), e));
            }
        }
    }

    /// Triggers an async decode of the currently selected record.
    fn decode_selected(&mut self) {
        if self
            .file
            .as_ref()
            .is_some_and(|f| !f.field_drafts.is_empty())
            && !self.recalculate_fields()
        {
            return;
        }
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
        let compression = record.compression_algorithm().unwrap_or("NONE").to_string();
        let (hll, vll) = record_dims(record);
        let (tx, rx) = channel();
        // We do the actual decode on a background thread to keep the UI smooth.
        std::thread::spawn(move || {
            let result = decode_image(&image_data, &compression, hll, vll);
            let _ = tx.send(result);
        });
        self.pending_decode = Some((self.selected_record, rx));
    }

    /// Stores the result of a pending decode into the per-record cache.
    fn finish_pending_decode(&mut self, ctx: &egui::Context) {
        let Some((record_index, rx)) = self.pending_decode.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(pixels)) => {
                let texture = ctx.load_texture(
                    format!("nist-image-{record_index}"),
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
                    if let Some(slot) = file.image_cache.get_mut(record_index) {
                        *slot = Some(image);
                    }
                }
            }
            Ok(Err(e)) => {
                self.status = Some(format!("Image decode failed: {:#}", e));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Not ready yet — put the receiver back.
                self.pending_decode = Some((record_index, rx));
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
        if self
            .file
            .as_ref()
            .is_some_and(|f| !f.field_drafts.is_empty())
            && !self.recalculate_fields()
        {
            return;
        }
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
        let compression = record.compression_algorithm().unwrap_or("NONE").to_string();
        let mut dialog = FileDialog::new()
            .set_title("Save decoded image as PNG")
            .add_filter("PNG image", &["png"])
            .set_file_name("decoded.png");
        if let Some(parent) = file.path.parent() {
            dialog = dialog.set_directory(parent);
        }
        let (hll, vll) = record_dims(record);
        if let Some(target) = dialog.save_file() {
            let result = decode_image(&image_data, &compression, hll, vll).and_then(|px| {
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

    fn recalculate_fields(&mut self) -> bool {
        let Some(file) = self.file.as_mut() else {
            return false;
        };
        match file.prepare_save() {
            Ok((nist, _)) => {
                file.commit(nist);
                file.dirty = true;
                self.pending_decode = None;
                self.status = Some(
                    "Recalculated all LEN fields and the CNT directory. Changes are not saved yet."
                        .into(),
                );
                true
            }
            Err(error) => {
                self.status = Some(format!("Cannot recalculate: {error:#}"));
                false
            }
        }
    }

    /// Saves a validated copy and only commits edits after the write succeeds.
    fn save_nist_file(&mut self, save_as: bool) -> bool {
        let Some(file) = self.file.as_ref() else {
            return false;
        };
        let (nist, bytes) = match file.prepare_save() {
            Ok(prepared) => prepared,
            Err(error) => {
                self.status = Some(format!("Cannot save: {error:#}"));
                return false;
            }
        };
        let target = if save_as {
            let mut dialog = FileDialog::new()
                .set_title("Save NIST file")
                .add_filter("NIST biometric files", &["an2", "nst", "eft", "nist"]);
            if let Some(parent) = file.path.parent() {
                dialog = dialog.set_directory(parent);
            }
            if let Some(name) = file.path.file_name() {
                dialog = dialog.set_file_name(name.to_string_lossy());
            }
            let Some(target) = dialog.save_file() else {
                return false;
            };
            target
        } else {
            file.path.clone()
        };
        let result = write_nist_file(&target, &bytes);
        match result {
            Ok(()) => {
                let file = self.file.as_mut().unwrap();
                file.commit(nist);
                file.path = target.clone();
                file.file_size = bytes.len() as u64;
                file.dirty = false;
                self.pending_decode = None;
                self.status = Some(format!(
                    "Saved {} (LEN and CNT recalculated)",
                    target.display()
                ));
                true
            }
            Err(error) => {
                self.status = Some(format!("Save failed: {error:#}"));
                false
            }
        }
    }

    fn confirm_unsaved_changes(&mut self) -> bool {
        if !self.file.as_ref().is_some_and(|file| file.dirty) {
            return true;
        }
        match rfd::MessageDialog::new()
            .set_title("Unsaved NIST changes")
            .set_description("Save your changes before continuing?\nChoose No to discard them, or Cancel to keep editing.")
            .set_buttons(rfd::MessageButtons::YesNoCancel)
            .show()
        {
            rfd::MessageDialogResult::Yes => self.save_nist_file(false),
            rfd::MessageDialogResult::No => true,
            _ => false,
        }
    }

    fn request_close(&mut self, ctx: &egui::Context) {
        if self.confirm_unsaved_changes() {
            self.close_confirmed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
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
        if ctx.input(|i| i.viewport().close_requested()) && !self.close_confirmed {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.request_close(&ctx);
        }
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
                    let writable = self
                        .file
                        .as_ref()
                        .is_some_and(|f| f.nist.warnings.is_empty());
                    if ui
                        .add_enabled(writable, egui::Button::new("Save NIST file"))
                        .clicked()
                    {
                        self.save_nist_file(false);
                        ui.close();
                    }
                    if ui
                        .add_enabled(writable, egui::Button::new("Save NIST file as…"))
                        .clicked()
                    {
                        self.save_nist_file(true);
                        ui.close();
                    }
                    ui.separator();
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
                        self.request_close(ui.ctx());
                    }
                });
                let writable = self
                    .file
                    .as_ref()
                    .is_some_and(|f| f.nist.warnings.is_empty());
                if ui
                    .add_enabled(writable, egui::Button::new("Recalculate fields"))
                    .clicked()
                {
                    self.recalculate_fields();
                }
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
                            "{}{} ({:.1} KB, {} records)",
                            file.path
                                .file_name()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_else(|| "(unnamed)".into()),
                            if file.dirty { " * Modified" } else { "" },
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
            ScrollArea::both()
                .max_height((ui.available_height() * 0.55).max(120.0))
                .show(ui, |ui| {
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

        ui.collapsing("Record fields (edit)", |ui| self.render_selected_fields(ui));
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
        ui.heading(format!(
            "{} (#{})",
            RecordType(rec.record_type).name(),
            rec.idc
        ));
        ui.separator();
        self.render_selected_fields(ui);
    }

    fn render_selected_fields(&mut self, ui: &mut Ui) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let rec = if self.selected_record == 0 {
            &file.nist.transaction
        } else {
            let Some(record) = file.nist.records.get(self.selected_record - 1) else {
                return;
            };
            record
        };
        if render_field_table(
            ui,
            rec,
            self.selected_record,
            &mut file.field_drafts,
            file.nist.warnings.is_empty(),
        ) {
            file.dirty = true;
            self.pending_decode = None;
            file.image_cache.iter_mut().for_each(|slot| *slot = None);
            self.status =
                Some("Fields modified. Recalculate or save to update LEN and CNT.".into());
        }
    }

    fn handle_shortcuts(&mut self, ui: &mut Ui, _ctx: &egui::Context) {
        if ui
            .ctx()
            .input(|i| i.key_pressed(egui::Key::O) && i.modifiers.command)
        {
            self.open_file_dialog(ui);
        }
        if ui
            .ctx()
            .input(|i| i.key_pressed(egui::Key::S) && i.modifiers.command)
        {
            let save_as = ui.ctx().input(|i| i.modifiers.shift);
            self.save_nist_file(save_as);
        }
    }
}

/// Renders the table of fields for any record kind.
fn render_field_table(
    ui: &mut Ui,
    rec: &NistRecord,
    record_index: usize,
    drafts: &mut BTreeMap<(usize, u16), String>,
    writable: bool,
) -> bool {
    let mut changed = false;
    ui.label(
        "Edit values below, then recalculate or save. LEN and CNT are calculated automatically.",
    );
    ui.small(
        "Use \\x1e for subfields (RS), \\x1f for items (US), and \\\\ for a literal backslash.",
    );
    ScrollArea::vertical()
        .id_salt(("record-fields", record_index))
        .show(ui, |ui| {
            egui::Grid::new(("fields", record_index))
                .num_columns(3)
                .spacing([12.0, 4.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("Field");
                    ui.strong("Code");
                    ui.strong("Value");
                    ui.end_row();
                    for f in &rec.fields {
                        if f.field == 999 && rec.image_data.is_some() {
                            continue;
                        }
                        ui.label(f.field.to_string());
                        ui.label(&f.code);
                        let calculated = f.field == 1 || (rec.record_type == 1 && f.field == 3);
                        let encoding = rec.record_type == 1 && f.field == 15;
                        let binary_compression = (3..=8).contains(&rec.record_type) && f.field == 8;
                        if !writable || calculated || encoding || binary_compression {
                            ui.label(escape_field_value(&f.value))
                                .on_hover_text(if calculated {
                                    "Calculated when you recalculate or save"
                                } else {
                                    "Read-only: original encoding or detected compression"
                                });
                        } else {
                            let key = (record_index, f.field);
                            let mut value = drafts
                                .get(&key)
                                .cloned()
                                .unwrap_or_else(|| escape_field_value(&f.value));
                            let response = ui.add(
                                egui::TextEdit::multiline(&mut value)
                                    .id_salt(key)
                                    .desired_rows(1)
                                    .desired_width(ui.available_width().max(240.0)),
                            );
                            if response.changed() {
                                drafts.insert(key, value);
                                changed = true;
                            }
                        }
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
    changed
}

fn escape_field_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\u{1e}', "\\x1e")
        .replace('\u{1f}', "\\x1f")
}

fn unescape_field_value(value: &str) -> Result<String> {
    let mut chars = value.chars();
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => result.push('\\'),
            Some('x') => {
                let digits: String = chars.by_ref().take(2).collect();
                match digits.to_ascii_lowercase().as_str() {
                    "1e" => result.push('\u{1e}'),
                    "1f" => result.push('\u{1f}'),
                    _ => {
                        anyhow::bail!("Only \\x1e (RS), \\x1f (US), and \\\\ escapes are supported")
                    }
                }
            }
            _ => anyhow::bail!("Incomplete escape: use \\\\ for a literal backslash"),
        }
    }
    Ok(result)
}

/// Replace the destination only after the complete file has been written.
fn write_nist_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("Creating temporary NIST file")?;
    temporary.write_all(bytes).context("Writing NIST file")?;
    if let Ok(metadata) = std::fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())
            .context("Preserving file permissions")?;
    }
    temporary
        .as_file()
        .sync_all()
        .context("Flushing NIST file")?;
    temporary
        .persist(path)
        .with_context(|| format!("Replacing {}", path.display()))?;
    Ok(())
}

/// Loads and parses a NIST file from disk.
fn load_nist_file(path: &std::path::Path) -> Result<(NistFile, u64)> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
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
            (img, format!("{compression} (auto-detected)"), None, w, h)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_fixture() -> LoadedFile {
        let path = PathBuf::from("test_data/type-3.an2");
        let (nist, size) = load_nist_file(&path).unwrap();
        LoadedFile::new(path, nist, size)
    }

    #[test]
    fn field_drafts_keep_separators_and_literal_backslashes_distinct() {
        let original = "A\u{1e}B\u{1f}C\\x1e";
        assert_eq!(
            unescape_field_value(&escape_field_value(original)).unwrap(),
            original
        );
        assert!(unescape_field_value("unfinished\\").is_err());
        assert!(unescape_field_value("bad\\x1d").is_err());
    }

    #[test]
    fn invalid_drafts_leave_loaded_file_and_drafts_intact() {
        let mut file = loaded_fixture();
        file.field_drafts.insert((0, 9), "unfinished\\".into());
        assert!(file.prepare_save().is_err());
        assert_eq!(file.nist.transaction.field_value("TCN"), Some("jck t3"));
        assert_eq!(file.field_drafts[&(0, 9)], "unfinished\\");
    }

    #[test]
    fn save_commits_validated_edits_and_failed_save_retains_them() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("edited.an2");
        let mut file = loaded_fixture();
        std::fs::copy(&file.path, &target).unwrap();
        let original = std::fs::read(&target).unwrap();
        file.path = target.clone();
        file.dirty = true;
        file.field_drafts
            .insert((0, 9), "edited transaction".into());
        let mut app = NistViewerApp::new();
        app.file = Some(file);
        assert!(app.save_nist_file(false));
        let file = app.file.as_ref().unwrap();
        assert!(!file.dirty);
        assert!(file.field_drafts.is_empty());
        assert_eq!(
            file.nist.transaction.field_value("TCN"),
            Some("edited transaction")
        );
        let saved = std::fs::read(&target).unwrap();
        assert_ne!(saved, original);
        let reopened = crate::nist::parser::decode(&saved).unwrap();
        assert!(reopened.warnings.is_empty());
        assert_eq!(
            reopened.transaction.field_value("TCN"),
            Some("edited transaction")
        );

        let file = app.file.as_mut().unwrap();
        file.field_drafts.insert((0, 9), "bad\\x1d".into());
        file.dirty = true;
        assert!(!app.save_nist_file(false));
        assert_eq!(std::fs::read(&target).unwrap(), saved);
        assert!(app.file.as_ref().unwrap().dirty);
        assert!(!app.file.as_ref().unwrap().field_drafts.is_empty());
    }

    #[test]
    fn failed_replacement_preserves_destination_and_cleans_up_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("destination");
        std::fs::create_dir(&target).unwrap();
        let original = target.join("keep.txt");
        std::fs::write(&original, b"keep").unwrap();
        assert!(write_nist_file(&target, b"new bytes").is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn field_table_renders_in_a_headless_egui_frame() {
        let mut app = NistViewerApp::new();
        app.file = Some(loaded_fixture());
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| app.render_selected_fields(ui));
        });
        output.drop_without_applying_deltas();
    }
}
