#![allow(
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cognitive_complexity
)]

use crate::{EfxTypeInfo, GuiParamEvent, PluginParamDef, ZfxMainThread};
use clack_extensions::gui::{GuiConfiguration, GuiSize, PluginGuiImpl, Window};
use clack_plugin::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

struct ClapWindowWrapper(u64);

impl raw_window_handle::HasWindowHandle for ClapWindowWrapper {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        #[cfg(target_os = "linux")]
        {
            let handle = raw_window_handle::XlibWindowHandle::new(self.0 as core::ffi::c_ulong);
            let raw = raw_window_handle::RawWindowHandle::Xlib(handle);
            // SAFETY: `raw` contains a valid Xlib window handle wrapping `self.0` that outlives this borrow.
            unsafe { Ok(raw_window_handle::WindowHandle::borrow_raw(raw)) }
        }
        #[cfg(target_os = "windows")]
        {
            let non_zero = isize::try_from(self.0)
                .ok()
                .and_then(std::num::NonZeroIsize::new)
                .ok_or(raw_window_handle::HandleError::Unavailable)?;
            let handle = raw_window_handle::Win32WindowHandle::new(non_zero);
            let raw = raw_window_handle::RawWindowHandle::Win32(handle);
            // SAFETY: `raw` contains a valid Win32 HWND handle wrapping `self.0` that outlives this borrow.
            unsafe { Ok(raw_window_handle::WindowHandle::borrow_raw(raw)) }
        }
    }
}

// ---------------------------------------------------------------------------
// Virtual File System (VFS) Asset Lookup Helpers
// ---------------------------------------------------------------------------

#[must_use]
pub fn find_asset(path: &str) -> Option<&'static [u8]> {
    let raw = path.trim();
    if raw.is_empty() {
        return None;
    }

    // Normalize any "\" or ":" to "/" (e.g. "Common:Knob" -> "Common/Knob", "Common:FootSw" -> "Common/FootSw")
    let normalized = raw.replace(['\\', ':'], "/");

    let mut candidates = Vec::new();
    candidates.push(normalized.clone());
    if normalized != raw {
        candidates.push(raw.to_string());
    }

    if let Some(stripped) = normalized.strip_prefix("Common/") {
        candidates.push(stripped.to_string());
    } else {
        candidates.push(format!("Common/{normalized}"));
    }

    for base in candidates {
        // 1. Direct
        if let Some(bytes) = crate::get_asset(&base) {
            return Some(bytes);
        }

        // 2. Extensions
        for ext in &[".png", ".jpg", ".jpeg", ".bmp"] {
            let with_ext = format!("{base}{ext}");
            if let Some(bytes) = crate::get_asset(&with_ext) {
                return Some(bytes);
            }
        }

        // 3. Lowercase
        let lower = base.to_lowercase();
        if let Some(bytes) = crate::get_asset(&lower) {
            return Some(bytes);
        }
        for ext in &[".png", ".jpg", ".jpeg", ".bmp"] {
            let lower_ext = format!("{lower}{ext}");
            if let Some(bytes) = crate::get_asset(&lower_ext) {
                return Some(bytes);
            }
        }
    }

    None
}

#[must_use]
pub fn find_frame_asset(base: &str, index: usize) -> Option<&'static [u8]> {
    let normalized = base.replace(':', "/");
    let mut bases = vec![normalized.clone()];
    if normalized != base {
        bases.push(base.to_string());
    }
    if let Some(stripped) = normalized.strip_prefix("Common/") {
        bases.push(stripped.to_string());
    }

    for b in bases {
        // 2-digit format e.g. Knob00.png, Pedal16.png
        let name_2d = format!("{b}{index:02}.png");
        if let Some(bytes) = find_asset(&name_2d) {
            return Some(bytes);
        }

        // 1-digit format e.g. Knob0.png
        let name_1d = format!("{b}{index}.png");
        if let Some(bytes) = find_asset(&name_1d) {
            return Some(bytes);
        }

        // Lowercase variants e.g. led00.png
        let name_lower_2d = format!("{}{:02}.png", b.to_lowercase(), index);
        if let Some(bytes) = find_asset(&name_lower_2d) {
            return Some(bytes);
        }

        let name_lower_1d = format!("{}{}.png", b.to_lowercase(), index);
        if let Some(bytes) = find_asset(&name_lower_1d) {
            return Some(bytes);
        }
    }

    None
}

#[must_use]
pub fn probe_frame_count(base: &str, declared_max: Option<u32>) -> usize {
    if let Some(m) = declared_max {
        if m > 0 {
            return m as usize;
        }
    }

    let count = (0..256)
        .take_while(|&count| find_frame_asset(base, count).is_some())
        .count();

    count.max(1)
}

#[must_use]
pub fn load_texture(ctx: &egui::Context, name: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?;
    let size = [img.width() as usize, img.height() as usize];
    let rgba = img.to_rgba8();
    let color_image =
        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR))
}

// ---------------------------------------------------------------------------
// Baseview + egui App
// ---------------------------------------------------------------------------

pub struct ZfxApp {
    layout: Option<EfxTypeInfo>,
    param_values: Arc<RwLock<HashMap<u32, f64>>>,
    param_defs: HashMap<u32, PluginParamDef>,
    gui_events: Arc<Mutex<Vec<GuiParamEvent>>>,
    #[cfg(feature = "feat_oversampling")]
    oversampling_mode: Arc<std::sync::atomic::AtomicU8>,
    #[cfg(feature = "feat_oversampling")]
    oversampling_phase: Arc<std::sync::atomic::AtomicU8>,
    textures: HashMap<String, egui::TextureHandle>,
    frame_counts: HashMap<String, usize>,
    bkgd_texture_name: Option<String>,
    active_gestures: HashSet<u32>,
}

impl ZfxApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        layout: Option<EfxTypeInfo>,
        param_values: Arc<RwLock<HashMap<u32, f64>>>,
        param_defs_list: Vec<PluginParamDef>,
        gui_events: Arc<Mutex<Vec<GuiParamEvent>>>,
        #[cfg(feature = "feat_oversampling")] oversampling_mode: Arc<std::sync::atomic::AtomicU8>,
        #[cfg(feature = "feat_oversampling")] oversampling_phase: Arc<std::sync::atomic::AtomicU8>,
    ) -> Self {
        let param_defs: HashMap<u32, PluginParamDef> =
            param_defs_list.into_iter().map(|p| (p.id, p)).collect();
        let bkgd_texture_name = layout
            .as_ref()
            .and_then(|l| l.total.bkgd_img.as_ref())
            .and_then(|b| b.img.as_ref())
            .map(|i| i.value.clone())
            .or_else(|| Some("bkgd.jpg".to_string()));
        Self {
            layout,
            param_values,
            param_defs,
            gui_events,
            #[cfg(feature = "feat_oversampling")]
            oversampling_mode,
            #[cfg(feature = "feat_oversampling")]
            oversampling_phase,
            textures: HashMap::new(),
            frame_counts: HashMap::new(),
            bkgd_texture_name,
            active_gestures: HashSet::new(),
        }
    }

    fn get_or_load_frame_texture(
        &mut self,
        ctx: &egui::Context,
        base_name: &str,
        frame_idx: usize,
    ) -> Option<&egui::TextureHandle> {
        let key = format!("{base_name}_{frame_idx}");
        if !self.textures.contains_key(&key) {
            let bytes = find_frame_asset(base_name, frame_idx).or_else(|| find_asset(base_name));

            if let Some(bytes) = bytes {
                if let Some(tex) = load_texture(ctx, &key, bytes) {
                    self.textures.insert(key.clone(), tex);
                }
            }
        }
        self.textures.get(&key)
    }

    fn get_or_load_bkgd_texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if !self.textures.contains_key("bkgd") {
            let mut candidates: Vec<String> = Vec::new();

            if let Some(name) = &self.bkgd_texture_name {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    // 1. Swapped extension first if jpg (PNGs in ZFX assets are pristine whereas some JPGs are truncated)
                    if let Some(stem) = trimmed
                        .strip_suffix(".jpg")
                        .or_else(|| trimmed.strip_suffix(".jpeg"))
                    {
                        candidates.push(format!("{stem}.png"));
                        candidates.push(format!("{}.png", stem.to_lowercase()));
                    }

                    // 2. Exact declared name
                    candidates.push(trimmed.to_string());

                    // 3. Extensions if no extension
                    for ext in &[".png", ".jpg", ".jpeg", ".bmp"] {
                        if !trimmed.to_lowercase().ends_with(ext) {
                            candidates.push(format!("{trimmed}{ext}"));
                        }
                    }

                    // 4. Lowercase variants
                    let lower = trimmed.to_lowercase();
                    candidates.push(lower.clone());
                    for ext in &[".png", ".jpg", ".jpeg", ".bmp"] {
                        if !lower.ends_with(ext) {
                            candidates.push(format!("{lower}{ext}"));
                        }
                    }

                    // 5. Swap png -> jpg
                    if let Some(stem) = trimmed.strip_suffix(".png") {
                        candidates.push(format!("{stem}.jpg"));
                    }
                }
            }

            // Universal fallback candidates
            candidates.push("bkgd.png".to_string());
            candidates.push("Bkgd.png".to_string());
            candidates.push("bkgd.jpg".to_string());
            candidates.push("Bkgd.jpg".to_string());
            candidates.push("bkgd".to_string());
            candidates.push("Bkgd".to_string());

            // Try plugin-name based candidate e.g. JAZZ00.png, Twin.jpg, Twin.png
            let p_name = crate::PLUGIN_NAME.replace(|c: char| !c.is_alphanumeric(), "");
            candidates.push(format!("{p_name}00.png"));
            candidates.push(format!("{p_name}.png"));
            candidates.push(format!("{p_name}.jpg"));

            let loaded = candidates.iter().find_map(|cand| {
                let bytes = find_asset(cand)?;
                let tex = load_texture(ctx, "bkgd", bytes)?;
                Some((cand, tex))
            });
            if let Some((cand, tex)) = loaded {
                eprintln!(
                    "[ZFX GUI] Loaded background texture '{}' (size: {}x{})",
                    cand,
                    tex.size()[0],
                    tex.size()[1]
                );
                self.textures.insert("bkgd".to_string(), tex);
            }
        }
        self.textures.get("bkgd")
    }

    #[cfg(feature = "feat_mic_ui")]
    fn get_or_load_mic_texture(&mut self, ctx: &egui::Context) -> Option<&egui::TextureHandle> {
        if !self.textures.contains_key("mic_tex") {
            let candidates = [
                "thumb",
                "thumb.png",
                "Thumb",
                "Thumb.png",
                "image_1",
                "image_1.png",
                "bkgd",
                "bkgd.png",
                "Bkgd.png",
            ];
            let loaded = candidates.iter().find_map(|&cand| {
                let bytes = find_asset(cand)?;
                let tex = load_texture(ctx, "mic_tex", bytes)?;
                Some((cand, tex))
            });
            if let Some((cand, tex)) = loaded {
                eprintln!(
                    "[ZFX GUI] Loaded mic texture '{}' (size: {}x{})",
                    cand,
                    tex.size()[0],
                    tex.size()[1]
                );
                self.textures.insert("mic_tex".to_string(), tex);
            }
        }
        self.textures.get("mic_tex")
    }

    #[cfg(feature = "feat_oversampling")]
    #[allow(clippy::too_many_lines)]
    fn render_oversampling_badge(&self, ui: &mut egui::Ui, pos: egui::Pos2) {
        let cur_u8 = self.oversampling_mode.load(std::sync::atomic::Ordering::Acquire);
        let cur_mode = crate::resampler::OversamplingMode::from_u8(cur_u8);
        let cur_phase_u8 = self.oversampling_phase.load(std::sync::atomic::Ordering::Acquire);
        let cur_phase = crate::resampler::OversamplingPhase::from_u8(cur_phase_u8);

        let mode_w = 90.0;
        let pill_h = 20.0;
        let mode_rect = egui::Rect::from_min_size(pos, egui::vec2(mode_w, pill_h));

        // 1. Oversampling Mode Pill [ 1x | 2x | 4x ]
        ui.painter().rect_filled(
            mode_rect,
            10.0,
            egui::Color32::from_rgba_premultiplied(16, 17, 20, 220),
        );
        ui.painter().rect_stroke(
            mode_rect,
            10.0,
            egui::Stroke::new(1.0, egui::Color32::from_rgba_premultiplied(65, 70, 82, 200)),
            egui::StrokeKind::Inside,
        );

        let seg_w = mode_w / 3.0;
        let modes = [
            (
                crate::resampler::OversamplingMode::Off,
                "1x",
                "1x: Native Rate (0 samples latency)",
            ),
            (
                crate::resampler::OversamplingMode::Double,
                "2x",
                match cur_phase {
                    crate::resampler::OversamplingPhase::Linear => {
                        "2x: Linear-Phase FIR (15 samples PDC)"
                    }
                    crate::resampler::OversamplingPhase::Minimum => {
                        "2x: Minimum-Phase FIR (2 samples PDC, zero pre-ringing)"
                    }
                },
            ),
            (
                crate::resampler::OversamplingMode::Quad,
                "4x",
                match cur_phase {
                    crate::resampler::OversamplingPhase::Linear => {
                        "4x: Linear-Phase FIR (23 samples PDC)"
                    }
                    crate::resampler::OversamplingPhase::Minimum => {
                        "4x: Minimum-Phase FIR (3 samples PDC, zero pre-ringing)"
                    }
                },
            ),
        ];

        for (idx, (m, label, tooltip)) in modes.iter().enumerate() {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let seg_rect = egui::Rect::from_min_size(
                egui::pos2(
                    (idx as f32).mul_add(seg_w, mode_rect.left()),
                    mode_rect.top(),
                ),
                egui::vec2(seg_w, pill_h),
            );
            let resp = ui.allocate_rect(seg_rect, egui::Sense::click());
            let is_selected = cur_mode == *m;
            let is_hovered = resp.hovered();

            if resp.clicked() && !is_selected {
                let mode_val = f64::from(m.to_u8());
                self.oversampling_mode.store(m.to_u8(), std::sync::atomic::Ordering::Release);
                if let Ok(mut values) = self.param_values.write() {
                    values.insert(crate::PARAM_OVERSAMPLING, mode_val);
                }
                if let Ok(mut evs) = self.gui_events.lock() {
                    evs.push(GuiParamEvent::BeginGesture(crate::PARAM_OVERSAMPLING));
                    evs.push(GuiParamEvent::SetValue(crate::PARAM_OVERSAMPLING, mode_val));
                    evs.push(GuiParamEvent::EndGesture(crate::PARAM_OVERSAMPLING));
                }
            }

            resp.on_hover_ui(|ui| {
                ui.label(*tooltip);
            });

            if is_selected {
                ui.painter().rect_filled(
                    seg_rect.shrink(1.5),
                    8.0,
                    egui::Color32::from_rgb(230, 165, 45),
                );
                ui.painter().text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(11.0),
                    egui::Color32::from_rgb(20, 20, 24),
                );
            } else {
                let text_color = if is_hovered {
                    egui::Color32::from_rgb(220, 225, 235)
                } else {
                    egui::Color32::from_rgb(140, 145, 155)
                };
                ui.painter().text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(10.5),
                    text_color,
                );
            }
        }

        // 2. Oversampling Phase Pill [ Lin | Min ]
        let phase_w = 70.0;
        let phase_rect = egui::Rect::from_min_size(
            egui::pos2(mode_rect.right() + 6.0, mode_rect.top()),
            egui::vec2(phase_w, pill_h),
        );

        let is_active = cur_mode != crate::resampler::OversamplingMode::Off;
        let bg_color = if is_active {
            egui::Color32::from_rgba_premultiplied(16, 17, 20, 220)
        } else {
            egui::Color32::from_rgba_premultiplied(16, 17, 20, 140)
        };
        let border_color = if is_active {
            egui::Color32::from_rgba_premultiplied(65, 70, 82, 200)
        } else {
            egui::Color32::from_rgba_premultiplied(50, 52, 60, 120)
        };

        ui.painter().rect_filled(phase_rect, 10.0, bg_color);
        ui.painter().rect_stroke(
            phase_rect,
            10.0,
            egui::Stroke::new(1.0, border_color),
            egui::StrokeKind::Inside,
        );

        let phase_seg_w = phase_w / 2.0;
        let phases = [
            (
                crate::resampler::OversamplingPhase::Linear,
                "Lin",
                "Linear Phase: Flat group delay, symmetric Kaiser FIR (15/23 smp PDC). Best for multi-mic phase alignment.",
            ),
            (
                crate::resampler::OversamplingPhase::Minimum,
                "Min",
                "Minimum Phase: Zero pre-ringing, ultra-low latency (2/3 smp PDC). Best for sharp pick attack & live tracking.",
            ),
        ];

        for (idx, (p, label, tooltip)) in phases.iter().enumerate() {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let seg_rect = egui::Rect::from_min_size(
                egui::pos2(
                    (idx as f32).mul_add(phase_seg_w, phase_rect.left()),
                    phase_rect.top(),
                ),
                egui::vec2(phase_seg_w, pill_h),
            );
            let resp = ui.allocate_rect(seg_rect, egui::Sense::click());
            let is_selected = cur_phase == *p;
            let is_hovered = resp.hovered();

            if resp.clicked() && !is_selected {
                let phase_val = f64::from(p.to_u8());
                self.oversampling_phase.store(p.to_u8(), std::sync::atomic::Ordering::Release);
                if let Ok(mut values) = self.param_values.write() {
                    values.insert(crate::PARAM_OVERSAMPLING_METHOD, phase_val);
                }
                if let Ok(mut evs) = self.gui_events.lock() {
                    evs.push(GuiParamEvent::BeginGesture(crate::PARAM_OVERSAMPLING_METHOD));
                    evs.push(GuiParamEvent::SetValue(crate::PARAM_OVERSAMPLING_METHOD, phase_val));
                    evs.push(GuiParamEvent::EndGesture(crate::PARAM_OVERSAMPLING_METHOD));
                }
            }

            resp.on_hover_ui(|ui| {
                ui.label(*tooltip);
            });

            if is_selected {
                let badge_color = if is_active {
                    egui::Color32::from_rgb(230, 165, 45)
                } else {
                    egui::Color32::from_rgb(160, 125, 55)
                };
                ui.painter()
                    .rect_filled(seg_rect.shrink(1.5), 8.0, badge_color);
                ui.painter().text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(10.5),
                    egui::Color32::from_rgb(20, 20, 24),
                );
            } else {
                let text_color = if is_hovered {
                    egui::Color32::from_rgb(220, 225, 235)
                } else if is_active {
                    egui::Color32::from_rgb(140, 145, 155)
                } else {
                    egui::Color32::from_rgb(90, 95, 105)
                };
                ui.painter().text(
                    seg_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(10.0),
                    text_color,
                );
            }
        }
    }
}

#[must_use]
pub fn get_window_size(layout: Option<&EfxTypeInfo>) -> (f64, f64) {
    let cat = crate::PLUGIN_CATEGORY.to_lowercase();
    if cat == "cabinet" || layout.is_some_and(|l| l.total.width > 1000.0) {
        let w = crate::NATIVE_BKGD_WIDTH.unwrap_or(430.0);
        let h = crate::NATIVE_BKGD_HEIGHT.unwrap_or(570.0);
        (w, h)
    } else if cfg!(feature = "feat_mic_ui")
        && (cat == "mic" || layout.is_some_and(|l| l.total.width <= 100.0))
    {
        (400.0, 440.0)
    } else if let Some(layout) = layout {
        (
            f64::from(layout.total.width),
            f64::from(layout.total.height),
        )
    } else {
        (800.0, 600.0)
    }
}

impl egui_baseview::App for ZfxApp {
    fn build(
        &mut self,
        _egui_ctx: egui::Context,
        _frame: &mut egui_baseview::Frame,
    ) -> Result<(), baseview::HandlerError> {
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut egui_baseview::Frame) {
        let ctx = ui.ctx().clone();

        // 30 FPS repaint timer so host parameter automation smoothly animates controls even when mouse is still
        ctx.request_repaint_after(std::time::Duration::from_millis(33));

        // Fill background to avoid X11 expose gaps
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, egui::Color32::from_rgb(26, 26, 28));

        let cat = crate::PLUGIN_CATEGORY.to_lowercase();
        let has_controls = self
            .layout
            .as_ref()
            .is_some_and(|l| l.ctrls.as_ref().is_some_and(|c| !c.ctrl.is_empty()));
        let is_cabinet =
            (cat == "cabinet" || self.layout.as_ref().is_some_and(|l| l.total.width > 1000.0))
                && !has_controls;
        #[cfg(feature = "feat_mic_ui")]
        let is_mic = cat == "mic" || self.layout.as_ref().is_some_and(|l| l.total.width <= 100.0);

        if is_cabinet {
            let (win_width, win_height) = get_window_size(self.layout.as_ref());
            let win_w = win_width as f32;
            let win_h = win_height as f32;
            let screen_rect =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(win_w, win_h));
            ui.allocate_rect(screen_rect, egui::Sense::hover());

            // Edge-to-edge native cabinet image rendering (1:1 with authentic artwork)
            if let Some(bkgd_tex) = self.get_or_load_bkgd_texture(&ctx) {
                ui.painter().image(
                    bkgd_tex.id(),
                    screen_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            #[cfg(feature = "feat_oversampling")]
            self.render_oversampling_badge(ui, egui::pos2(win_w - 176.0, 10.0));
            return;
        }

        #[cfg(feature = "feat_mic_ui")]
        if is_mic {
            let (win_width, win_height) = get_window_size(self.layout.as_ref());
            let win_w = win_width as f32;
            let win_h = win_height as f32;
            let screen_rect =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(win_w, win_h));
            ui.allocate_rect(screen_rect, egui::Sense::hover());

            let mut param_updates: Vec<(u32, f64)> = Vec::new();
            let mut new_gui_events: Vec<GuiParamEvent> = Vec::new();

            // Studio faceplate chassis
            ui.painter()
                .rect_filled(screen_rect, 0.0, egui::Color32::from_rgb(22, 23, 26));

            // Top Header: Sleek faceplate header
            let header_rect =
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(win_w, 50.0));
            ui.painter()
                .rect_filled(header_rect, 0.0, egui::Color32::from_rgb(30, 31, 36));
            ui.painter().line_segment(
                [egui::pos2(0.0, 50.0), egui::pos2(win_w, 50.0)],
                egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 52, 60)),
            );

            let display_name = crate::PLUGIN_NAME.replace('_', " ");
            ui.painter().text(
                egui::pos2(16.0, 14.0),
                egui::Align2::LEFT_TOP,
                &display_name,
                egui::FontId::proportional(15.0),
                egui::Color32::from_rgb(225, 230, 240),
            );
            ui.painter().text(
                egui::pos2(16.0, 32.0),
                egui::Align2::LEFT_TOP,
                "ZOOM ZFX STUDIO MICROPHONE",
                egui::FontId::proportional(9.5),
                egui::Color32::from_rgb(130, 135, 145),
            );

            // Bypass Button (prm = 0)
            let cur_bypass = {
                let values = self.param_values.read().unwrap();
                values.get(&0).copied().unwrap_or(0.0)
            };
            let is_bypassed = cur_bypass >= 0.5;
            let bypass_btn_rect =
                egui::Rect::from_min_size(egui::pos2(win_w - 85.0, 13.0), egui::vec2(70.0, 24.0));
            let bypass_resp = ui.allocate_rect(bypass_btn_rect, egui::Sense::click());
            if bypass_resp.clicked() {
                let new_byp = if is_bypassed { 0.0 } else { 1.0 };
                new_gui_events.push(GuiParamEvent::BeginGesture(0));
                param_updates.push((0, new_byp));
                new_gui_events.push(GuiParamEvent::SetValue(0, new_byp));
                new_gui_events.push(GuiParamEvent::EndGesture(0));
            }
            let (background_color, border_color, status_text) = if is_bypassed {
                (
                    egui::Color32::from_rgb(55, 30, 30),
                    egui::Color32::from_rgb(230, 80, 80),
                    "BYPASS",
                )
            } else {
                (
                    egui::Color32::from_rgb(28, 55, 35),
                    egui::Color32::from_rgb(80, 220, 110),
                    "ACTIVE",
                )
            };
            ui.painter()
                .rect_filled(bypass_btn_rect, 4.0, background_color);
            ui.painter().rect_stroke(
                bypass_btn_rect,
                4.0,
                egui::Stroke::new(1.0, border_color),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                bypass_btn_rect.center(),
                egui::Align2::CENTER_CENTER,
                status_text,
                egui::FontId::proportional(11.0),
                border_color,
            );

            // Read parameters:
            // prm 1 = RoomSize (0..100)
            // prm 2 = Sideways / Position (0..100, default 50 = Center On-Axis)
            let (cur_room, cur_sideways) = {
                let values = self.param_values.read().unwrap();
                (
                    values.get(&1).copied().unwrap_or(0.0),
                    values.get(&2).copied().unwrap_or(50.0),
                )
            };

            // Stage Area: Speaker Cone & Horizontal Microphone Track
            let stage_rect =
                egui::Rect::from_min_size(egui::pos2(15.0, 60.0), egui::vec2(win_w - 30.0, 230.0));
            let stage_resp = ui.allocate_rect(stage_rect, egui::Sense::click_and_drag());

            // Stage background & recessed bezel
            ui.painter()
                .rect_filled(stage_rect, 6.0, egui::Color32::from_rgb(15, 16, 18));
            ui.painter().rect_stroke(
                stage_rect,
                6.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(42, 44, 50)),
                egui::StrokeKind::Inside,
            );

            let cone_center = egui::pos2(win_w * 0.5, stage_rect.top() + 100.0);
            let cone_radius = 80.0;

            // Speaker Cone Drawing: Concentric rib rings and dust cap
            ui.painter().circle_stroke(
                cone_center,
                cone_radius,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(45, 47, 55)),
            );
            ui.painter().circle_stroke(
                cone_center,
                cone_radius * 0.72,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(32, 34, 40)),
            );
            ui.painter().circle_stroke(
                cone_center,
                cone_radius * 0.45,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(36, 38, 45)),
            );
            ui.painter().circle_filled(
                cone_center,
                cone_radius * 0.25,
                egui::Color32::from_rgb(24, 25, 30),
            );
            ui.painter().circle_stroke(
                cone_center,
                cone_radius * 0.25,
                egui::Stroke::new(1.5, egui::Color32::from_rgb(52, 55, 65)),
            );

            // Axis Center Line & On-Axis Label
            ui.painter().line_segment(
                [
                    egui::pos2(cone_center.x, stage_rect.top() + 8.0),
                    egui::pos2(cone_center.x, stage_rect.bottom() - 36.0),
                ],
                egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(85, 95, 115, 75)),
            );
            ui.painter().text(
                egui::pos2(cone_center.x, stage_rect.top() + 10.0),
                egui::Align2::CENTER_TOP,
                "ON-AXIS CENTER",
                egui::FontId::proportional(8.5),
                egui::Color32::from_rgb(100, 105, 120),
            );

            // Sideways Travel Track: 0% at left edge, 50% at center, 100% at right edge
            let track_margin = 35.0;
            let track_left = stage_rect.left() + track_margin;
            let track_right = stage_rect.right() - track_margin;
            let track_w = track_right - track_left;

            // Interactive Drag/Click on the Speaker Stage for Sideways (prm = 2)
            if stage_resp.drag_started() {
                self.active_gestures.insert(2);
                new_gui_events.push(GuiParamEvent::BeginGesture(2));
            }
            if stage_resp.dragged() || stage_resp.clicked() {
                if let Some(pos) = stage_resp.interact_pointer_pos() {
                    let norm = ((pos.x - track_left) / track_w).clamp(0.0, 1.0);
                    let new_sideways = f64::from(norm * 100.0);
                    param_updates.push((2, new_sideways));
                    new_gui_events.push(GuiParamEvent::SetValue(2, new_sideways));
                }
            }
            if stage_resp.drag_stopped() {
                self.active_gestures.remove(&2);
                new_gui_events.push(GuiParamEvent::EndGesture(2));
            }

            // Microphone Placement at cur_sideways
            let mic_x =
                (cur_sideways.clamp(0.0, 100.0) as f32 / 100.0).mul_add(track_w, track_left);

            // Render authentic microphone graphic if available
            let mic_tex = self.get_or_load_mic_texture(&ctx);
            if let Some(tex) = mic_tex {
                let tex_sz = tex.size_vec2();
                let mic_render_h = 125.0;
                let mic_render_w = ((tex_sz.x / tex_sz.y) * mic_render_h).clamp(24.0, 60.0);
                let mic_center_y = stage_rect.top() + 115.0;
                let mic_rect = egui::Rect::from_center_size(
                    egui::pos2(mic_x, mic_center_y),
                    egui::vec2(mic_render_w, mic_render_h),
                );

                // Soft shadow behind microphone
                ui.painter().rect_filled(
                    mic_rect.expand(2.0),
                    3.0,
                    egui::Color32::from_black_alpha(100),
                );

                ui.painter().image(
                    tex.id(),
                    mic_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }

            // Capsule crosshair & target reticle at speaker center Y
            let capsule_y = cone_center.y;
            let capsule_pos = egui::pos2(mic_x, capsule_y);
            ui.painter().circle_stroke(
                capsule_pos,
                11.0,
                egui::Stroke::new(1.5, egui::Color32::from_rgb(255, 185, 40)),
            );
            ui.painter().circle_filled(
                capsule_pos,
                11.0,
                egui::Color32::from_rgba_unmultiplied(255, 185, 40, 35),
            );
            ui.painter()
                .circle_filled(capsule_pos, 3.5, egui::Color32::from_rgb(255, 220, 100));

            // Position Readout
            let offset_from_center = cur_sideways - 50.0;
            let pos_desc = if offset_from_center.abs() < 4.0 {
                "On-Axis Center".to_string()
            } else if offset_from_center < 0.0 {
                format!("{:.0}% Left Off-Axis", offset_from_center.abs() * 2.0)
            } else {
                format!("{:.0}% Right Off-Axis", offset_from_center * 2.0)
            };
            ui.painter().text(
                egui::pos2(win_w * 0.5, stage_rect.bottom() - 20.0),
                egui::Align2::CENTER_CENTER,
                format!("Position: {cur_sideways:.1}% ({pos_desc})"),
                egui::FontId::proportional(11.0),
                egui::Color32::from_rgb(210, 215, 225),
            );

            // Room Size / Ambience Control Box (prm = 1)
            let room_box =
                egui::Rect::from_min_size(egui::pos2(15.0, 302.0), egui::vec2(win_w - 30.0, 120.0));
            ui.painter()
                .rect_filled(room_box, 6.0, egui::Color32::from_rgb(24, 25, 30));
            ui.painter().rect_stroke(
                room_box,
                6.0,
                egui::Stroke::new(1.0, egui::Color32::from_rgb(44, 46, 54)),
                egui::StrokeKind::Inside,
            );

            // Room Ambience Header & Readout
            ui.painter().text(
                egui::pos2(room_box.left() + 16.0, room_box.top() + 14.0),
                egui::Align2::LEFT_TOP,
                "ROOM ACOUSTIC AMBIENCE",
                egui::FontId::proportional(11.5),
                egui::Color32::from_rgb(170, 175, 185),
            );
            ui.painter().text(
                egui::pos2(room_box.right() - 16.0, room_box.top() + 14.0),
                egui::Align2::RIGHT_TOP,
                format!("{cur_room:.1}%"),
                egui::FontId::proportional(13.0),
                egui::Color32::from_rgb(240, 195, 75),
            );
            ui.painter().text(
                egui::pos2(room_box.left() + 16.0, room_box.top() + 32.0),
                egui::Align2::LEFT_TOP,
                "Early room reflections & boundary acoustic dimension",
                egui::FontId::proportional(9.5),
                egui::Color32::from_rgb(115, 120, 130),
            );

            // Interactive horizontal slider for RoomSize (prm = 1)
            let slider_track_rect = egui::Rect::from_min_size(
                egui::pos2(room_box.left() + 16.0, room_box.top() + 66.0),
                egui::vec2(room_box.width() - 32.0, 20.0),
            );
            let slider_resp = ui.allocate_rect(slider_track_rect, egui::Sense::click_and_drag());
            let track_center_y = slider_track_rect.center().y;

            if slider_resp.drag_started() {
                self.active_gestures.insert(1);
                new_gui_events.push(GuiParamEvent::BeginGesture(1));
            }
            if slider_resp.dragged() || slider_resp.clicked() {
                if let Some(pos) = slider_resp.interact_pointer_pos() {
                    let norm = ((pos.x - slider_track_rect.left()) / slider_track_rect.width())
                        .clamp(0.0, 1.0);
                    let new_room = f64::from(norm * 100.0);
                    param_updates.push((1, new_room));
                    new_gui_events.push(GuiParamEvent::SetValue(1, new_room));
                }
            }
            if slider_resp.drag_stopped() {
                self.active_gestures.remove(&1);
                new_gui_events.push(GuiParamEvent::EndGesture(1));
            }

            // Slider track line (inactive background)
            ui.painter().line_segment(
                [
                    egui::pos2(slider_track_rect.left(), track_center_y),
                    egui::pos2(slider_track_rect.right(), track_center_y),
                ],
                egui::Stroke::new(5.0, egui::Color32::from_rgb(38, 40, 48)),
            );

            // Active filled portion
            let thumb_x = (cur_room.clamp(0.0, 100.0) as f32 / 100.0)
                .mul_add(slider_track_rect.width(), slider_track_rect.left());
            ui.painter().line_segment(
                [
                    egui::pos2(slider_track_rect.left(), track_center_y),
                    egui::pos2(thumb_x, track_center_y),
                ],
                egui::Stroke::new(5.0, egui::Color32::from_rgb(230, 165, 45)),
            );

            // Metallic thumb handle
            ui.painter().circle_filled(
                egui::pos2(thumb_x, track_center_y),
                8.5,
                egui::Color32::from_rgb(235, 235, 240),
            );
            ui.painter().circle_stroke(
                egui::pos2(thumb_x, track_center_y),
                8.5,
                egui::Stroke::new(1.5, egui::Color32::from_rgb(230, 165, 45)),
            );
            ui.painter().circle_filled(
                egui::pos2(thumb_x, track_center_y),
                2.5,
                egui::Color32::from_rgb(50, 52, 60),
            );

            // Slider scale labels
            ui.painter().text(
                egui::pos2(slider_track_rect.left(), slider_track_rect.bottom() + 4.0),
                egui::Align2::LEFT_TOP,
                "0% (Dry)",
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgb(110, 115, 125),
            );
            ui.painter().text(
                egui::pos2(
                    slider_track_rect.center().x,
                    slider_track_rect.bottom() + 4.0,
                ),
                egui::Align2::CENTER_TOP,
                "50% (Studio)",
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgb(110, 115, 125),
            );
            ui.painter().text(
                egui::pos2(slider_track_rect.right(), slider_track_rect.bottom() + 4.0),
                egui::Align2::RIGHT_TOP,
                "100% (Hall)",
                egui::FontId::proportional(9.0),
                egui::Color32::from_rgb(110, 115, 125),
            );

            if !param_updates.is_empty() {
                let mut values = self.param_values.write().unwrap();
                for (id, val) in param_updates {
                    values.insert(id, val);
                }
            }
            if !new_gui_events.is_empty() {
                let mut events_lock = self.gui_events.lock().unwrap();
                events_lock.extend(new_gui_events);
            }

            #[cfg(feature = "feat_oversampling")]
            self.render_oversampling_badge(ui, egui::pos2(win_w - 188.0, 15.0));

            return;
        }

        let (win_width, win_height) = get_window_size(self.layout.as_ref());
        let win_w = win_width as f32;
        let win_h = win_height as f32;

        let screen_rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(win_w, win_h));

        ui.allocate_rect(screen_rect, egui::Sense::hover());

        // Paint background image plate
        if let Some(bkgd_tex) = self.get_or_load_bkgd_texture(&ctx) {
            ui.painter().image(
                bkgd_tex.id(),
                screen_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            ui.painter()
                .rect_filled(screen_rect, 0.0, egui::Color32::from_rgb(30, 30, 34));
        }

        // Render controls
        let ctrls_opt = self.layout.as_ref().and_then(|l| l.ctrls.clone());
        if let Some(ctrls) = ctrls_opt {
            let mut param_updates: Vec<(u32, f64)> = Vec::new();
            let mut new_gui_events: Vec<GuiParamEvent> = Vec::new();

            for ctrl in &ctrls.ctrl {
                let prm = ctrl.prm;
                let prm_def = self.param_defs.get(&prm);
                let min_val = prm_def.map_or(0.0, |p| p.min_value);
                let max_val = prm_def
                    .map_or_else(|| f64::from(ctrl.max.unwrap_or(100)), |p| p.max_value)
                    .max(min_val + 1e-4);
                let def_val =
                    prm_def.map_or_else(|| min_val.midpoint(max_val), |p| p.default_value);
                let param_name = prm_def
                    .map(|p| p.name.trim_end_matches('\0'))
                    .or(ctrl.name.as_deref())
                    .unwrap_or("Parameter");

                // Read current parameter value
                let cur_val = {
                    let values = self.param_values.read().unwrap();
                    values.get(&prm).copied().unwrap_or(def_val)
                };
                let norm = ((cur_val - min_val) / (max_val - min_val)).clamp(0.0, 1.0);

                let total_frames = *self
                    .frame_counts
                    .entry(ctrl.img.clone())
                    .or_insert_with(|| probe_frame_count(&ctrl.img, ctrl.max));

                let frame_idx = if total_frames <= 1 {
                    0
                } else {
                    ((norm * (total_frames - 1) as f64).round() as usize).min(total_frames - 1)
                };

                let ctrl_rect = egui::Rect::from_min_size(
                    egui::pos2(ctrl.pos_x, ctrl.pos_y),
                    egui::vec2(ctrl.width, ctrl.height),
                );

                let type_name = ctrl.type_tag.as_str();

                match type_name {
                    "Knob" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click_and_drag());

                        if response.drag_started() && self.active_gestures.insert(prm) {
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                        }

                        if response.dragged() {
                            if self.active_gestures.insert(prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            let drag_delta = f64::from(-response.drag_delta().y);
                            let speed = if ui.input(|i| i.modifiers.shift) {
                                0.1
                            } else {
                                0.6
                            };
                            let delta = drag_delta * speed * ((max_val - min_val) / 100.0);
                            let new_val = (cur_val + delta).clamp(min_val, max_val);
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                        }

                        if response.drag_stopped() && self.active_gestures.remove(&prm) {
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        if response.double_clicked() {
                            if !self.active_gestures.contains(&prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            param_updates.push((prm, def_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, def_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                            self.active_gestures.remove(&prm);
                        }

                        if response.hovered() {
                            let scroll_y = f64::from(ui.input(|i| i.smooth_scroll_delta.y));
                            if scroll_y.abs() > 0.01 {
                                let step_scale = if ui.input(|i| i.modifiers.shift) {
                                    0.005
                                } else {
                                    0.02
                                };
                                let step = scroll_y.signum() * step_scale * (max_val - min_val);
                                let new_val = (cur_val + step).clamp(min_val, max_val);
                                if !self.active_gestures.contains(&prm) {
                                    new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                                }
                                param_updates.push((prm, new_val));
                                new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                                new_gui_events.push(GuiParamEvent::EndGesture(prm));
                                self.active_gestures.remove(&prm);
                            }
                        }

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{param_name}: {cur_val:.1}"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, frame_idx)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "Slider" | "LedSlider" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click_and_drag());
                        let is_vertical = ctrl.height >= ctrl.width;
                        let sl_w = ctrl.sl_width.unwrap_or(ctrl.width);
                        let sl_h =
                            ctrl.sl_height
                                .unwrap_or(if is_vertical { 20.0 } else { ctrl.height });

                        if response.drag_started() && self.active_gestures.insert(prm) {
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                        }

                        if response.dragged() {
                            if self.active_gestures.insert(prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            let speed = if ui.input(|i| i.modifiers.shift) {
                                0.15
                            } else {
                                1.0
                            };
                            let delta = if is_vertical {
                                let drag_delta = f64::from(-response.drag_delta().y);
                                let track_len = f64::from((ctrl.height - sl_h).max(10.0));
                                drag_delta * speed * ((max_val - min_val) / track_len)
                            } else {
                                let drag_delta = f64::from(response.drag_delta().x);
                                let track_len = f64::from((ctrl.width - sl_w).max(10.0));
                                drag_delta * speed * ((max_val - min_val) / track_len)
                            };
                            let new_val = (cur_val + delta).clamp(min_val, max_val);
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                        }

                        if response.drag_stopped() && self.active_gestures.remove(&prm) {
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        if response.double_clicked() {
                            if !self.active_gestures.contains(&prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            param_updates.push((prm, def_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, def_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                            self.active_gestures.remove(&prm);
                        }

                        if response.hovered() {
                            let scroll_y = f64::from(ui.input(|i| i.smooth_scroll_delta.y));
                            if scroll_y.abs() > 0.01 {
                                let step_scale = if ui.input(|i| i.modifiers.shift) {
                                    0.005
                                } else {
                                    0.02
                                };
                                let step = scroll_y.signum() * step_scale * (max_val - min_val);
                                let new_val = (cur_val + step).clamp(min_val, max_val);
                                if !self.active_gestures.contains(&prm) {
                                    new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                                }
                                param_updates.push((prm, new_val));
                                new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                                new_gui_events.push(GuiParamEvent::EndGesture(prm));
                                self.active_gestures.remove(&prm);
                            }
                        }

                        let response = response.on_hover_ui(|ui| {
                            ui.label(format!("{param_name}: {cur_val:.1}"));
                        });

                        // Compute thumb position along track
                        let thumb_rect = if is_vertical {
                            let track_len = (ctrl.height - sl_h).max(0.0);
                            let thumb_y = (1.0 - norm as f32).mul_add(track_len, ctrl.pos_y);
                            let thumb_x = (ctrl.width - sl_w).mul_add(0.5, ctrl.pos_x);
                            egui::Rect::from_min_size(
                                egui::pos2(thumb_x, thumb_y),
                                egui::vec2(sl_w, sl_h),
                            )
                        } else {
                            let track_len = (ctrl.width - sl_w).max(0.0);
                            let thumb_x = (norm as f32).mul_add(track_len, ctrl.pos_x);
                            let thumb_y = (ctrl.height - sl_h).mul_add(0.5, ctrl.pos_y);
                            egui::Rect::from_min_size(
                                egui::pos2(thumb_x, thumb_y),
                                egui::vec2(sl_w, sl_h),
                            )
                        };

                        let fader_frame = if total_frames <= 1 {
                            0
                        } else if total_frames == 2 {
                            usize::from(response.dragged() || response.hovered())
                        } else {
                            frame_idx
                        };

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, fader_frame)
                        {
                            ui.painter().image(
                                tex.id(),
                                thumb_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "PushKey" | "Toggle" => {
                        let is_sub = ctrl.sub_ctrl == Some(1);
                        let response = if is_sub {
                            ui.allocate_rect(ctrl_rect, egui::Sense::hover())
                        } else {
                            ui.allocate_rect(ctrl_rect, egui::Sense::click())
                        };

                        let is_on = ctrl.param_of_index.map_or_else(
                            || cur_val >= min_val.midpoint(max_val),
                            |target_idx| (cur_val.round() as u32) == target_idx,
                        );

                        if !is_sub && response.clicked() {
                            let new_val = ctrl.param_of_index.map_or_else(
                                || if is_on { min_val } else { max_val },
                                f64::from,
                            );
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        let state_frame = if total_frames <= 1 {
                            0
                        } else if total_frames == 2 {
                            usize::from(is_on)
                        } else {
                            frame_idx
                        };

                        response.on_hover_ui(|ui| {
                            let status = if ctrl.param_of_index.is_some() {
                                if is_on {
                                    "Selected"
                                } else {
                                    "Inactive"
                                }
                            } else if is_on {
                                "ON"
                            } else {
                                "OFF"
                            };
                            ui.label(format!("{param_name}: {status}"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, state_frame)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "Tap" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click());
                        if response.clicked() {
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            param_updates.push((prm, max_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, max_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        let tap_frame =
                            usize::from(response.is_pointer_button_down_on() && total_frames > 1);

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{param_name}: Tap"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, tap_frame)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "ImgKey" | "ImgPrm" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click());
                        let steps = total_frames.max(2);
                        let cur_step =
                            ((norm * (steps - 1) as f64).round() as usize).min(steps - 1);

                        if response.clicked() {
                            let next_step = (cur_step + 1) % steps;
                            let new_val = (next_step as f64 / (steps - 1) as f64)
                                .mul_add(max_val - min_val, min_val);
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        } else if response.secondary_clicked() {
                            let prev_step = (cur_step + steps - 1) % steps;
                            let new_val = (prev_step as f64 / (steps - 1) as f64)
                                .mul_add(max_val - min_val, min_val);
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{}: Mode {}/{}", param_name, cur_step + 1, steps));
                        });

                        if let Some(tex) = self.get_or_load_frame_texture(&ctx, &ctrl.img, cur_step)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "IncKey" | "NInputKey" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click());
                        if response.clicked() {
                            let delta = ctrl.delta.map_or(1.0, f64::from);
                            let mut next_val = cur_val + delta;
                            if next_val > max_val {
                                next_val = min_val;
                            } else if next_val < min_val {
                                next_val = max_val;
                            }
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            param_updates.push((prm, next_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, next_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        let inc_frame =
                            usize::from(response.is_pointer_button_down_on() && total_frames > 1);

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{param_name}: {cur_val:.1}"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, inc_frame)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "Mic" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::drag());

                        if response.drag_started() && self.active_gestures.insert(prm) {
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                        }

                        if response.dragged() {
                            if self.active_gestures.insert(prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            let delta_x = f64::from(response.drag_delta().x)
                                * 0.5
                                * ((max_val - min_val) / 100.0);
                            let new_val = (cur_val + delta_x).clamp(min_val, max_val);
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                        }

                        if response.drag_stopped() && self.active_gestures.remove(&prm) {
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{}: {:.1}%", param_name, norm * 100.0));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, frame_idx)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    "Led" | "LedT" => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::hover());
                        let is_on = ctrl.param_of_index.map_or_else(
                            || cur_val >= min_val.midpoint(max_val),
                            |target_idx| (cur_val.round() as u32) == target_idx,
                        );

                        let led_frame = ctrl.param_of_index.map_or_else(
                            || {
                                if total_frames == 2 {
                                    usize::from(is_on)
                                } else {
                                    frame_idx
                                }
                            },
                            |target_idx| {
                                let is_active = (cur_val.round() as u32) == target_idx;
                                if total_frames <= 1 {
                                    0
                                } else {
                                    usize::from(is_active)
                                }
                            },
                        );

                        response.on_hover_ui(|ui| {
                            let status = if ctrl.param_of_index.is_some() {
                                if is_on {
                                    "Active"
                                } else {
                                    "Inactive"
                                }
                            } else {
                                &format!("{cur_val:.1}")
                            };
                            ui.label(format!("{param_name}: {status}"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, led_frame)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }

                    _ => {
                        let response = ui.allocate_rect(ctrl_rect, egui::Sense::click_and_drag());

                        if response.drag_started() && self.active_gestures.insert(prm) {
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                        }

                        if response.dragged() {
                            if self.active_gestures.insert(prm) {
                                new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            }
                            let drag_delta = f64::from(-response.drag_delta().y);
                            let speed = if ui.input(|i| i.modifiers.shift) {
                                0.1
                            } else {
                                0.6
                            };
                            let delta = drag_delta * speed * ((max_val - min_val) / 100.0);
                            let new_val = (cur_val + delta).clamp(min_val, max_val);
                            param_updates.push((prm, new_val));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, new_val));
                        }

                        if response.drag_stopped() && self.active_gestures.remove(&prm) {
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        if response.double_clicked() {
                            param_updates.push((prm, def_val));
                            new_gui_events.push(GuiParamEvent::BeginGesture(prm));
                            new_gui_events.push(GuiParamEvent::SetValue(prm, def_val));
                            new_gui_events.push(GuiParamEvent::EndGesture(prm));
                        }

                        response.on_hover_ui(|ui| {
                            ui.label(format!("{param_name}: {cur_val:.1}"));
                        });

                        if let Some(tex) =
                            self.get_or_load_frame_texture(&ctx, &ctrl.img, frame_idx)
                        {
                            ui.painter().image(
                                tex.id(),
                                ctrl_rect,
                                egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                ),
                                egui::Color32::WHITE,
                            );
                        }
                    }
                }
            }

            // Apply parameter value updates
            if !param_updates.is_empty() {
                let mut values = self.param_values.write().unwrap();
                for (id, val) in param_updates {
                    values.insert(id, val);
                }
            }

            // Dispatch gesture events to host output
            if !new_gui_events.is_empty() {
                if let Ok(mut evs) = self.gui_events.lock() {
                    evs.extend(new_gui_events);
                }
            }

            #[cfg(feature = "feat_oversampling")]
            self.render_oversampling_badge(ui, egui::pos2(win_w - 176.0, 10.0));
        }
    }
}

// ---------------------------------------------------------------------------
// CLAP GUI Extension Implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
const PREFERRED_API: clack_extensions::gui::GuiApiType<'static> =
    clack_extensions::gui::GuiApiType::X11;
#[cfg(target_os = "windows")]
const PREFERRED_API: clack_extensions::gui::GuiApiType<'static> =
    clack_extensions::gui::GuiApiType::WIN32;

impl PluginGuiImpl for ZfxMainThread<'_> {
    fn is_api_supported(&self, configuration: GuiConfiguration) -> bool {
        let sup = !configuration.is_floating && configuration.api_type == PREFERRED_API;
        crate::scan_log(&format!(
            "is_api_supported: api_type={:?}, is_floating={}, ret={}",
            configuration.api_type, configuration.is_floating, sup
        ));
        sup
    }

    fn get_preferred_api(&self) -> Option<GuiConfiguration<'_>> {
        crate::scan_log("get_preferred_api called");
        Some(GuiConfiguration {
            api_type: PREFERRED_API,
            is_floating: false,
        })
    }

    fn create(&self, configuration: GuiConfiguration) -> Result<(), PluginError> {
        crate::scan_log(&format!(
            "create GUI: api_type={:?}, is_floating={}",
            configuration.api_type, configuration.is_floating
        ));
        if configuration.is_floating || configuration.api_type != PREFERRED_API {
            return Err(PluginError::Message("Unsupported GUI API configuration"));
        }
        Ok(())
    }

    fn destroy(&self) {
        if let Some(win) = self.gui_window.borrow_mut().take() {
            win.close();
        }
    }

    fn set_scale(&self, _scale: f64) -> Result<(), PluginError> {
        Ok(())
    }

    fn get_size(&self) -> Option<GuiSize> {
        let (width, height) = get_window_size(self.shared.layout.as_ref());
        Some(GuiSize {
            width: width as u32,
            height: height as u32,
        })
    }

    fn can_resize(&self) -> bool {
        false
    }

    fn get_resize_hints(&self) -> Option<clack_extensions::gui::GuiResizeHints> {
        None
    }

    fn adjust_size(&self, _size: GuiSize) -> Option<GuiSize> {
        self.get_size()
    }

    fn set_size(&self, size: GuiSize) -> Result<(), PluginError> {
        if let Some(win) = self.gui_window.borrow().as_ref() {
            let _ = win.resize(baseview::dpi::Size::Physical(baseview::dpi::PhysicalSize {
                width: size.width,
                height: size.height,
            }));
        }
        Ok(())
    }

    fn set_parent(&self, window: Window) -> Result<(), PluginError> {
        if let Some(win) = self.gui_window.borrow_mut().take() {
            win.close();
        }

        let clap_win = window.as_raw();
        #[cfg(target_os = "linux")]
        // SAFETY: The host guarantees that `window` provides a valid X11 handle on Linux platforms.
        let raw_handle = unsafe { clap_win.specific.x11 };
        #[cfg(target_os = "windows")]
        // SAFETY: The host guarantees that `window` provides a valid Win32 HWND on Windows platforms.
        let raw_handle = unsafe { clap_win.specific.win32 as usize as u64 };
        let wrapper = ClapWindowWrapper(raw_handle);

        let (width, height) = get_window_size(self.shared.layout.as_ref());

        let settings = egui_baseview::EguiWindowSettings::new()
            .with_title(crate::PLUGIN_NAME)
            .with_size(baseview::dpi::Size::Logical(baseview::dpi::LogicalSize {
                width,
                height,
            }))
            .with_resizable(false)
            .with_parent(Some(&wrapper));

        let app = ZfxApp::new(
            self.shared.layout.clone(),
            std::sync::Arc::clone(&self.shared.param_values),
            self.shared.params.clone(),
            std::sync::Arc::clone(&self.shared.gui_events),
            #[cfg(feature = "feat_oversampling")]
            std::sync::Arc::clone(&self.shared.oversampling_mode),
            #[cfg(feature = "feat_oversampling")]
            std::sync::Arc::clone(&self.shared.oversampling_phase),
        );

        match egui_baseview::EguiWindow::create(settings, app) {
            Ok(win) => {
                let _ = win.show();
                *self.gui_window.borrow_mut() = Some(win);
                Ok(())
            }
            Err(e) => {
                eprintln!("[ZFX GUI Error] Failed to open baseview window: {e:?}");
                Ok(())
            }
        }
    }

    fn set_transient(&self, _window: Window) -> Result<(), PluginError> {
        Ok(())
    }

    fn suggest_title(&self, _title: &str) {}

    fn show(&self) -> Result<(), PluginError> {
        if let Some(win) = self.gui_window.borrow().as_ref() {
            let _ = win.show();
        }
        Ok(())
    }

    fn hide(&self) -> Result<(), PluginError> {
        if let Some(win) = self.gui_window.borrow().as_ref() {
            let _ = win.hide();
        }
        Ok(())
    }
}
