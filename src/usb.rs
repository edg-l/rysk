//! What a USB device is, seen from the controller that drives it.
//!
//! A device here is not a model of a wire. It is asked questions and answers them: a
//! control transfer arrives as the eight bytes of its setup packet and whatever went out
//! with it, and an IN endpoint is asked whether it has anything to send yet. Everything
//! below that -- packets, tokens, handshakes, the frame a transfer was scheduled in --
//! belongs to a bus nothing here has, and a controller that finishes a transfer the
//! instant it is asked to start one is indistinguishable from a fast one.
//!
//! What is modelled is the part software can see: the descriptors a device answers with,
//! the state a standard request changes, and the report an interrupt endpoint delivers.
//!
//! Universal Serial Bus Specification 2.0, chapter 9, defines the requests and the
//! descriptors. `include/uapi/linux/usb/ch9.h` is the same thing as a driver reads it.

use std::fmt;

/// How fast a device runs. The controller reports this in the port register the device
/// is attached to and copies it into the slot context, and a driver reads it to decide
/// how large a control transfer's packets may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
}

impl Speed {
    /// The number xHCI gives it, which is what a port register and a slot context both
    /// hold. xHCI 1.2, table 5-11.
    pub fn code(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Low => 2,
            Self::High => 3,
            Self::Super => 4,
        }
    }
}

/// The eight bytes every control transfer starts with.
/// Universal Serial Bus Specification 2.0, 9.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl Setup {
    /// The packet those eight bytes spell.
    pub fn parse(bytes: [u8; 8]) -> Self {
        Self {
            request_type: bytes[0],
            request: bytes[1],
            value: u16::from_le_bytes([bytes[2], bytes[3]]),
            index: u16::from_le_bytes([bytes[4], bytes[5]]),
            length: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }

    /// Whether the data stage goes to the host.
    pub fn to_host(self) -> bool {
        self.request_type & DIRECTION_IN != 0
    }

    /// Which of the three sets of requests this one is from: the standard ones every
    /// device answers, a class's, or a vendor's.
    pub fn kind(self) -> u8 {
        self.request_type & TYPE_MASK
    }

    /// The high byte of `value`, which is the descriptor type of a request for one.
    pub fn descriptor(self) -> (u8, u8) {
        ((self.value >> 8) as u8, self.value as u8)
    }
}

/// The bits of a setup packet's first byte: which way the data goes, which set of
/// requests it is from, and what it is addressed to.
pub const DIRECTION_IN: u8 = 0x80;
pub const TYPE_MASK: u8 = 0x60;
pub const TYPE_STANDARD: u8 = 0x00;
pub const TYPE_CLASS: u8 = 0x20;
pub const RECIPIENT_MASK: u8 = 0x1f;
pub const RECIPIENT_DEVICE: u8 = 0;
pub const RECIPIENT_INTERFACE: u8 = 1;
pub const RECIPIENT_ENDPOINT: u8 = 2;

/// The standard requests. Universal Serial Bus Specification 2.0, table 9-4.
pub const GET_STATUS: u8 = 0;
pub const CLEAR_FEATURE: u8 = 1;
pub const SET_FEATURE: u8 = 3;
pub const SET_ADDRESS: u8 = 5;
pub const GET_DESCRIPTOR: u8 = 6;
pub const SET_DESCRIPTOR: u8 = 7;
pub const GET_CONFIGURATION: u8 = 8;
pub const SET_CONFIGURATION: u8 = 9;
pub const GET_INTERFACE: u8 = 10;
pub const SET_INTERFACE: u8 = 11;

/// The descriptor types. Universal Serial Bus Specification 2.0, table 9-5.
pub const DT_DEVICE: u8 = 1;
pub const DT_CONFIGURATION: u8 = 2;
pub const DT_STRING: u8 = 3;
pub const DT_INTERFACE: u8 = 4;
pub const DT_ENDPOINT: u8 = 5;
pub const DT_DEVICE_QUALIFIER: u8 = 6;

/// A device that refuses the transfer, which is the one answer that is not bytes.
///
/// Everything a device cannot do is this: a request it does not implement, a descriptor
/// it does not have, an endpoint that is not there. The controller turns it into a
/// stall error on the event ring and the driver into `-EPIPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall;

/// What a device answers a control transfer with: the bytes going back to the host, or
/// a refusal. A transfer with no data stage answers with none.
pub type Answer = Result<Vec<u8>, Stall>;

/// A device on the far end of a port.
///
/// The controller holds one of these per port and never learns anything else about it:
/// which address it was given, which configuration it is in and what it has to report
/// are all the device's own.
pub trait Device: fmt::Debug + Send {
    /// How fast it runs.
    fn speed(&self) -> Speed;

    /// A control transfer on endpoint zero: the setup packet, and the bytes that went
    /// out with it, which is nothing for a transfer coming in.
    fn control(&mut self, setup: Setup, out: &[u8]) -> Answer;

    /// Whatever `endpoint` has to send, if it has anything yet.
    ///
    /// Nothing is not an error: an interrupt IN endpoint with no report to give is the
    /// ordinary case, and the transfer asking for one waits until there is. That wait
    /// is what makes a key arriving at a keyboard something the guest learns about.
    fn read(&mut self, endpoint: u8) -> Option<Vec<u8>>;
}

/// The descriptors a device answers `GET_DESCRIPTOR` with, built once because none of
/// them ever changes.
///
/// The configuration is one blob rather than a list, because that is what a device
/// returns: a configuration descriptor with the interface, class and endpoint
/// descriptors that belong to it packed behind it, and a total length at the front
/// saying how far that goes. Universal Serial Bus Specification 2.0, 9.4.3.
#[derive(Debug, Clone, Default)]
pub struct Descriptors {
    pub device: Vec<u8>,
    pub configuration: Vec<u8>,
    /// One per index, with index zero the list of languages rather than a string.
    pub strings: Vec<Vec<u8>>,
}

impl Descriptors {
    /// The descriptor of `kind` at `index`, if there is one.
    pub fn get(&self, kind: u8, index: u8) -> Option<&[u8]> {
        match kind {
            DT_DEVICE => Some(&self.device),
            DT_CONFIGURATION if index == 0 => Some(&self.configuration),
            DT_STRING => self.strings.get(index as usize).map(Vec::as_slice),
            _ => None,
        }
    }
}

/// A string descriptor: a length, a type, and the characters as sixteen-bit units.
/// Universal Serial Bus Specification 2.0, 9.6.7.
pub fn string(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut bytes = vec![(2 + units.len() * 2) as u8, DT_STRING];
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The descriptor at index zero, which is the languages the rest are in rather than a
/// string. Universal Serial Bus Specification 2.0, 9.6.7.
pub fn languages(codes: &[u16]) -> Vec<u8> {
    let mut bytes = vec![(2 + codes.len() * 2) as u8, DT_STRING];
    for code in codes {
        bytes.extend_from_slice(&code.to_le_bytes());
    }
    bytes
}

/// American English, which is the only language anything here speaks.
pub const ENGLISH: u16 = 0x0409;

/// What a device is besides its descriptors: which configuration it has been put in,
/// and which alternate setting of its one interface.
///
/// Every device on this machine has one configuration and one interface, so this is
/// what a standard request writes and reads back, and nothing follows from it. A device
/// with more would have to act on the write.
#[derive(Debug, Clone, Copy, Default)]
pub struct Configured {
    pub configuration: u8,
    pub interface: u8,
}

/// Answer the standard requests, which are the same for every device here.
///
/// A request this does not answer is one the device itself has to, which for the
/// devices on this machine means a class request. `None` says so rather than refusing,
/// since the caller is the only thing that knows whether there is anything else to try.
pub fn standard(descriptors: &Descriptors, state: &mut Configured, setup: Setup) -> Option<Answer> {
    if setup.kind() != TYPE_STANDARD {
        return None;
    }
    Some(match (setup.request_type & RECIPIENT_MASK, setup.request) {
        (RECIPIENT_DEVICE, GET_DESCRIPTOR) => {
            let (kind, index) = setup.descriptor();
            match descriptors.get(kind, index) {
                // A request for more than there is answers with what there is, which
                // is how a driver reads a nine-byte configuration descriptor to find
                // out how long the whole of it is and then asks for that much.
                Some(bytes) => Ok(bytes[..bytes.len().min(setup.length as usize)].to_vec()),
                // A device that has no descriptor of that kind refuses, which is what
                // tells a driver asking for a device qualifier that this is not a
                // device with two speeds.
                None => Err(Stall),
            }
        }
        // The address is the controller's to assign: it is what routes a packet to one
        // device rather than another, and there are no packets here. A device that
        // received one would still have to answer, so this does.
        (RECIPIENT_DEVICE, SET_ADDRESS) => Ok(Vec::new()),
        (RECIPIENT_DEVICE, SET_CONFIGURATION) => {
            state.configuration = setup.value as u8;
            Ok(Vec::new())
        }
        (RECIPIENT_DEVICE, GET_CONFIGURATION) => Ok(vec![state.configuration]),
        (RECIPIENT_INTERFACE, SET_INTERFACE) => {
            state.interface = setup.value as u8;
            Ok(Vec::new())
        }
        (RECIPIENT_INTERFACE, GET_INTERFACE) => Ok(vec![state.interface]),
        // Bus powered, no remote wakeup, and no endpoint here is ever halted.
        (RECIPIENT_DEVICE, GET_STATUS) => Ok(vec![0, 0]),
        (RECIPIENT_INTERFACE | RECIPIENT_ENDPOINT, GET_STATUS) => Ok(vec![0, 0]),
        // Nothing here has a feature to set, and clearing an endpoint halt that never
        // happened is what a driver does after an error it did not have.
        (_, CLEAR_FEATURE | SET_FEATURE) => Ok(Vec::new()),
        // Descriptors are the device's, not software's.
        (_, SET_DESCRIPTOR) => Err(Stall),
        _ => Err(Stall),
    })
}
