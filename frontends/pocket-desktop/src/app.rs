//! egui application: library screen, settings, per-game sheet.

use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Rect, RichText, ScrollArea, Sense, Vec2};

use pocket_core::kernel::{InputEvent, FB_HEIGHT, FB_WIDTH};
use pocket_library::{CpuBackendPref, GameEntry, GameSettings, LauncherConfig, Library};

use crate::runner::{FrameSnapshot, InputCommand, RunOutcome, Runner};

/// Virtual button layout for the Run screen — modelled after the
/// j2me-loader gamepad: a D-pad on the left and three action buttons
/// (A / B / Start) on the right. Pressing a button sends a
/// `WM_KEYDOWN`/`WM_KEYUP` pair down to the guest, mapped to the
/// canonical Pocket PC virtual-key codes from `gx.h`.
const VK_UP: u16 = 0x26;
const VK_DOWN: u16 = 0x28;
const VK_LEFT: u16 = 0x25;
const VK_RIGHT: u16 = 0x27;
const VK_RETURN: u16 = 0xD4; // Pocket PC center / Start.
const VK_A: u16 = 0xD1;
const VK_B: u16 = 0xD2;
const VK_C: u16 = 0xD3;
const VK_ESCAPE: u16 = 0x1B; // Back.

/// Top-level egui app.
pub struct PocketLauncher {
    library: Library,
    selected_game: Option<String>,
    screen: Screen,
    runner: Runner,
    events_rx: Receiver<UiEvent>,
    events_tx: Sender<UiEvent>,
    /// Live framebuffer updates streamed by [`Runner`] while a game
    /// is running. `Some` between [`Self::spawn_run`] and
    /// `UiEvent::RunFinished`; `None` otherwise.
    frame_rx: Option<Receiver<FrameSnapshot>>,
    /// Channel for sending [`InputCommand`]s (taps / D-pad / stop)
    /// to the running emulator. Mirrors `frame_rx`.
    input_tx: Option<Sender<InputCommand>>,
    /// Track which virtual buttons are currently held so we can fire
    /// matching `WM_KEYUP` when the user releases them. Indexed by
    /// VK code.
    pressed_keys: std::collections::HashSet<u16>,
    /// `Some` while a stylus drag is in progress — carries the last
    /// reported game-space coordinates so we don't spam the guest
    /// with redundant events.
    pointer_down_at: Option<(u16, u16)>,
    /// Game currently being launched, used as a status caption
    /// while the run is in progress.
    running_game: Option<String>,
    status: String,
    config_draft: Option<LauncherConfig>,
    game_settings_draft: Option<(String, GameSettings)>,
    last_frame_texture: Option<egui::TextureHandle>,
    last_frame_status: Option<String>,
    frame_stats: FrameStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Library,
    Settings,
    GameSettings,
    Run,
}

#[derive(Debug)]
pub enum UiEvent {
    ImportFinished(Result<String, String>),
    RunFinished(RunOutcome),
}

#[derive(Debug, Clone)]
struct FrameStats {
    displayed_frames: u64,
    fps: f32,
    window_started_at: Option<Instant>,
    window_frames: u32,
    last_frame_at: Option<Instant>,
    last_frame_ms: Option<f32>,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self {
            displayed_frames: 0,
            fps: 0.0,
            window_started_at: None,
            window_frames: 0,
            last_frame_at: None,
            last_frame_ms: None,
        }
    }
}

impl FrameStats {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn record_frame(&mut self) {
        let now = Instant::now();
        if let Some(previous) = self.last_frame_at {
            self.last_frame_ms = Some(now.duration_since(previous).as_secs_f32() * 1000.0);
        }
        self.last_frame_at = Some(now);
        self.displayed_frames = self.displayed_frames.saturating_add(1);
        self.window_frames = self.window_frames.saturating_add(1);

        let started_at = match self.window_started_at {
            Some(t) => t,
            None => {
                self.window_started_at = Some(now);
                now
            }
        };
        let elapsed = now.duration_since(started_at);
        if elapsed >= Duration::from_secs(1) {
            self.fps = self.window_frames as f32 / elapsed.as_secs_f32();
            self.window_frames = 0;
            self.window_started_at = Some(now);
        }
    }

    fn overlay_text(&self) -> String {
        let last_ms = self.last_frame_ms.unwrap_or(0.0);
        let since_ms = self
            .last_frame_at
            .map(|t| t.elapsed().as_secs_f32() * 1000.0)
            .unwrap_or(0.0);
        format!(
            "FPS {:.1}  Frames {}  Last {:.0}ms  Since {:.0}ms",
            self.fps, self.displayed_frames, last_ms, since_ms
        )
    }
}

impl PocketLauncher {
    pub fn new(_cc: &eframe::CreationContext<'_>, library: Library) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            library,
            selected_game: None,
            screen: Screen::Library,
            runner: Runner::new(),
            events_rx: rx,
            events_tx: tx,
            frame_rx: None,
            input_tx: None,
            pressed_keys: std::collections::HashSet::new(),
            pointer_down_at: None,
            running_game: None,
            status: "Welcome to PocketHLE.".to_string(),
            config_draft: None,
            game_settings_draft: None,
            last_frame_texture: None,
            last_frame_status: None,
            frame_stats: FrameStats::default(),
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                UiEvent::ImportFinished(Ok(name)) => {
                    self.status = format!("Imported {name}.");
                    self.reload_library();
                }
                UiEvent::ImportFinished(Err(e)) => {
                    self.status = format!("Import failed: {e}");
                }
                UiEvent::RunFinished(outcome) => {
                    self.last_frame_status = Some(outcome.summary.clone());
                    if let Some(frame) = outcome.framebuffer {
                        self.upload_frame_texture(ctx, &frame);
                    }
                    self.status = outcome.summary;
                    self.frame_rx = None;
                    self.input_tx = None;
                    self.release_all_keys();
                    self.pointer_down_at = None;
                    self.running_game = None;
                }
            }
        }
        // Drain any live preview frames the background runner may
        // have produced since the last UI tick.
        let mut latest: Option<FrameSnapshot> = None;
        if let Some(rx) = self.frame_rx.as_ref() {
            while let Ok(frame) = rx.try_recv() {
                latest = Some(frame);
            }
        }
        if let Some(frame) = latest {
            self.upload_frame_texture(ctx, &frame);
        }
    }

    fn upload_frame_texture(&mut self, ctx: &egui::Context, frame: &FrameSnapshot) {
        let size = [frame.width as usize, frame.height as usize];
        let img = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
        if let Some(tex) = self.last_frame_texture.as_mut() {
            tex.set(img, egui::TextureOptions::NEAREST);
        } else {
            let tex = ctx.load_texture("pockethle-fb", img, egui::TextureOptions::NEAREST);
            self.last_frame_texture = Some(tex);
        }
        self.frame_stats.record_frame();
    }

    fn reload_library(&mut self) {
        match Library::open(self.library.root()) {
            Ok(lib) => self.library = lib,
            Err(e) => self.status = format!("Could not reload library: {e}"),
        }
    }

    fn ui_top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("PocketHLE");
            ui.label(
                RichText::new("Pocket PC / Windows Mobile launcher")
                    .small()
                    .color(Color32::from_gray(160)),
            );
            ui.add_space(16.0);
            if ui
                .selectable_label(self.screen == Screen::Library, "Library")
                .clicked()
            {
                self.screen = Screen::Library;
            }
            if ui
                .selectable_label(self.screen == Screen::Settings, "Settings")
                .clicked()
            {
                self.config_draft = Some(self.library.config().clone());
                self.screen = Screen::Settings;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Import .CAB...").clicked() {
                    self.spawn_import_dialog();
                }
            });
        });
        ui.separator();
    }

    fn ui_library(&mut self, ui: &mut egui::Ui) {
        let games: Vec<GameEntry> = self.library.games().to_vec();
        if games.is_empty() {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("No games yet")
                        .heading()
                        .color(Color32::from_gray(160)),
                );
                ui.add_space(8.0);
                ui.label("Click \"Import .CAB...\" to add a Pocket PC game.");
                ui.add_space(20.0);
                if ui.button("Import .CAB...").clicked() {
                    self.spawn_import_dialog();
                }
            });
            return;
        }
        ScrollArea::vertical().show(ui, |ui| {
            let avail = ui.available_width();
            let card_width = 280.0_f32.min(avail);
            let columns = ((avail / (card_width + 12.0)).floor() as usize).max(1);
            egui::Grid::new("library_grid")
                .num_columns(columns)
                .spacing(Vec2::new(12.0, 12.0))
                .show(ui, |ui| {
                    for (i, game) in games.iter().enumerate() {
                        self.ui_game_card(ui, game, card_width);
                        if (i + 1) % columns == 0 {
                            ui.end_row();
                        }
                    }
                });
        });
    }

    fn ui_game_card(&mut self, ui: &mut egui::Ui, game: &GameEntry, width: f32) {
        let frame = egui::Frame::group(ui.style())
            .rounding(8.0)
            .inner_margin(12.0);
        frame.show(ui, |ui| {
            ui.set_width(width);
            ui.set_min_height(120.0);
            ui.horizontal(|ui| {
                let icon = RichText::new("\u{1F4F1}").size(32.0);
                ui.label(icon);
                ui.vertical(|ui| {
                    ui.label(RichText::new(&game.display_name).strong().size(16.0));
                    if let Some(p) = &game.provider {
                        ui.label(RichText::new(p).small().color(Color32::from_gray(170)));
                    }
                    ui.label(
                        RichText::new(format!("CAB: {}", game.source_cab))
                            .small()
                            .color(Color32::from_gray(140)),
                    );
                    ui.label(
                        RichText::new(format!("Backend: {}", game.settings.cpu_backend.label()))
                            .small()
                            .color(Color32::from_gray(140)),
                    );
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Run").clicked() {
                    self.spawn_run(game);
                }
                if ui.button("Settings").clicked() {
                    self.selected_game = Some(game.id.clone());
                    self.game_settings_draft = Some((game.id.clone(), game.settings.clone()));
                    self.screen = Screen::GameSettings;
                }
                if ui.button("Remove").clicked() {
                    if let Err(e) = self.library.remove(&game.id) {
                        self.status = format!("Remove failed: {e}");
                    } else {
                        self.status = format!("Removed {}", game.display_name);
                    }
                }
            });
        });
    }

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        let Some(mut draft) = self.config_draft.take() else {
            return;
        };
        let mut save_clicked = false;
        let mut cancel_clicked = false;
        ui.heading("Launcher settings");
        ui.add_space(8.0);
        let library_root = self.library.root().display().to_string();
        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing(Vec2::new(12.0, 8.0))
            .show(ui, |ui| {
                ui.label("Default CPU backend");
                egui::ComboBox::from_id_source("cpu_backend")
                    .selected_text(draft.default_cpu_backend.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut draft.default_cpu_backend,
                            CpuBackendPref::Stub,
                            CpuBackendPref::Stub.label(),
                        );
                        if cfg!(feature = "unicorn") {
                            ui.selectable_value(
                                &mut draft.default_cpu_backend,
                                CpuBackendPref::Unicorn,
                                CpuBackendPref::Unicorn.label(),
                            );
                        }
                    });
                ui.end_row();

                ui.label("Verbosity (0..3)");
                ui.add(egui::Slider::new(&mut draft.verbosity, 0..=3));
                ui.end_row();

                ui.label("Show FPS overlay");
                ui.checkbox(&mut draft.show_fps, "");
                ui.end_row();

                ui.label("Library root");
                ui.label(library_root);
                ui.end_row();
            });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                save_clicked = true;
            }
            if ui.button("Cancel").clicked() {
                cancel_clicked = true;
            }
        });
        if save_clicked {
            *self.library.config_mut() = draft;
            if let Err(e) = self.library.save() {
                self.status = format!("Could not save settings: {e}");
            } else {
                self.status = "Settings saved.".to_string();
            }
            self.screen = Screen::Library;
        } else if cancel_clicked {
            self.screen = Screen::Library;
        } else {
            self.config_draft = Some(draft);
        }
    }

    fn ui_game_settings(&mut self, ui: &mut egui::Ui) {
        let Some((id, mut draft)) = self.game_settings_draft.take() else {
            return;
        };
        let display_name = self
            .library
            .get(&id)
            .map(|g| g.display_name.clone())
            .unwrap_or_else(|| id.clone());
        let mut save_clicked = false;
        let mut cancel_clicked = false;
        ui.heading(format!("Settings: {display_name}"));
        ui.add_space(8.0);
        egui::Grid::new("game_settings_grid")
            .num_columns(2)
            .spacing(Vec2::new(12.0, 8.0))
            .show(ui, |ui| {
                ui.label("CPU backend");
                egui::ComboBox::from_id_source("game_cpu_backend")
                    .selected_text(draft.cpu_backend.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut draft.cpu_backend,
                            CpuBackendPref::Stub,
                            CpuBackendPref::Stub.label(),
                        );
                        if cfg!(feature = "unicorn") {
                            ui.selectable_value(
                                &mut draft.cpu_backend,
                                CpuBackendPref::Unicorn,
                                CpuBackendPref::Unicorn.label(),
                            );
                        }
                    });
                ui.end_row();

                ui.label("Max slices");
                ui.add(egui::DragValue::new(&mut draft.max_slices).clamp_range(1..=u64::MAX));
                ui.end_row();

                ui.label("Instructions / slice");
                ui.add(
                    egui::DragValue::new(&mut draft.instructions_per_slice)
                        .clamp_range(1..=u64::MAX),
                );
                ui.end_row();

                ui.label("Halt on unimplemented API");
                ui.checkbox(&mut draft.halt_on_unimplemented, "");
                ui.end_row();
            });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                save_clicked = true;
            }
            if ui.button("Cancel").clicked() {
                cancel_clicked = true;
            }
        });
        if save_clicked {
            if let Err(e) = self.library.update_settings(&id, draft) {
                self.status = format!("Could not save game settings: {e}");
            } else {
                self.status = "Game settings saved.".to_string();
            }
            self.screen = Screen::Library;
        } else if cancel_clicked {
            self.screen = Screen::Library;
        } else {
            self.game_settings_draft = Some((id, draft));
        }
    }

    fn ui_run(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Run");
            if let Some(name) = self.running_game.as_ref() {
                ui.label(format!("— {name}"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Back to library").clicked() {
                    // Best-effort — if the runner thread already
                    // exited the channel will just drop the message.
                    if let Some(tx) = self.input_tx.as_ref() {
                        let _ = tx.send(InputCommand::Stop);
                    }
                    self.release_all_keys();
                    self.screen = Screen::Library;
                }
            });
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            self.ui_run_screen(ui);
            ui.add_space(12.0);
            self.ui_virtual_pad(ui);
        });
        ui.add_space(8.0);
        if let Some(s) = self.last_frame_status.as_ref() {
            ui.label(RichText::new(s).small().color(Color32::from_gray(170)));
        }
    }

    /// Render the live framebuffer (or placeholder) and forward any
    /// pointer presses on it as `WM_LBUTTONDOWN` / `WM_LBUTTONUP`
    /// events with stylus coordinates in 240×320 game space.
    fn ui_run_screen(&mut self, ui: &mut egui::Ui) {
        let Some(tex) = self.last_frame_texture.clone() else {
            ui.allocate_ui(
                Vec2::new(FB_WIDTH as f32 * 2.0, FB_HEIGHT as f32 * 2.0),
                |ui| {
                    ui.label("(no framebuffer captured yet — try Run again)");
                },
            );
            return;
        };
        let size = tex.size_vec2();
        // Display at 2x for readability, the same way the CLI's
        // minifb DisplayHook scales.
        let scale = 2.0_f32;
        let display_size = size * scale;
        let (rect, response) = ui.allocate_exact_size(display_size, Sense::click_and_drag());
        let image = egui::Image::from_texture(&tex).fit_to_exact_size(display_size);
        image.paint_at(ui, rect);
        // The j2me-loader-style FPS overlay is opt-in: gated on the
        // launcher's `show_fps` config flag so users who find a
        // permanent debug HUD distracting can switch it off in
        // Settings.
        if self.library.config().show_fps {
            let overlay_rect =
                Rect::from_min_size(rect.min + Vec2::new(6.0, 6.0), Vec2::new(390.0, 24.0));
            ui.painter()
                .rect_filled(overlay_rect, 4.0, Color32::from_black_alpha(190));
            ui.painter().text(
                overlay_rect.min + Vec2::new(6.0, 4.0),
                egui::Align2::LEFT_TOP,
                self.frame_stats.overlay_text(),
                egui::FontId::monospace(13.0),
                Color32::LIGHT_GREEN,
            );
        }
        self.handle_pointer(&rect, &response);
    }

    fn handle_pointer(&mut self, rect: &Rect, response: &egui::Response) {
        let Some(pos) = response.interact_pointer_pos() else {
            // No pointer over the framebuffer this frame — if the
            // user just released the button, send a corresponding
            // PointerUp at the last known coords.
            if response.drag_stopped() || response.clicked() {
                if let Some((x, y)) = self.pointer_down_at.take() {
                    self.send_input(InputEvent::PointerUp { x, y });
                }
            }
            return;
        };
        let local = pos - rect.min;
        let scale_x = FB_WIDTH as f32 / rect.width();
        let scale_y = FB_HEIGHT as f32 / rect.height();
        let game_x = (local.x * scale_x).clamp(0.0, (FB_WIDTH - 1) as f32) as u16;
        let game_y = (local.y * scale_y).clamp(0.0, (FB_HEIGHT - 1) as f32) as u16;
        if response.drag_started() || response.is_pointer_button_down_on() {
            // Either freshly pressed, or holding & dragging — if we
            // weren't already tracking a press, fire PointerDown.
            if self.pointer_down_at.is_none() {
                self.send_input(InputEvent::PointerDown {
                    x: game_x,
                    y: game_y,
                });
            } else {
                self.send_input(InputEvent::PointerMove {
                    x: game_x,
                    y: game_y,
                });
            }
            self.pointer_down_at = Some((game_x, game_y));
        }
        if response.drag_stopped() || response.clicked() {
            self.send_input(InputEvent::PointerUp {
                x: game_x,
                y: game_y,
            });
            self.pointer_down_at = None;
        }
    }

    /// j2me-loader-inspired virtual gamepad: a D-pad on the left and
    /// three action buttons (A / B / Start) plus two soft keys on
    /// the right. Each button drives a `WM_KEYDOWN`/`WM_KEYUP` pair
    /// while held.
    fn ui_virtual_pad(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.label(RichText::new("Controls").strong());
            ui.add_space(4.0);
            // ----- D-pad: 3x3 grid with cardinal arrows -----
            egui::Grid::new("vpad_dpad")
                .spacing(Vec2::new(2.0, 2.0))
                .show(ui, |ui| {
                    ui.label("");
                    self.vbutton(ui, "▲", VK_UP, 44.0);
                    ui.label("");
                    ui.end_row();
                    self.vbutton(ui, "◀", VK_LEFT, 44.0);
                    self.vbutton(ui, "Start", VK_RETURN, 44.0);
                    self.vbutton(ui, "▶", VK_RIGHT, 44.0);
                    ui.end_row();
                    ui.label("");
                    self.vbutton(ui, "▼", VK_DOWN, 44.0);
                    ui.label("");
                    ui.end_row();
                });
            ui.add_space(8.0);
            // ----- Soft keys -----
            ui.horizontal(|ui| {
                self.vbutton(ui, "A", VK_A, 52.0);
                self.vbutton(ui, "B", VK_B, 52.0);
                self.vbutton(ui, "C", VK_C, 52.0);
            });
            ui.add_space(4.0);
            // ----- Back / start -----
            ui.horizontal(|ui| {
                self.vbutton(ui, "Back", VK_ESCAPE, 60.0);
            });
        });
    }

    /// Render one virtual button. Pressed-while-pointer-is-down
    /// generates `WM_KEYDOWN` once; releasing fires `WM_KEYUP`.
    fn vbutton(&mut self, ui: &mut egui::Ui, label: &str, vk: u16, size: f32) {
        let was_pressed = self.pressed_keys.contains(&vk);
        let mut button = egui::Button::new(RichText::new(label).size(16.0).strong())
            .min_size(Vec2::new(size, size));
        if was_pressed {
            button = button.fill(Color32::from_rgb(80, 130, 255));
        }
        let response = ui.add_sized(Vec2::new(size, size), button);
        let now_pressed = response.is_pointer_button_down_on();
        match (was_pressed, now_pressed) {
            (false, true) => {
                self.pressed_keys.insert(vk);
                self.send_input(InputEvent::KeyDown { vk });
            }
            (true, false) => {
                self.pressed_keys.remove(&vk);
                self.send_input(InputEvent::KeyUp { vk });
            }
            _ => {}
        }
    }

    fn send_input(&self, ev: InputEvent) {
        if let Some(tx) = self.input_tx.as_ref() {
            let _ = tx.send(InputCommand::Input(ev));
        }
    }

    fn release_all_keys(&mut self) {
        let keys: Vec<u16> = self.pressed_keys.drain().collect();
        for vk in keys {
            self.send_input(InputEvent::KeyUp { vk });
        }
    }

    fn handle_physical_keyboard(&mut self, ctx: &egui::Context) {
        if self.screen != Screen::Run {
            return;
        }
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            let egui::Event::Key {
                key,
                pressed,
                repeat,
                ..
            } = event
            else {
                continue;
            };
            let Some(vk) = physical_vk(key) else {
                continue;
            };
            if pressed {
                if !repeat && self.pressed_keys.insert(vk) {
                    self.send_input(InputEvent::KeyDown { vk });
                }
            } else if self.pressed_keys.remove(&vk) {
                self.send_input(InputEvent::KeyUp { vk });
            }
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.release_all_keys();
        }
    }

    fn spawn_import_dialog(&mut self) {
        let library_root = self.library.root().to_path_buf();
        let last_dir = self.library.config().last_import_dir.clone();
        let tx = self.events_tx.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Import Pocket PC game")
                // A single "Pocket PC game" filter that matches every
                // shape we know how to import keeps the dialog UX
                // simple — the user picks a file and we pick the
                // right loader from the extension below.
                .add_filter(
                    "Pocket PC game (.cab / .zip / .exe)",
                    &["cab", "CAB", "zip", "ZIP", "exe", "EXE"],
                )
                .add_filter("Cabinet archive", &["cab", "CAB"])
                .add_filter("Zip archive", &["zip", "ZIP"])
                .add_filter("ARM PE executable", &["exe", "EXE"]);
            if let Some(d) = last_dir {
                dialog = dialog.set_directory(d);
            }
            let Some(path) = dialog.pick_file() else {
                let _ = tx.send(UiEvent::ImportFinished(Err("cancelled".into())));
                return;
            };
            let result = (|| -> Result<String, String> {
                let mut lib = Library::open(&library_root).map_err(|e| e.to_string())?;
                let parent = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(library_root.clone());
                lib.config_mut().last_import_dir = Some(parent);
                lib.save().map_err(|e| e.to_string())?;
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase);
                let entry = match ext.as_deref() {
                    Some("cab") => lib.import_cab(&path),
                    Some("zip") => lib.import_zip(&path),
                    // Treat anything else (`.exe`, no extension, …)
                    // as a raw ARM PE32. `import_exe` rejects it
                    // with a clear error if the machine type is
                    // wrong.
                    _ => lib.import_exe(&path),
                }
                .map_err(|e| e.to_string())?;
                Ok(entry.display_name.clone())
            })();
            let _ = tx.send(UiEvent::ImportFinished(result));
        });
        self.status = "Importing...".to_string();
    }

    fn spawn_run(&mut self, game: &GameEntry) {
        self.last_frame_texture = None;
        self.last_frame_status = None;
        self.frame_stats.reset();
        self.screen = Screen::Run;
        self.running_game = Some(game.display_name.clone());
        let (frame_tx, frame_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        self.frame_rx = Some(frame_rx);
        self.input_tx = Some(input_tx);
        self.pressed_keys.clear();
        self.pointer_down_at = None;
        let library_root = self.library.root().to_path_buf();
        let game = game.clone();
        let tx = self.events_tx.clone();
        let runner = self.runner.clone();
        std::thread::spawn(move || {
            let outcome = runner.run_game(library_root, game, Some(frame_tx), Some(input_rx));
            let _ = tx.send(UiEvent::RunFinished(outcome));
        });
        self.status = "Starting emulator...".to_string();
    }
}

fn physical_vk(key: egui::Key) -> Option<u16> {
    Some(match key {
        egui::Key::ArrowUp => VK_UP,
        egui::Key::ArrowDown => VK_DOWN,
        egui::Key::ArrowLeft => VK_LEFT,
        egui::Key::ArrowRight => VK_RIGHT,
        egui::Key::Enter | egui::Key::Space => VK_RETURN,
        egui::Key::Escape => VK_ESCAPE,
        egui::Key::A => VK_A,
        egui::Key::B => VK_B,
        egui::Key::C => VK_C,
        _ => return None,
    })
}

impl eframe::App for PocketLauncher {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_physical_keyboard(ctx);
        self.drain_events(ctx);
        egui::TopBottomPanel::top("top").show(ctx, |ui| self.ui_top_bar(ui));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.status).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .small()
                            .color(Color32::from_gray(140)),
                    );
                });
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.screen {
            Screen::Library => self.ui_library(ui),
            Screen::Settings => self.ui_settings(ui),
            Screen::GameSettings => self.ui_game_settings(ui),
            Screen::Run => self.ui_run(ui),
        });
        // While a game is running we want to drain the live frame
        // channel as fast as the runner produces frames; the original
        // 250 ms cadence capped the launcher at 4 fps, which is most
        // of what users perceived as "lag" in JumpyBall on the
        // desktop. ~16 ms (60 fps) matches the runner's per-slice
        // hook and keeps idle-frame redraws cheap because egui only
        // actually re-uploads textures when something changed.
        let repaint_after = if matches!(self.screen, Screen::Run) || self.frame_rx.is_some() {
            Duration::from_millis(16)
        } else {
            Duration::from_millis(250)
        };
        ctx.request_repaint_after(repaint_after);
    }
}
