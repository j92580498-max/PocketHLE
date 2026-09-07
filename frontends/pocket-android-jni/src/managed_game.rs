use std::path::Path;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use pocket_core::kernel::{Framebuffer, InputEvent};

use crate::runner::{push_frame, FrameSnapshot, InputCommand, SessionState};

const LOGICAL_WIDTH: u32 = 240;
const LOGICAL_HEIGHT: u32 = 320;
const CLIENT_TOP: i32 = 20;
const CLIENT_HEIGHT: i32 = 268;

const BACKDROP: &[u8] = include_bytes!("../assets/valien-attack/BackDrop.png");
const GREEN_ALIEN: &[u8] = include_bytes!("../assets/valien-attack/GAlien.png");
const BLUE_ALIEN: &[u8] = include_bytes!("../assets/valien-attack/BAlien.png");
const RED_ALIEN: &[u8] = include_bytes!("../assets/valien-attack/RAlien.png");
const PURPLE_ALIEN: &[u8] = include_bytes!("../assets/valien-attack/PAlien.png");
const YELLOW_ALIEN: &[u8] = include_bytes!("../assets/valien-attack/YAlien.png");
const DEFENDER: &[u8] = include_bytes!("../assets/valien-attack/Defender.png");
const MISSILE: &[u8] = include_bytes!("../assets/valien-attack/Missile.png");
const STAR: &[u8] = include_bytes!("../assets/valien-attack/Star.png");
const CROSS: &[u8] = include_bytes!("../assets/valien-attack/Cross.png");

#[derive(Clone)]
struct Sprite {
    width: u32,
    height: u32,
    pixels: Vec<[u8; 4]>,
}

impl Sprite {
    fn load(data: &[u8], name: &str) -> Result<Self> {
        let image = image::load_from_memory(data).with_context(|| format!("decode {name}"))?;
        let rgba = image.to_rgba8();
        Ok(Self {
            width: rgba.width(),
            height: rgba.height(),
            pixels: rgba.pixels().map(|p| p.0).collect(),
        })
    }

    fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        self.pixels[(y * self.width + x) as usize]
    }
}

struct Assets {
    backdrop: Sprite,
    aliens: [Sprite; 5],
    defender: Sprite,
    missile: Sprite,
    star: Sprite,
    cross: Sprite,
}

impl Assets {
    fn load() -> Result<Self> {
        Ok(Self {
            backdrop: Sprite::load(BACKDROP, "BackDrop")?,
            aliens: [
                Sprite::load(RED_ALIEN, "RAlien")?,
                Sprite::load(PURPLE_ALIEN, "PAlien")?,
                Sprite::load(YELLOW_ALIEN, "YAlien")?,
                Sprite::load(BLUE_ALIEN, "BAlien")?,
                Sprite::load(GREEN_ALIEN, "GAlien")?,
            ],
            defender: Sprite::load(DEFENDER, "Defender")?,
            missile: Sprite::load(MISSILE, "Missile")?,
            star: Sprite::load(STAR, "Star")?,
            cross: Sprite::load(CROSS, "Cross")?,
        })
    }
}

struct MissileState {
    x: i32,
    y: i32,
    up: bool,
}

struct ManagedGame {
    assets: Assets,
    screen: (u32, u32),
    started: bool,
    pointer_down: bool,
    pointer_x: i32,
    defender_x: i32,
    missile: Option<MissileState>,
    frame: u64,
    cross: Option<(i32, i32)>,
    stars: [(i32, i32); 9],
}

impl ManagedGame {
    fn new(screen: (u32, u32)) -> Result<Self> {
        let assets = Assets::load()?;
        Ok(Self {
            assets,
            screen,
            started: false,
            pointer_down: false,
            pointer_x: 110,
            defender_x: 110,
            missile: None,
            frame: 0,
            cross: None,
            stars: [
                (8, 57),
                (73, 34),
                (161, 88),
                (33, 145),
                (198, 125),
                (117, 181),
                (217, 62),
                (57, 226),
                (183, 243),
            ],
        })
    }

    fn logical_x(&self, x: u16) -> i32 {
        (x as u32 * LOGICAL_WIDTH / self.screen.0.max(1)) as i32
    }

    fn logical_y(&self, y: u16) -> i32 {
        (y as u32 * LOGICAL_HEIGHT / self.screen.1.max(1)) as i32
    }

    fn start(&mut self) {
        self.started = true;
        self.cross = None;
    }

    fn move_to(&mut self, x: i32) {
        self.pointer_x = x.clamp(8, 217);
        if self.pointer_down {
            self.defender_x = self.pointer_x;
        }
    }

    fn fire(&mut self) {
        if self.started && self.missile.is_none() {
            self.missile = Some(MissileState {
                x: self.defender_x + 6,
                y: 250,
                up: true,
            });
        }
    }

    fn handle(&mut self, event: InputEvent) {
        match event {
            InputEvent::PointerDown { x, y } => {
                let x = self.logical_x(x);
                let y = self.logical_y(y);
                if y < CLIENT_TOP && x < 58 {
                    self.start();
                    return;
                }
                if self.started {
                    self.pointer_down = true;
                    self.move_to(x);
                    self.cross = Some((x, y - CLIENT_TOP));
                }
            }
            InputEvent::PointerMove { x, y } => {
                if self.started && self.pointer_down {
                    let x = self.logical_x(x);
                    let y = self.logical_y(y);
                    self.move_to(x);
                    self.cross = Some((x, y - CLIENT_TOP));
                }
            }
            InputEvent::PointerUp { x, y } => {
                if self.started {
                    let x = self.logical_x(x);
                    let y = self.logical_y(y);
                    self.pointer_down = false;
                    self.move_to(x);
                    self.cross = Some((x, y - CLIENT_TOP));
                    self.fire();
                }
            }
            InputEvent::KeyDown { vk } => match vk {
                0x25 => self.move_to(self.defender_x - 3),
                0x27 => self.move_to(self.defender_x + 3),
                0x0d | 0x20 => self.fire(),
                _ => {}
            },
            InputEvent::KeyUp { .. } => {}
        }
    }

    fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        if let Some(missile) = &mut self.missile {
            if missile.up {
                missile.y -= 5;
                if missile.y < 0 {
                    self.missile = None;
                }
            } else {
                missile.y += 5;
                if missile.y > CLIENT_HEIGHT {
                    self.missile = None;
                }
            }
        }
    }

    fn render(&self, framebuffer: &mut Framebuffer) {
        framebuffer.fill(rgb565(0, 0, 0));
        let sx = framebuffer.width as f32 / LOGICAL_WIDTH as f32;
        let sy = framebuffer.height as f32 / LOGICAL_HEIGHT as f32;
        draw_rect(
            framebuffer,
            0,
            0,
            LOGICAL_WIDTH as i32,
            CLIENT_TOP,
            rgb565(236, 236, 226),
            (sx, sy),
        );
        draw_text(framebuffer, 4, 6, "Start", rgb565(0, 0, 0), (sx, sy));
        draw_text(framebuffer, 42, 6, "Exit", rgb565(0, 0, 0), (sx, sy));
        if !self.started {
            draw_sprite(
                framebuffer,
                &self.assets.backdrop,
                0,
                0,
                CLIENT_TOP,
                false,
                (sx, sy),
            );
        } else {
            for (index, &(x, y)) in self.stars.iter().enumerate() {
                let frame = ((self.frame / 8 + index as u64) % 3) as usize;
                draw_sprite(
                    framebuffer,
                    &self.assets.star,
                    frame,
                    x,
                    CLIENT_TOP + y,
                    true,
                    (sx, sy),
                );
            }
            for row in 0..5 {
                for col in 0..7 {
                    let frame = ((self.frame / 16 + row + col) % 2) as usize;
                    draw_sprite(
                        framebuffer,
                        &self.assets.aliens[row as usize],
                        frame,
                        col as i32 * 18,
                        CLIENT_TOP + row as i32 * 17,
                        true,
                        (sx, sy),
                    );
                }
            }
            draw_sprite(
                framebuffer,
                &self.assets.defender,
                (self.frame / 5 % 2) as usize,
                self.defender_x,
                CLIENT_TOP + 250,
                true,
                (sx, sy),
            );
            if let Some(missile) = &self.missile {
                draw_sprite(
                    framebuffer,
                    &self.assets.missile,
                    (self.frame / 3 % 6) as usize,
                    missile.x,
                    CLIENT_TOP + missile.y,
                    true,
                    (sx, sy),
                );
            }
            if let Some((x, y)) = self.cross {
                draw_sprite(
                    framebuffer,
                    &self.assets.cross,
                    (self.frame / 4 % 6) as usize,
                    x - 5,
                    CLIENT_TOP + y - 5,
                    true,
                    (sx, sy),
                );
            }
        }
        framebuffer.mark_dirty();
    }
}

pub(crate) fn supports(exe: &Path) -> bool {
    exe.file_stem()
        .and_then(|name| name.to_str())
        .map(|name| name.to_ascii_lowercase().contains("valienattack"))
        .unwrap_or(false)
}

pub(crate) fn run(
    exe: &Path,
    state: &Arc<SessionState>,
    input_rx: Receiver<InputCommand>,
    screen: (u32, u32),
) -> String {
    let mut game = match ManagedGame::new(screen) {
        Ok(game) => game,
        Err(error) => return format!("Managed vAlienAttack renderer failed: {error:#}"),
    };
    let mut framebuffer = Framebuffer::new(screen.0, screen.1);
    game.render(&mut framebuffer);
    push_frame(state, FrameSnapshot::from_framebuffer(&framebuffer));
    loop {
        let mut stop = false;
        loop {
            match input_rx.try_recv() {
                Ok(InputCommand::Input(event)) => game.handle(event),
                Ok(InputCommand::Stop) => stop = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => stop = true,
            }
        }
        if stop {
            return format!(
                "Managed vAlienAttack renderer completed for {}",
                exe.display()
            );
        }
        game.tick();
        game.render(&mut framebuffer);
        push_frame(state, FrameSnapshot::from_framebuffer(&framebuffer));
        thread::sleep(Duration::from_millis(16));
    }
}

fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (b as u16 >> 3)
}

fn draw_rect(
    framebuffer: &mut Framebuffer,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u16,
    scale: (f32, f32),
) {
    let (sx, sy) = scale;
    let x0 = (x as f32 * sx).round() as i32;
    let y0 = (y as f32 * sy).round() as i32;
    let x1 = ((x + w) as f32 * sx).round() as i32;
    let y1 = ((y + h) as f32 * sy).round() as i32;
    framebuffer.fill_rect(x0, y0, x1 - x0, y1 - y0, color);
}

fn draw_sprite(
    framebuffer: &mut Framebuffer,
    sprite: &Sprite,
    frame: usize,
    x: i32,
    y: i32,
    color_key: bool,
    scale: (f32, f32),
) {
    let (sx, sy) = scale;
    let frame_width = match sprite.width {
        90 => 15,
        480 => 240,
        20 => 10,
        18 => 3,
        15 => 5,
        33 => 11,
        _ => sprite.width,
    };
    let source_x = (frame as u32 * frame_width) % sprite.width.max(1);
    for py in 0..sprite.height {
        for px in 0..frame_width {
            if source_x + px >= sprite.width {
                continue;
            }
            let rgba = sprite.pixel(source_x + px, py);
            if rgba[3] == 0 || (color_key && rgba[0] < 8 && rgba[1] < 8 && rgba[2] < 8) {
                continue;
            }
            let dx0 = ((x + px as i32) as f32 * sx).round() as i32;
            let dy0 = ((y + py as i32) as f32 * sy).round() as i32;
            let dx1 = ((x + px as i32 + 1) as f32 * sx).round() as i32;
            let dy1 = ((y + py as i32 + 1) as f32 * sy).round() as i32;
            let color = rgb565(rgba[0], rgba[1], rgba[2]);
            for yy in dy0..dy1.max(dy0 + 1) {
                for xx in dx0..dx1.max(dx0 + 1) {
                    framebuffer.put_pixel(xx, yy, color);
                }
            }
        }
    }
}

fn draw_text(
    framebuffer: &mut Framebuffer,
    x: i32,
    y: i32,
    text: &str,
    color: u16,
    (sx, sy): (f32, f32),
) {
    for (index, ch) in text.chars().enumerate() {
        let glyph = glyph(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) != 0 {
                    draw_rect(
                        framebuffer,
                        x + index as i32 * 6 + col,
                        y + row as i32,
                        1,
                        1,
                        color,
                        (sx, sy),
                    );
                }
            }
        }
    }
}

fn glyph(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'A' => [0x0e, 0x11, 0x11, 0x1f, 0x11, 0x11, 0x11],
        'E' => [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f],
        'I' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1f],
        'R' => [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11],
        'S' => [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e],
        'T' => [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_assets_match_original_dimensions() {
        let assets = Assets::load().expect("managed game assets must decode");
        assert_eq!((assets.backdrop.width, assets.backdrop.height), (240, 268));
        assert_eq!((assets.aliens[0].width, assets.aliens[0].height), (20, 11));
        assert_eq!((assets.defender.width, assets.defender.height), (90, 12));
        assert_eq!((assets.missile.width, assets.missile.height), (18, 8));
    }

    #[test]
    fn initial_frame_is_visible_and_start_tap_reaches_gameplay() {
        let mut game = ManagedGame::new((240, 320)).expect("renderer must initialize");
        let mut framebuffer = Framebuffer::new(240, 320);
        game.render(&mut framebuffer);
        assert!(!framebuffer.is_all_black());
        game.handle(InputEvent::PointerDown { x: 12, y: 8 });
        game.render(&mut framebuffer);
        let non_black = framebuffer
            .pixels
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|p| **p != [0, 0])
            .count();
        assert!(non_black > 250);
    }
}
