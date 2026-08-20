//! The window, and the machine running behind it on a thread of its own.
//!
//! `eframe` owns the main thread, because that is where a platform's event loop has to
//! be, so the machine is what moves: `Machine: Send` was asserted for this. Nothing
//! crosses between them but the handles a frontend was always going to hold. What the
//! window reads is a `Screen`, which is the display card seen from the host, and what
//! it writes is the `Running` flag the harts check between quanta, so closing the
//! window stops the machine at an instruction boundary rather than killing it.
//!
//! A frame costs the pages that moved rather than the whole picture. Video memory
//! marks the pages a hart has written, a picture is drawn a row at a time, so the rows
//! any of those pages belong to are the bands worth uploading, and each band is one
//! partial update of the texture the picture lives in.
//!
//! Reading video memory while a hart is writing it is the bargain `Vram::as_slice`
//! describes: a frame may be torn halfway through being drawn, which is what a real
//! card's scanout engine sees, and the next frame fixes it.

use std::{ops::Range, thread, time::Duration};

use eframe::egui::{
    CentralPanel, Color32, ColorImage, Context, Event, Image, Panel, TextureHandle, TextureOptions,
    Ui, ViewportBuilder, load::SizedTexture,
};

use crate::{
    bochs::{Dirty, Format, Mode, PAGE, Screen},
    hid::{Keys, Pointer},
    input::{Keyboard, Mouse},
    machine::{Halt, Machine, Running},
};

/// The ends of the machine a window drives it through: what a frame is read out of,
/// and what a keystroke and a mouse movement are pushed into. Every one of them is
/// missing on a machine that was not built with the device behind it.
#[derive(Debug, Default)]
pub struct Ends {
    pub screen: Option<Screen>,
    pub keys: Option<Keys>,
    pub pointer: Option<Pointer>,
}

/// How long the window leaves between asking the display what it is showing. A guest
/// draws when it likes and says nothing about it, so a frontend has to look; sixty
/// times a second is what a monitor would have shown anyway.
const FRAME: Duration = Duration::from_millis(16);

/// How large the window opens, which is the picture the card's own monitor says it
/// prefers. Whatever the guest sets is scaled into it.
const WIDTH: f32 = 1280.0;
const HEIGHT: f32 = 800.0;

/// Pixels are pixels. A guest picture magnified into a window is nearest-neighbour and
/// not smoothed, since a smoothed one is no longer the picture the guest drew.
const PIXELS: TextureOptions = TextureOptions::NEAREST;

/// Open a window on `screen` and run `machine` behind it, until the window is closed
/// or the machine reaches a trap nothing handles. Answers that trap, if that is how it
/// ended.
pub fn run(machine: &mut Machine, ends: Ends) -> Result<Option<Halt>, eframe::Error> {
    let running = machine.running.clone();
    let screen = ends.screen;
    // Made here rather than by `eframe`, so that the thread watching the guest's screen
    // has something to wake the window through before there is a window.
    let ctx = Context::default();
    let mut halt = None;
    let mut opened = Ok(());

    thread::scope(|scope| {
        scope.spawn(|| halt = machine.run());
        let _watching = Watching(running.clone());
        scope.spawn({
            let (ctx, screen, running) = (ctx.clone(), screen.clone(), running.clone());
            move || watch(&ctx, screen.as_ref(), &running)
        });

        let options = eframe::NativeOptions {
            viewport: ViewportBuilder::default().with_inner_size([WIDTH, HEIGHT]),
            ..Default::default()
        };
        opened = eframe::run_native_ext(
            "rysk",
            options,
            Some(ctx.clone()),
            Box::new(|_| {
                let window = Window::new(screen, ends.keys, ends.pointer, running.clone());
                Ok(Box::new(window))
            }),
        );
    });

    opened.map(|()| halt)
}

/// Watch the guest's screen, and wake the window when there is something new on it.
///
/// This is why nothing here repaints on a timer. A repaint is a whole frame laid out,
/// tessellated, uploaded and presented, and asking for one sixty times a second costs
/// that whether or not the guest drew anything; a machine on a serial console draws
/// nothing for its whole life. Asking whether it drew is a bit per page of video
/// memory, so the polling is here and the painting only happens when it found
/// something.
///
/// The mode is watched as well as the pixels, since a guest that has just turned its
/// display on has changed what the window says without having written a byte.
fn watch(ctx: &Context, screen: Option<&Screen>, running: &Running) {
    let mut showing = screen.and_then(Screen::mode);
    while running.going() {
        let mode = screen.and_then(Screen::mode);
        if mode != showing || screen.is_some_and(|screen| screen.vram().drawn()) {
            ctx.request_repaint();
        }
        showing = mode;
        thread::sleep(FRAME);
    }
    // One last frame, which is the one that says the machine has stopped.
    ctx.request_repaint();
}

/// Somebody is watching the machine, for as long as this exists.
///
/// The window is what watches it, and whatever becomes of the window — closed, never
/// opened, or a panic on the way out of the platform's own event loop — the machine
/// behind it has nobody left. `Drop` rather than a line after `run_native`, because an
/// unwind goes past that line and then `thread::scope` waits for ever on harts nothing
/// ever asked to stop.
struct Watching(Running);

impl Drop for Watching {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// The window: the guest's screen, and the machine it belongs to.
struct Window {
    /// The display card, if the machine was built with one.
    screen: Option<Screen>,
    /// Whether the machine is still going, which is the one thing about it this
    /// milestone can say. The panels that say the rest come later.
    running: Running,
    picture: Option<Picture>,
    /// The guest's own keyboard and mouse, which the window presses and moves.
    keyboard: Keyboard,
    mouse: Mouse,
}

/// The texture the guest's picture is in, and the mode it was built for.
///
/// Any change of mode is a new texture rather than an update of this one: a different
/// shape, a different pixel or a different place in video memory all mean every pixel
/// of the old texture is now the wrong pixel.
struct Picture {
    mode: Mode,
    texture: TextureHandle,
}

impl Window {
    fn new(
        screen: Option<Screen>,
        keys: Option<Keys>,
        pointer: Option<Pointer>,
        running: Running,
    ) -> Self {
        Self {
            screen,
            running,
            picture: None,
            keyboard: Keyboard::new(keys),
            mouse: Mouse::new(pointer),
        }
    }

    /// Everything that happened at the window this frame, as the guest's own devices.
    ///
    /// The keys are taken by physical position rather than by what they produced, so
    /// the keymap that applies is the guest's and not the host's. Movement is gathered
    /// across the frame and sent once at the end, since a report carries one byte in
    /// each direction and a frame holds many events.
    fn input(&mut self, ctx: &Context) {
        ctx.input(|input| {
            for event in &input.events {
                match event {
                    Event::Key {
                        physical_key: Some(key),
                        pressed,
                        ..
                    } => {
                        self.keyboard.key(*key, *pressed);
                    }
                    Event::MouseMoved(delta) => self.mouse.moved(*delta),
                    Event::PointerButton {
                        button, pressed, ..
                    } => self.mouse.button(*button, *pressed),
                    Event::MouseWheel { unit, delta, .. } => self.mouse.wheel(*unit, *delta),
                    _ => {}
                }
            }
            // A window nobody is typing at holds nothing down. Without this a key held
            // as the window loses focus stays held in the guest for ever, since the
            // release goes to whatever took the focus.
            if !input.focused {
                self.keyboard.released();
            }
        });
        self.mouse.flush();
    }

    /// Bring the texture up to date with what the guest has drawn, and answer the mode
    /// it is now in, or nothing if the guest is not showing anything.
    fn refresh(&mut self, ctx: &Context) -> Option<Mode> {
        let screen = self.screen.as_ref()?;
        let Some(mode) = screen.mode() else {
            self.picture = None;
            return None;
        };
        let vram = screen.vram();

        // Taken before the bytes are read rather than after: a write that lands during
        // the read either reaches this frame or leaves its page dirty for the next,
        // where taking the bits afterwards would clear a page that had been read
        // before it was written.
        let dirty = vram.take_dirty();
        // The whole bargain of `as_slice`: these bytes are shared with every running
        // hart, and a frame drawn from them may be one a hart is halfway through.
        let bytes = unsafe { vram.as_slice() };

        match &mut self.picture {
            Some(picture) if picture.mode == mode => {
                for band in bands(&dirty, &mode) {
                    let rows = rows(bytes, &mode, band.clone());
                    picture
                        .texture
                        .set_partial([0, band.start as usize], rows, PIXELS);
                }
            }
            // A mode this window has not drawn yet, which is every pixel of it.
            _ => {
                let whole = rows(bytes, &mode, 0..mode.height);
                self.picture = Some(Picture {
                    mode,
                    texture: ctx.load_texture("guest", whole, PIXELS),
                });
            }
        }
        Some(mode)
    }

    /// What the window says about the machine above the picture: the mode the guest
    /// asked for, and whether the machine is still running.
    fn status(&self, ui: &mut Ui, mode: Option<Mode>) {
        let showing = match (&self.screen, mode) {
            (None, _) => "no display: the machine was built without one".to_owned(),
            (Some(_), None) => "showing nothing".to_owned(),
            (Some(_), Some(mode)) => format!(
                "{}x{} in {:?}, {} bytes to a row",
                mode.width, mode.height, mode.format, mode.stride
            ),
        };
        let state = match self.running.going() {
            true => "running",
            false => "stopped",
        };
        ui.horizontal(|ui| {
            ui.label(showing);
            ui.separator();
            ui.label(state);
        });
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Nothing asks for the next frame here. `watch` is what does, once it has seen
        // that there is a next frame worth having.
        let ctx = ui.ctx().clone();
        self.input(&ctx);
        let mode = self.refresh(&ctx);
        Panel::top("status").show(ui, |ui| self.status(ui, mode));
        CentralPanel::default().show(ui, |ui| {
            let Some(picture) = &self.picture else {
                return;
            };
            // Scaled to fit and centred, keeping the picture's own proportions: a
            // guest's pixels are not the window's, and stretching them would be a
            // different picture.
            let size = picture.texture.size_vec2();
            let space = ui.available_size();
            let scale = (space.x / size.x).min(space.y / size.y);
            let texture = SizedTexture::new(picture.texture.id(), size);
            ui.centered_and_justified(|ui| {
                ui.add(Image::new(texture).fit_to_exact_size(size * scale));
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.running.stop();
    }
}

/// The rows of the picture a frame's dirty pages fall in.
///
/// Video memory is dirty by the page and a picture is drawn by the row, so a page that
/// was written is every row any of its bytes belongs to. `Dirty::pages` answers in
/// order, so a band either carries on from the one before it or starts a new one.
fn bands(dirty: &Dirty, mode: &Mode) -> Vec<Range<u32>> {
    let stride = mode.stride as u64;
    let mut bands: Vec<Range<u32>> = Vec::new();
    for page in dirty.pages() {
        let (first, last) = (page * PAGE, page * PAGE + PAGE - 1);
        // Everything before the top left pixel is video memory the guest is not
        // showing, which a driver panning or double buffering leaves behind it.
        if last < mode.offset {
            continue;
        }
        let top = (first.saturating_sub(mode.offset) / stride) as u32;
        let bottom = ((last - mode.offset) / stride + 1).min(mode.height as u64) as u32;
        let band = top..bottom;
        if band.is_empty() {
            continue;
        }
        match bands.last_mut() {
            Some(last) if band.start <= last.end => last.end = last.end.max(band.end),
            _ => bands.push(band),
        }
    }
    bands
}

/// Rows `band` of the picture, as the pixels a texture is made of.
///
/// The format is decided once for the whole band and not once for a pixel: every pixel
/// of a picture is in the same one, and a picture is a million of them a frame. Each
/// row is taken as a slice and walked in whole pixels, so the bounds check is one per
/// row rather than one per byte read out of it.
fn rows(bytes: &[u8], mode: &Mode, band: Range<u32>) -> ColorImage {
    let (width, stride) = (mode.width as usize, mode.stride as u64);
    let depth = mode.format.bytes() as usize;
    let mut pixels = Vec::with_capacity(width * band.len());
    let row = |row: u32| {
        let start = (mode.offset + row as u64 * stride) as usize;
        &bytes[start..start + width * depth]
    };
    match mode.format {
        // Thirty-two bits is one `0x00RRGGBB` word in whichever order the driver asked
        // the card to keep its pixels in.
        Format::Xrgb8888 { big_endian: false } => {
            for y in band.clone() {
                let row = row(y).chunks_exact(4);
                pixels.extend(row.map(|p| Color32::from_rgb(p[2], p[1], p[0])));
            }
        }
        Format::Xrgb8888 { big_endian: true } => {
            for y in band.clone() {
                let row = row(y).chunks_exact(4);
                pixels.extend(row.map(|p| Color32::from_rgb(p[1], p[2], p[3])));
            }
        }
        // Five, six and five bits in a native-order word. Each is widened by repeating
        // its own top bits, so that all ones is white rather than nearly white.
        Format::R5g6b5 => {
            for y in band.clone() {
                let row = row(y).chunks_exact(2);
                pixels.extend(row.map(|p| {
                    let word = u16::from_le_bytes([p[0], p[1]]);
                    let (red, green, blue) = (word >> 11, (word >> 5) & 0x3f, word & 0x1f);
                    Color32::from_rgb(
                        (red << 3 | red >> 2) as u8,
                        (green << 2 | green >> 4) as u8,
                        (blue << 3 | blue >> 2) as u8,
                    )
                }));
            }
        }
    }
    ColorImage::new([width, band.len()], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bochs::{Bochs, VGAMEM},
        pci::Function,
    };

    /// The card's first window, which is where the framebuffer is.
    const FRAMEBUFFER: usize = 0;

    /// A card, and the host's side of it.
    fn card() -> (Bochs, Screen) {
        let card = Bochs::new(VGAMEM);
        let screen = card.screen();
        (card, screen)
    }

    /// A mode of that shape, without going through the registers a driver writes it
    /// through: what is under test here is what a host makes of a mode, not what makes
    /// one.
    fn mode(width: u32, height: u32, format: Format, offset: u64) -> Mode {
        let stride = width * format.bytes();
        Mode {
            width,
            height,
            format,
            stride,
            offset,
            size: stride as u64 * height as u64,
        }
    }

    fn xrgb(width: u32, height: u32, offset: u64) -> Mode {
        mode(
            width,
            height,
            Format::Xrgb8888 { big_endian: false },
            offset,
        )
    }

    /// Six hundred and forty pixels of four bytes is two and a half thousand to a row,
    /// so one page of video memory is part of three rows and no row is a whole page.
    #[test]
    fn a_page_is_every_row_any_of_its_bytes_belongs_to() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 3 * PAGE, 32, 0).unwrap();
        assert_eq!(
            bands(&screen.vram().take_dirty(), &xrgb(640, 480, 0)),
            vec![4..7]
        );
    }

    #[test]
    fn two_writes_to_one_page_are_one_band() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 3 * PAGE, 32, 0).unwrap();
        card.store(FRAMEBUFFER, 3 * PAGE + 64, 32, 0).unwrap();
        assert_eq!(
            bands(&screen.vram().take_dirty(), &xrgb(640, 480, 0)),
            vec![4..7]
        );
    }

    #[test]
    fn writes_to_pages_that_meet_are_one_band() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 3 * PAGE, 32, 0).unwrap();
        card.store(FRAMEBUFFER, 4 * PAGE, 32, 0).unwrap();
        assert_eq!(
            bands(&screen.vram().take_dirty(), &xrgb(640, 480, 0)),
            vec![4..8]
        );
    }

    #[test]
    fn writes_to_pages_far_apart_are_two_bands() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 3 * PAGE, 32, 0).unwrap();
        card.store(FRAMEBUFFER, 40 * PAGE, 32, 0).unwrap();
        assert_eq!(
            bands(&screen.vram().take_dirty(), &xrgb(640, 480, 0)),
            [4..7, 64..66]
        );
    }

    /// A picture of sixty-three rows of two hundred and fifty-six bytes ends part way
    /// through the fourth page, and the rest of that page is not the picture.
    #[test]
    fn a_band_stops_at_the_last_row() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 3 * PAGE, 32, 0).unwrap();
        assert_eq!(
            bands(&screen.vram().take_dirty(), &xrgb(64, 63, 0)),
            vec![48..63]
        );
    }

    /// A driver that has panned, or that is drawing the frame after the one being
    /// shown, writes video memory the top left pixel is not in.
    #[test]
    fn what_the_guest_is_not_showing_is_no_band_at_all() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 2 * PAGE, 32, 0).unwrap();
        assert!(bands(&screen.vram().take_dirty(), &xrgb(640, 480, 8 * PAGE)).is_empty());
    }

    /// `0x00rrggbb` little endian is the bytes blue, green, red and the one the card
    /// ignores; big endian is the same word the other way round.
    #[test]
    fn a_pixel_is_read_the_way_round_the_card_keeps_it() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 0, 32, 0x0012_3456).unwrap();
        let bytes = unsafe { screen.vram().as_slice() };

        let little = Format::Xrgb8888 { big_endian: false };
        let big = Format::Xrgb8888 { big_endian: true };
        assert_eq!(
            rows(bytes, &mode(1, 1, little, 0), 0..1).pixels,
            [Color32::from_rgb(0x12, 0x34, 0x56)]
        );
        assert_eq!(
            rows(bytes, &mode(1, 1, big, 0), 0..1).pixels,
            [Color32::from_rgb(0x34, 0x12, 0x00)]
        );
    }

    /// A channel of five or six bits widens by repeating its own top bits, so that all
    /// ones is white rather than very nearly white.
    #[test]
    fn a_short_channel_widens_to_the_whole_of_its_range() {
        let (mut card, screen) = card();
        card.store(FRAMEBUFFER, 0, 16, 0xffff).unwrap();
        card.store(FRAMEBUFFER, 2, 16, 0x0821).unwrap();
        let bytes = unsafe { screen.vram().as_slice() };
        assert_eq!(
            rows(bytes, &mode(2, 1, Format::R5g6b5, 0), 0..1).pixels,
            [Color32::WHITE, Color32::from_rgb(8, 4, 8)]
        );
    }

    /// Every row of the band, and no row outside it.
    #[test]
    fn a_band_is_read_out_of_the_rows_it_names() {
        let (mut card, screen) = card();
        for row in 0..4u64 {
            card.store(FRAMEBUFFER, row * 8, 32, row + 1).unwrap();
        }
        let bytes = unsafe { screen.vram().as_slice() };
        let picture = rows(bytes, &xrgb(2, 4, 0), 1..3);
        assert_eq!(picture.size, [2, 2]);
        assert_eq!(
            picture.pixels,
            [
                Color32::from_rgb(0, 0, 2),
                Color32::BLACK,
                Color32::from_rgb(0, 0, 3),
                Color32::BLACK,
            ]
        );
    }
}
