//! What a host key press is, as the keyboard on the bus understands it.
//!
//! `hid` is the keyboard and the mouse as the controller sees them: a report saying
//! which keys are down and how far the mouse moved. This is the other end, the one a
//! window pushes into, and almost the whole of the difference between them is a table.
//!
//! The keys are *physical positions* rather than the characters they produce. A guest
//! has a keymap of its own and applying the host's first would mean applying two: a key
//! is reported by where it is on the board, the guest decides what it means, and a
//! guest set to Dvorak behaves as a real board plugged into it would.
//!
//! What the window cannot forward is what `egui` has no physical key for: the numeric
//! keypad, caps lock, print screen, scroll lock, pause and the menu key. They are
//! positions its `Key` does not name, so nothing here can name them either.
//!
//! The usage codes are the USB HID Usage Tables, Keyboard/Keypad page (`0x07`).

use eframe::egui::{Key, MouseWheelUnit, PointerButton, Vec2};

use crate::hid::{Keys, Pointer};

/// How many keys a boot-protocol report carries, past the modifiers.
const ROLLOVER: usize = 6;

/// What every key slot reads as when more than `ROLLOVER` of them are down, which is a
/// keyboard saying it cannot tell which. `ErrorRollOver`, usage 1.
const TOO_MANY: u8 = 0x01;

/// The first of the eight modifiers, which are usages `0xe0` to `0xe7` and are reported
/// as one bit each rather than as keys.
const MODIFIERS: u8 = 0xe0;

/// The buttons of a boot-protocol mouse, one bit each in the order the report has them.
const LEFT: u8 = 1 << 0;
const RIGHT: u8 = 1 << 1;
const MIDDLE: u8 = 1 << 2;

/// How far a boot-protocol report can say the mouse moved, in either direction. What
/// went further than this takes more than one report.
const REACH: f32 = i8::MAX as f32;

/// What one click of a wheel is, in each of the three units a host measures scrolling
/// in. The report carries clicks, so anything smoother has to be gathered into them.
///
/// A notch is three lines by long convention -- Windows counts a wheel in one-hundred-
/// and-twentieths and calls that three lines -- and about fifty logical pixels in a
/// desktop toolkit. A page is rare and approximate: eight notches, which is roughly
/// what a screenful is.
const POINTS: f32 = 50.0;
const LINES: f32 = 3.0;
const PAGES: f32 = 1.0 / 8.0;

/// The keyboard as a window drives it.
#[derive(Debug)]
pub struct Keyboard {
    keys: Option<Keys>,
    /// The usage codes currently down, oldest first. Which six of them a report carries
    /// is which six went down first, so the order is what decides and a `Vec` is what
    /// keeps it.
    held: Vec<u8>,
}

impl Keyboard {
    pub fn new(keys: Option<Keys>) -> Self {
        Self {
            keys,
            held: Vec::new(),
        }
    }

    /// A physical key went down or came up. Answers whether the guest was told, which
    /// is false for a key this machine has no usage code for.
    ///
    /// A key already down going down again is a repeat and is not reported: a keyboard
    /// sends one report when a key goes down and the next when something changes, and
    /// the repeating is the guest's own. Forwarding the host's would repeat twice.
    pub fn key(&mut self, key: Key, pressed: bool) -> bool {
        let Some(usage) = usage(key) else {
            return false;
        };
        let held = self.held.iter().position(|&held| held == usage);
        match (pressed, held) {
            (true, None) => self.held.push(usage),
            (false, Some(at)) => drop(self.held.remove(at)),
            _ => return false,
        }
        self.report();
        true
    }

    /// Everything is up, which is what a window losing focus means: a key held while
    /// the guest stops being told about it would otherwise stay down for ever.
    pub fn released(&mut self) {
        if !self.held.is_empty() {
            self.held.clear();
            self.report();
        }
    }

    /// Say what is down now.
    fn report(&self) {
        let Some(keys) = &self.keys else {
            return;
        };
        let mut modifiers = 0u8;
        let mut down = Vec::with_capacity(ROLLOVER);
        for &usage in &self.held {
            match usage.checked_sub(MODIFIERS) {
                Some(bit) => modifiers |= 1 << bit,
                None => down.push(usage),
            }
        }
        // A board that cannot say which keys are down says so, rather than saying six
        // of them and dropping the rest. The modifiers are still exact, since they are
        // bits rather than slots.
        if down.len() > ROLLOVER {
            down = vec![TOO_MANY; ROLLOVER];
        }
        keys.holding(modifiers, &down);
    }
}

/// The mouse as a window drives it.
///
/// A frame's worth of movement is gathered before any of it is sent, because a report
/// carries one byte in each direction and a frame can hold many events. What is left
/// over after a report goes into the next one, so a fast drag arrives as several
/// reports rather than as a clamped one.
#[derive(Debug)]
pub struct Mouse {
    pointer: Option<Pointer>,
    buttons: u8,
    x: f32,
    y: f32,
    wheel: f32,
}

impl Mouse {
    pub fn new(pointer: Option<Pointer>) -> Self {
        Self {
            pointer,
            buttons: 0,
            x: 0.0,
            y: 0.0,
            wheel: 0.0,
        }
    }

    /// The mouse moved, in whatever the host counts movement in. This is the raw
    /// motion rather than where a cursor ended up, since a mouse has no idea where a
    /// cursor is and a guest moves its own.
    pub fn moved(&mut self, delta: Vec2) {
        self.x += delta.x;
        self.y += delta.y;
    }

    /// The wheel turned, by `delta` of whatever the host counts scrolling in. Positive
    /// is away from the hand, which is what both a host toolkit and the report mean by
    /// scrolling up.
    pub fn wheel(&mut self, unit: MouseWheelUnit, delta: Vec2) {
        self.wheel += delta.y
            / match unit {
                MouseWheelUnit::Point => POINTS,
                MouseWheelUnit::Line => LINES,
                MouseWheelUnit::Page => PAGES,
            };
    }

    /// A button went down or came up, which is reported at once rather than gathered:
    /// a click and the release after it are two things a guest has to see separately.
    pub fn button(&mut self, button: PointerButton, pressed: bool) {
        let bit = match button {
            PointerButton::Primary => LEFT,
            PointerButton::Secondary => RIGHT,
            PointerButton::Middle => MIDDLE,
            // The two extra buttons are not in a boot-protocol report.
            _ => return,
        };
        match pressed {
            true => self.buttons |= bit,
            false => self.buttons &= !bit,
        }
        self.send(0, 0, 0);
    }

    /// Send what has gathered, which is what ends a frame. Movement further than one
    /// report reaches becomes several, and what is left over waits for the next frame.
    pub fn flush(&mut self) {
        while self.x.abs() >= 1.0 || self.y.abs() >= 1.0 || self.wheel.abs() >= 1.0 {
            let x = self.x.clamp(-REACH, REACH).trunc();
            let y = self.y.clamp(-REACH, REACH).trunc();
            let wheel = self.wheel.clamp(-REACH, REACH).trunc();
            self.x -= x;
            self.y -= y;
            self.wheel -= wheel;
            self.send(x as i8, y as i8, wheel as i8);
        }
    }

    fn send(&self, x: i8, y: i8, wheel: i8) {
        if let Some(pointer) = &self.pointer {
            pointer.moved(self.buttons, x, y, wheel);
        }
    }
}

/// The usage code for a physical key, if the keyboard page has one.
///
/// A few of `egui`'s keys name a character rather than a position -- a colon, a pipe, a
/// question mark. Those never arrive as a physical key, since a physical key is a
/// place; they are mapped anyway, to the place a US board produces them from, so that
/// an integration which does send one is not silently dropped.
fn usage(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => 0x04,
        Key::B => 0x05,
        Key::C => 0x06,
        Key::D => 0x07,
        Key::E => 0x08,
        Key::F => 0x09,
        Key::G => 0x0a,
        Key::H => 0x0b,
        Key::I => 0x0c,
        Key::J => 0x0d,
        Key::K => 0x0e,
        Key::L => 0x0f,
        Key::M => 0x10,
        Key::N => 0x11,
        Key::O => 0x12,
        Key::P => 0x13,
        Key::Q => 0x14,
        Key::R => 0x15,
        Key::S => 0x16,
        Key::T => 0x17,
        Key::U => 0x18,
        Key::V => 0x19,
        Key::W => 0x1a,
        Key::X => 0x1b,
        Key::Y => 0x1c,
        Key::Z => 0x1d,

        // The digit row, which starts at one and puts zero after nine.
        Key::Num1 | Key::Exclamationmark => 0x1e,
        Key::Num2 => 0x1f,
        Key::Num3 => 0x20,
        Key::Num4 => 0x21,
        Key::Num5 => 0x22,
        Key::Num6 => 0x23,
        Key::Num7 => 0x24,
        Key::Num8 => 0x25,
        Key::Num9 => 0x26,
        Key::Num0 => 0x27,

        Key::Enter => 0x28,
        Key::Escape => 0x29,
        Key::Backspace => 0x2a,
        Key::Tab => 0x2b,
        Key::Space => 0x2c,

        Key::Minus => 0x2d,
        Key::Equals | Key::Plus => 0x2e,
        Key::OpenBracket | Key::OpenCurlyBracket => 0x2f,
        Key::CloseBracket | Key::CloseCurlyBracket => 0x30,
        Key::Backslash | Key::Pipe => 0x31,
        Key::Semicolon | Key::Colon => 0x33,
        Key::Quote => 0x34,
        Key::Backtick => 0x35,
        Key::Comma => 0x36,
        Key::Period => 0x37,
        Key::Slash | Key::Questionmark => 0x38,

        Key::F1 => 0x3a,
        Key::F2 => 0x3b,
        Key::F3 => 0x3c,
        Key::F4 => 0x3d,
        Key::F5 => 0x3e,
        Key::F6 => 0x3f,
        Key::F7 => 0x40,
        Key::F8 => 0x41,
        Key::F9 => 0x42,
        Key::F10 => 0x43,
        Key::F11 => 0x44,
        Key::F12 => 0x45,

        Key::Insert => 0x49,
        Key::Home => 0x4a,
        Key::PageUp => 0x4b,
        Key::Delete => 0x4c,
        Key::End => 0x4d,
        Key::PageDown => 0x4e,
        Key::ArrowRight => 0x4f,
        Key::ArrowLeft => 0x50,
        Key::ArrowDown => 0x51,
        Key::ArrowUp => 0x52,

        // The key an ISO board has between the left shift and the Z, which a US board
        // does not have at all.
        Key::IntlBackslash => 0x64,

        Key::F13 => 0x68,
        Key::F14 => 0x69,
        Key::F15 => 0x6a,
        Key::F16 => 0x6b,
        Key::F17 => 0x6c,
        Key::F18 => 0x6d,
        Key::F19 => 0x6e,
        Key::F20 => 0x6f,
        Key::F21 => 0x70,
        Key::F22 => 0x71,
        Key::F23 => 0x72,
        Key::F24 => 0x73,

        // The eight modifiers, which a report carries as bits rather than as keys.
        Key::ControlLeft => 0xe0,
        Key::ShiftLeft => 0xe1,
        Key::AltLeft => 0xe2,
        Key::SuperLeft => 0xe3,
        Key::ControlRight => 0xe4,
        Key::ShiftRight => 0xe5,
        Key::AltRight => 0xe6,
        Key::SuperRight => 0xe7,

        // Keys past F24 have no usage on this page, and the editing and browser keys
        // are not places on a board.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hid::Hid, usb::Device};

    /// The endpoint a device's reports come out of.
    const REPORTS: u8 = 0x81;

    /// A keyboard driven from the window, and the device a controller would read.
    fn keyboard() -> (Keyboard, Hid) {
        let (hid, keys) = Hid::keyboard();
        (Keyboard::new(Some(keys)), hid)
    }

    fn mouse() -> (Mouse, Hid) {
        let (hid, pointer) = Hid::mouse();
        (Mouse::new(Some(pointer)), hid)
    }

    /// Every report the device has to hand, oldest first.
    fn reports(hid: &mut Hid) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| hid.read(REPORTS)).collect()
    }

    #[test]
    fn a_key_down_and_up_is_two_reports() {
        let (mut board, mut hid) = keyboard();
        board.key(Key::A, true);
        board.key(Key::A, false);

        assert_eq!(
            reports(&mut hid),
            [
                vec![0, 0, 0x04, 0, 0, 0, 0, 0],
                vec![0, 0, 0, 0, 0, 0, 0, 0]
            ]
        );
    }

    /// The host repeats a held key and the guest repeats it too, so forwarding the
    /// host's would repeat it twice.
    #[test]
    fn a_key_already_down_going_down_again_says_nothing() {
        let (mut board, mut hid) = keyboard();
        assert!(board.key(Key::A, true));
        assert!(!board.key(Key::A, true));

        assert_eq!(reports(&mut hid).len(), 1);
    }

    #[test]
    fn a_modifier_is_a_bit_and_not_a_key() {
        let (mut board, mut hid) = keyboard();
        board.key(Key::ControlLeft, true);
        board.key(Key::C, true);

        assert_eq!(
            reports(&mut hid),
            [
                vec![0x01, 0, 0, 0, 0, 0, 0, 0],
                vec![0x01, 0, 0x06, 0, 0, 0, 0, 0]
            ]
        );
    }

    #[test]
    fn each_modifier_has_its_own_bit() {
        let (mut board, mut hid) = keyboard();
        for key in [
            Key::ControlLeft,
            Key::ShiftLeft,
            Key::AltLeft,
            Key::SuperLeft,
            Key::ControlRight,
            Key::ShiftRight,
            Key::AltRight,
            Key::SuperRight,
        ] {
            board.key(key, true);
        }

        assert_eq!(reports(&mut hid).last().expect("a report")[0], 0xff);
    }

    /// Six is what the slots hold; a seventh means the board cannot say which.
    #[test]
    fn more_keys_than_the_report_holds_is_a_rollover() {
        let (mut board, mut hid) = keyboard();
        for key in [Key::A, Key::B, Key::C, Key::D, Key::E, Key::F] {
            board.key(key, true);
        }
        assert_eq!(
            reports(&mut hid).last().expect("a report"),
            &vec![0, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09]
        );

        board.key(Key::G, true);
        assert_eq!(
            reports(&mut hid).last().expect("a report"),
            &vec![0, 0, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01]
        );
    }

    /// Modifiers are bits rather than slots, so they do not fill the report and a
    /// rollover still says which of them are down.
    #[test]
    fn a_rollover_still_reports_the_modifiers() {
        let (mut board, mut hid) = keyboard();
        board.key(Key::ShiftLeft, true);
        for key in [Key::A, Key::B, Key::C, Key::D, Key::E, Key::F, Key::G] {
            board.key(key, true);
        }

        let last = reports(&mut hid).last().expect("a report").clone();
        assert_eq!(last[0], 0x02, "left shift is still exact");
        assert_eq!(&last[2..], [0x01; 6]);
    }

    #[test]
    fn losing_focus_lets_go_of_everything() {
        let (mut board, mut hid) = keyboard();
        board.key(Key::ShiftLeft, true);
        board.key(Key::A, true);
        board.released();

        assert_eq!(
            reports(&mut hid).last().expect("a report"),
            &vec![0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_key_the_page_has_no_usage_for_is_not_forwarded() {
        let (mut board, mut hid) = keyboard();
        assert!(!board.key(Key::F35, true));
        assert!(!board.key(Key::BrowserBack, true));

        assert!(reports(&mut hid).is_empty());
    }

    #[test]
    fn a_frame_of_movement_is_one_report() {
        let (mut mouse, mut hid) = mouse();
        mouse.moved(Vec2::new(3.0, 4.0));
        mouse.moved(Vec2::new(1.0, -2.0));
        mouse.flush();

        assert_eq!(reports(&mut hid), [vec![0, 4, 2, 0]]);
    }

    /// A report carries one byte each way, so a drag further than that is several of
    /// them and nothing is lost.
    #[test]
    fn movement_further_than_a_report_reaches_is_several() {
        let (mut mouse, mut hid) = mouse();
        mouse.moved(Vec2::new(300.0, 0.0));
        mouse.flush();

        let reports = reports(&mut hid);
        assert_eq!(reports.len(), 3);
        let moved: i32 = reports.iter().map(|report| report[1] as i8 as i32).sum();
        assert_eq!(moved, 300);
    }

    #[test]
    fn movement_of_less_than_a_pixel_waits_for_the_next_frame() {
        let (mut mouse, mut hid) = mouse();
        mouse.moved(Vec2::new(0.4, 0.0));
        mouse.flush();
        assert!(reports(&mut hid).is_empty());

        mouse.moved(Vec2::new(0.7, 0.0));
        mouse.flush();
        assert_eq!(reports(&mut hid), [vec![0, 1, 0, 0]]);
    }

    #[test]
    fn a_button_is_reported_the_moment_it_moves() {
        let (mut mouse, mut hid) = mouse();
        mouse.button(PointerButton::Primary, true);
        mouse.button(PointerButton::Secondary, true);
        mouse.button(PointerButton::Primary, false);

        assert_eq!(
            reports(&mut hid),
            [
                vec![LEFT, 0, 0, 0],
                vec![LEFT | RIGHT, 0, 0, 0],
                vec![RIGHT, 0, 0, 0]
            ]
        );
    }

    #[test]
    fn a_held_button_rides_along_with_the_movement() {
        let (mut mouse, mut hid) = mouse();
        mouse.button(PointerButton::Primary, true);
        mouse.moved(Vec2::new(5.0, 0.0));
        mouse.flush();

        assert_eq!(
            reports(&mut hid).last().expect("a report"),
            &vec![LEFT, 5, 0, 0]
        );
    }

    /// A wheel is counted in clicks whatever the host counts it in.
    #[test]
    fn every_unit_of_scrolling_becomes_the_same_click() {
        for (unit, delta) in [
            (MouseWheelUnit::Point, POINTS),
            (MouseWheelUnit::Line, LINES),
            (MouseWheelUnit::Page, PAGES),
        ] {
            let (mut mouse, mut hid) = mouse();
            mouse.wheel(unit, Vec2::new(0.0, delta * 2.0));
            mouse.flush();

            assert_eq!(reports(&mut hid), [vec![0, 0, 0, 2]], "{unit:?}");
        }
    }

    #[test]
    fn part_of_a_click_is_not_a_click_yet() {
        let (mut mouse, mut hid) = mouse();
        mouse.wheel(MouseWheelUnit::Point, Vec2::new(0.0, POINTS * 0.5));
        mouse.flush();
        assert!(reports(&mut hid).is_empty());

        mouse.wheel(MouseWheelUnit::Point, Vec2::new(0.0, POINTS * 0.5));
        mouse.flush();
        assert_eq!(reports(&mut hid), [vec![0, 0, 0, 1]]);
    }

    /// Scrolling sideways is not something a boot-protocol report can say.
    #[test]
    fn a_sideways_wheel_says_nothing() {
        let (mut mouse, mut hid) = mouse();
        mouse.wheel(MouseWheelUnit::Point, Vec2::new(POINTS * 4.0, 0.0));
        mouse.flush();

        assert!(reports(&mut hid).is_empty());
    }
}
