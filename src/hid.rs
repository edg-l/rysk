//! The two devices behind the controller: a keyboard and a mouse.
//!
//! Both are the same shape. One interface of the human interface device class, one
//! interrupt IN endpoint, and a report descriptor saying what the bytes that come out
//! of that endpoint mean. Neither has an OUT endpoint: a keyboard's lamps arrive as a
//! report on the control endpoint, which is what the boot subclass requires of a
//! device meant to work before a driver has been loaded.
//!
//! The reports are the boot protocol's, and the report descriptors describe exactly
//! them, so a host that puts the device into either protocol gets the same bytes from
//! the keyboard. The mouse differs by one byte, which is the wheel: a boot mouse has
//! none and every real mouse does, so `SET_PROTOCOL` is a request this device acts on
//! rather than accepts and ignores.
//!
//! Device Class Definition for Human Interface Devices 1.11 defines the descriptors and
//! the class requests; appendices B.1 and B.2 of it are where the two report
//! descriptors below come from.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::usb::{self, Answer, Configured, Descriptors, Device, Setup, Speed, Stall, TYPE_CLASS};

/// The class, the subclass that says the reports are the boot protocol's, and the two
/// protocols within it.
const CLASS: u8 = 3;
const BOOT: u8 = 1;
const KEYBOARD: u8 = 1;
const MOUSE: u8 = 2;

/// The two descriptor types the class adds: what the interface says about its reports,
/// and the report descriptor itself.
const DT_HID: u8 = 0x21;
const DT_REPORT: u8 = 0x22;

/// Which report a class request is about, which is the high byte of its value.
/// Device Class Definition for Human Interface Devices 1.11, 7.2.1.
const REPORT_INPUT: u8 = 1;
const REPORT_OUTPUT: u8 = 2;

/// The class requests. Device Class Definition for Human Interface Devices 1.11, 7.2.
const GET_REPORT: u8 = 0x01;
const GET_IDLE: u8 = 0x02;
const GET_PROTOCOL: u8 = 0x03;
const SET_REPORT: u8 = 0x09;
const SET_IDLE: u8 = 0x0a;
const SET_PROTOCOL: u8 = 0x0b;

/// Which of the two layouts the device is reporting in.
///
/// A device powers up in report protocol whatever its subclass says, and a host that
/// wants the other one asks for it. Device Class Definition for Human Interface
/// Devices 1.11, 7.2.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Protocol {
    Boot,
    #[default]
    Report,
}

/// The endpoint the reports come out of, and how often a host is asked to look.
const ENDPOINT: u8 = 0x81;
const INTERVAL: u8 = 10;
const MAX_PACKET: u16 = 8;

/// The largest control transfer either device answers, which is what endpoint zero is
/// told it can carry. Eight is what a full-speed device usually says, and it makes a
/// driver read the first eight bytes of the device descriptor and then ask again.
const CONTROL_PACKET: u8 = 8;

/// The identity these report. Nothing binds on it: `usbhid` matches the interface's
/// class, so this is only what `lsusb` prints, and it says rysk rather than borrowing
/// a number that belongs to somebody.
const VENDOR: u16 = 0x1234;
const KEYBOARD_PRODUCT: u16 = 0x0001;
const MOUSE_PRODUCT: u16 = 0x0002;

/// How many reports may be waiting before the oldest is dropped.
///
/// A guest that has not asked for one in a while has a driver that is not running, and
/// keeping every keystroke since then would deliver a burst of them the moment it
/// starts. Deep enough that nothing is lost between two polls of an endpoint asked
/// about every ten milliseconds.
const QUEUE: usize = 16;

/// The end of a device that faces the world: what a frontend presses, moves and clicks.
///
/// Shared, because whoever is doing the typing is not the hart, which is the same split
/// the serial port's `Keyboard` makes. What goes in is a whole report rather than an
/// event, since a report is what the device sends and working one out from the last is
/// the frontend's job: which keys are still down is something only it knows.
#[derive(Debug, Clone, Default)]
pub struct Reports(Arc<Mutex<VecDeque<Vec<u8>>>>);

impl Reports {
    fn push(&self, report: Vec<u8>) {
        let mut queue = self.0.lock().unwrap();
        if queue.len() >= QUEUE {
            queue.pop_front();
        }
        queue.push_back(report);
    }

    fn take(&self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().pop_front()
    }
}

/// What a frontend does to the keyboard.
#[derive(Debug, Clone, Default)]
pub struct Keys(Reports);

impl Keys {
    /// Say what the keyboard is reporting now: which modifiers are held, and up to six
    /// keys that are down, as HID usage codes from the keyboard page.
    ///
    /// A key going down and coming up again is two of these, and a report of nothing is
    /// what says every key was released.
    pub fn holding(&self, modifiers: u8, keys: &[u8]) {
        let mut report = vec![modifiers, 0];
        report.extend(keys.iter().take(6));
        report.resize(8, 0);
        self.0.push(report);
    }

    /// One key struck: the report that has it down, and the report that has it up.
    pub fn typed(&self, modifiers: u8, key: u8) {
        self.holding(modifiers, &[key]);
        self.holding(0, &[]);
    }
}

/// And to the mouse: which buttons are down, how far it moved since the last report,
/// and how far the wheel turned.
#[derive(Debug, Clone, Default)]
pub struct Pointer(Reports);

impl Pointer {
    pub fn moved(&self, buttons: u8, x: i8, y: i8, wheel: i8) {
        self.0.push(vec![buttons, x as u8, y as u8, wheel as u8]);
    }
}

/// The report descriptor of a keyboard whose reports are the boot protocol's: a byte of
/// modifiers as eight bits, a byte the device does not use, five lamps and three bits of
/// padding going the other way, and six bytes of whichever keys are down.
/// Device Class Definition for Human Interface Devices 1.11, appendix B.1.
#[rustfmt::skip]
const KEYBOARD_REPORT: &[u8] = &[
    0x05, 0x01,        // usage page: generic desktop
    0x09, 0x06,        // usage: keyboard
    0xa1, 0x01,        // collection: application
    0x05, 0x07,        //   usage page: keyboard
    0x19, 0xe0,        //   usage minimum: left control
    0x29, 0xe7,        //   usage maximum: right meta
    0x15, 0x00,        //   logical minimum: 0
    0x25, 0x01,        //   logical maximum: 1
    0x75, 0x01,        //   report size: 1
    0x95, 0x08,        //   report count: 8
    0x81, 0x02,        //   input: the eight modifiers, one bit each
    0x95, 0x01,        //   report count: 1
    0x75, 0x08,        //   report size: 8
    0x81, 0x01,        //   input: constant, which is the byte nothing uses
    0x95, 0x05,        //   report count: 5
    0x75, 0x01,        //   report size: 1
    0x05, 0x08,        //   usage page: lamps
    0x19, 0x01,        //   usage minimum: num lock
    0x29, 0x05,        //   usage maximum: kana
    0x91, 0x02,        //   output: the five lamps
    0x95, 0x01,        //   report count: 1
    0x75, 0x03,        //   report size: 3
    0x91, 0x01,        //   output: constant, padding the lamps to a byte
    0x95, 0x06,        //   report count: 6
    0x75, 0x08,        //   report size: 8
    0x15, 0x00,        //   logical minimum: 0
    0x25, 0x65,        //   logical maximum: 101
    0x05, 0x07,        //   usage page: keyboard
    0x19, 0x00,        //   usage minimum: none
    0x29, 0x65,        //   usage maximum: application
    0x81, 0x00,        //   input: six keys, as an array of what is down
    0xc0,              // end collection
];

/// And of a mouse: three buttons and five bits of padding, then how far it moved in
/// each direction and how far the wheel turned.
///
/// The wheel is the one thing here the boot protocol has no room for, which is why the
/// device answers `GET_PROTOCOL` with something that matters.
/// Device Class Definition for Human Interface Devices 1.11, appendix B.2.
#[rustfmt::skip]
const MOUSE_REPORT: &[u8] = &[
    0x05, 0x01,        // usage page: generic desktop
    0x09, 0x02,        // usage: mouse
    0xa1, 0x01,        // collection: application
    0x09, 0x01,        //   usage: pointer
    0xa1, 0x00,        //   collection: physical
    0x05, 0x09,        //     usage page: buttons
    0x19, 0x01,        //     usage minimum: button one
    0x29, 0x03,        //     usage maximum: button three
    0x15, 0x00,        //     logical minimum: 0
    0x25, 0x01,        //     logical maximum: 1
    0x95, 0x03,        //     report count: 3
    0x75, 0x01,        //     report size: 1
    0x81, 0x02,        //     input: the three buttons
    0x95, 0x01,        //     report count: 1
    0x75, 0x05,        //     report size: 5
    0x81, 0x01,        //     input: constant, padding them to a byte
    0x05, 0x01,        //     usage page: generic desktop
    0x09, 0x30,        //     usage: x
    0x09, 0x31,        //     usage: y
    0x09, 0x38,        //     usage: wheel
    0x15, 0x81,        //     logical minimum: -127
    0x25, 0x7f,        //     logical maximum: 127
    0x75, 0x08,        //     report size: 8
    0x95, 0x03,        //     report count: 3
    0x81, 0x06,        //     input: how far each moved since the last report
    0xc0,              //   end collection
    0xc0,              // end collection
];

/// The descriptor an interface of this class carries, which says which class
/// specification it follows and how long its report descriptor is.
/// Device Class Definition for Human Interface Devices 1.11, 6.2.1.
fn hid_descriptor(report: &[u8]) -> Vec<u8> {
    vec![
        9,
        DT_HID,
        // Version 1.11, and no country the keys are laid out for.
        0x11,
        0x01,
        0,
        1,
        DT_REPORT,
        report.len() as u8,
        (report.len() >> 8) as u8,
    ]
}

/// The whole of what `GET_DESCRIPTOR` for a configuration answers: the configuration,
/// the one interface in it, what that interface says about its reports, and the one
/// endpoint the reports come out of.
fn configuration(protocol: u8, report: &[u8]) -> Vec<u8> {
    let mut bytes = vec![
        9,
        usb::DT_CONFIGURATION,
        // The total length, filled in below once everything is in.
        0,
        0,
        // One interface, configuration number one, and no string naming it.
        1,
        1,
        0,
        // Bus powered, which is the one bit of this byte that has to be set, and
        // fifty units of two milliamps.
        0x80,
        50,
    ];
    bytes.extend_from_slice(&[
        9,
        usb::DT_INTERFACE,
        // Interface zero, its only setting, and its one endpoint.
        0,
        0,
        1,
        CLASS,
        BOOT,
        protocol,
        0,
    ]);
    bytes.extend_from_slice(&hid_descriptor(report));
    bytes.extend_from_slice(&[
        7,
        usb::DT_ENDPOINT,
        ENDPOINT,
        // An interrupt endpoint, how much it sends at once, and how often to ask.
        3,
        MAX_PACKET as u8,
        (MAX_PACKET >> 8) as u8,
        INTERVAL,
    ]);
    let total = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&total.to_le_bytes());
    bytes
}

/// The eighteen bytes that say what the device is.
fn device_descriptor(product: u16) -> Vec<u8> {
    let mut bytes = vec![18, usb::DT_DEVICE];
    // Version 2.0 of the bus, and a class said by the interface rather than here,
    // which is what a device whose interfaces could be of different classes says.
    bytes.extend_from_slice(&0x0200u16.to_le_bytes());
    bytes.extend_from_slice(&[0, 0, 0, CONTROL_PACKET]);
    bytes.extend_from_slice(&VENDOR.to_le_bytes());
    bytes.extend_from_slice(&product.to_le_bytes());
    // Its own version, then the three strings and the one configuration.
    bytes.extend_from_slice(&0x0100u16.to_le_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 1]);
    bytes
}

/// One of the two devices. They differ in their descriptors, in which report they
/// answer with, and in whether a protocol change means anything.
#[derive(Debug)]
pub struct Hid {
    descriptors: Descriptors,
    report: &'static [u8],
    state: Configured,
    protocol: Protocol,
    /// How often the device should repeat a report nothing has changed in, in units of
    /// four milliseconds, with zero meaning never. Storage: nothing here repeats a
    /// report, because nothing here has a clock of its own to repeat it against.
    idle: u8,
    /// Which lamps a host has asked for, for a device that has an output report to ask
    /// about. The keyboard has one and has no lamps to light; the mouse has neither,
    /// and refuses a request about a report its descriptor does not describe.
    lamps: Option<u8>,
    /// What the device is sending now, which is what it last reported until a frontend
    /// says otherwise. An endpoint asked twice between two reports answers once.
    reports: Reports,
    /// How many bytes of a report the protocol it is in has.
    boot_length: usize,
}

impl Hid {
    /// A keyboard, and the handle a frontend types on.
    pub fn keyboard() -> (Self, Keys) {
        let keys = Keys::default();
        let hid = Self::new(
            KEYBOARD,
            KEYBOARD_PRODUCT,
            "keyboard",
            KEYBOARD_REPORT,
            keys.0.clone(),
            8,
            Some(0),
        );
        (hid, keys)
    }

    /// A mouse, and the handle a frontend moves.
    pub fn mouse() -> (Self, Pointer) {
        let pointer = Pointer::default();
        let hid = Self::new(
            MOUSE,
            MOUSE_PRODUCT,
            "mouse",
            MOUSE_REPORT,
            pointer.0.clone(),
            3,
            None,
        );
        (hid, pointer)
    }

    fn new(
        protocol: u8,
        product: u16,
        name: &str,
        report: &'static [u8],
        reports: Reports,
        boot_length: usize,
        lamps: Option<u8>,
    ) -> Self {
        Self {
            descriptors: Descriptors {
                device: device_descriptor(product),
                configuration: configuration(protocol, report),
                strings: vec![
                    usb::languages(&[usb::ENGLISH]),
                    usb::string("rysk"),
                    usb::string(name),
                    usb::string("0"),
                ],
            },
            report,
            state: Configured::default(),
            protocol: Protocol::default(),
            idle: 0,
            lamps,
            reports,
            boot_length,
        }
    }

    /// The descriptors an interface of this class answers for, which are asked for
    /// through the interface rather than through the device.
    /// Device Class Definition for Human Interface Devices 1.11, 7.1.1.
    fn interface_descriptor(&self, setup: Setup) -> Answer {
        let (kind, _) = setup.descriptor();
        let bytes = match kind {
            DT_REPORT => self.report,
            DT_HID => return Ok(hid_descriptor(self.report)),
            _ => return Err(Stall),
        };
        Ok(bytes[..bytes.len().min(setup.length as usize)].to_vec())
    }

    /// The class requests. Device Class Definition for Human Interface Devices 1.11, 7.2.
    fn class(&mut self, setup: Setup, out: &[u8]) -> Answer {
        match setup.request {
            // The report an endpoint would have sent, asked for over the control
            // endpoint instead. A device with nothing to report answers with what it is
            // reporting now, which is a report of nothing held.
            GET_REPORT => match (setup.value >> 8) as u8 {
                REPORT_INPUT => Ok(self.pending().unwrap_or_else(|| vec![0; self.length()])),
                // The lamps, which this keyboard has none of and remembers anyway, so
                // that a host reading them back is told what it asked for.
                REPORT_OUTPUT => self.lamps.map(|lamps| vec![lamps]).ok_or(Stall),
                _ => Err(Stall),
            },
            SET_REPORT => match ((setup.value >> 8) as u8, self.lamps) {
                (REPORT_OUTPUT, Some(_)) => {
                    self.lamps = Some(out.first().copied().unwrap_or(0));
                    Ok(Vec::new())
                }
                // A report a device's descriptor does not describe is one it has
                // nowhere to put.
                _ => Err(Stall),
            },
            SET_IDLE => {
                self.idle = (setup.value >> 8) as u8;
                Ok(Vec::new())
            }
            GET_IDLE => Ok(vec![self.idle]),
            SET_PROTOCOL => {
                self.protocol = match setup.value {
                    0 => Protocol::Boot,
                    _ => Protocol::Report,
                };
                Ok(Vec::new())
            }
            GET_PROTOCOL => Ok(vec![(self.protocol == Protocol::Report) as u8]),
            _ => Err(Stall),
        }
    }

    /// How many bytes of a report the protocol the device is in has. They are the same
    /// for a keyboard and differ by the wheel for a mouse.
    fn length(&self) -> usize {
        match self.protocol {
            Protocol::Boot => self.boot_length,
            Protocol::Report => self.reported_length(),
        }
    }

    /// How long a report is in report protocol, which is what a frontend puts in.
    fn reported_length(&self) -> usize {
        match self.boot_length {
            // A mouse's wheel is the byte the boot protocol has no room for.
            3 => 4,
            other => other,
        }
    }

    /// The next report, cut to the protocol the device is in.
    fn pending(&mut self) -> Option<Vec<u8>> {
        let mut report = self.reports.take()?;
        report.resize(self.length(), 0);
        Some(report)
    }
}

impl Device for Hid {
    /// Full speed, which is what a device with one interrupt endpoint of eight bytes
    /// every ten milliseconds has no reason to be faster than.
    fn speed(&self) -> Speed {
        Speed::Full
    }

    fn control(&mut self, setup: Setup, out: &[u8]) -> Answer {
        // A descriptor asked for through the interface is the class's, and one asked
        // for through the device is the device's, which is the same request number
        // meaning two things depending on what it is addressed to.
        if setup.kind() == usb::TYPE_STANDARD
            && setup.request == usb::GET_DESCRIPTOR
            && setup.request_type & usb::RECIPIENT_MASK == usb::RECIPIENT_INTERFACE
        {
            return self.interface_descriptor(setup);
        }
        if let Some(answer) = usb::standard(&self.descriptors, &mut self.state, setup) {
            return answer;
        }
        match setup.kind() {
            TYPE_CLASS => self.class(setup, out),
            _ => Err(Stall),
        }
    }

    fn read(&mut self, endpoint: u8) -> Option<Vec<u8>> {
        (endpoint == ENDPOINT).then(|| self.pending())?
    }
}
