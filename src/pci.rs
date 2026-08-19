//! A PCI Express root complex: config space, the windows a base address register
//! hands out addresses from, and the four wires a function pulls to interrupt.
//!
//! Everything below this point on the bus is enumerated rather than placed. Where the
//! rest of the machine says an address decodes to a device because a table says so,
//! here the guest reads what is there, asks each register how much space it wants,
//! and then says where it goes. The root complex is what answers those questions and
//! what routes an access once they have been answered.
//!
//! The layout is the one the QEMU `virt` machine publishes, since that is the machine
//! a RISC-V kernel already has a device tree binding for: `pci-host-ecam-generic`.

use std::sync::{Arc, Mutex};

use crate::{
    device::{Device, Line, Msi},
    trap::Exception,
};

/// Enhanced configuration access: a 4 KiB page of config space per function, indexed
/// by bus, device and function, so the whole of a 256-bus domain is 256 MiB.
pub const ECAM: u64 = 0x3000_0000;
pub const ECAM_SIZE: u64 = 0x1000_0000;

/// The window 32-bit base address registers are handed addresses from. It ends where
/// dram begins, which is what limits it to a gigabyte.
pub const MMIO: u64 = 0x4000_0000;
pub const MMIO_SIZE: u64 = 0x4000_0000;

/// And the one 64-bit registers are handed addresses from, which is a fixed sixteen
/// gigabytes in rather than anything that follows the top of dram.
pub const MMIO64: u64 = 0x4_0000_0000;
pub const MMIO64_SIZE: u64 = 0x4_0000_0000;

/// The port window. RISC-V has no port instructions, so this is here to be mapped and
/// not used: a function with an I/O register would answer here, and none does.
pub const PIO: u64 = 0x0300_0000;
pub const PIO_SIZE: u64 = 0x1_0000;

/// How many devices a bus holds, which is what five bits of the address select.
pub const DEVICES: usize = 32;
/// How many wires there are for functions to interrupt on, which is what limits the
/// swizzle below to four.
pub const PINS: usize = 4;

/// The register offsets of a type 0 header. Everything a function does not implement
/// reads as zero.
/// PCI Local Bus Specification 3.0, 6.1.
const VENDOR: u64 = 0x00;
const COMMAND: u64 = 0x04;
const CLASS: u64 = 0x08;
const HEADER_TYPE: u64 = 0x0c;
const BAR0: u64 = 0x10;
const SUBSYSTEM: u64 = 0x2c;
const CAPABILITIES: u64 = 0x34;
const INTERRUPT: u64 = 0x3c;
/// One past the last register a type 0 header defines, which is where the capability
/// list begins because there is nowhere else for it to go.
const HEADER_END: u64 = 0x40;

/// The capability every function here that interrupts by message has, and the three
/// registers it is made of: what it is and how big, and where in the function's own
/// windows the vector table and the pending array were put.
/// PCI Local Bus Specification 3.0, 6.8.2.
const MSIX: u64 = HEADER_END;
const MSIX_ID: u32 = 0x11;
const MSIX_CONTROL: u64 = MSIX;
const MSIX_TABLE: u64 = MSIX + 4;
const MSIX_PBA: u64 = MSIX + 8;
const MSIX_END: u64 = MSIX + 12;
/// Message control: how many vectors there are, one less than the number, and the two
/// bits software turns the whole thing on and off with.
const MSIX_SIZE: u32 = 0x7ff;
const MSIX_MASK: u32 = 1 << 30;
const MSIX_ENABLE: u32 = 1 << 31;
/// The bits of it a write may change.
const MSIX_WRITABLE: u32 = MSIX_ENABLE | MSIX_MASK;
/// A vector table entry: where to write, what to write, and whether to write it.
/// PCI Local Bus Specification 3.0, 6.8.2.9.
const VECTOR: u64 = 16;
const VECTOR_ADDRESS: usize = 0;
const VECTOR_ADDRESS_HIGH: usize = 1;
const VECTOR_DATA: usize = 2;
const VECTOR_CONTROL: usize = 3;
const VECTOR_MASKED: u32 = 1;

/// How many bytes the array of vectors raised while masked takes. It is defined in
/// doublewords however few vectors there are.
/// PCI Local Bus Specification 3.0, 6.8.2.6.
const fn pending_bytes(vectors: usize) -> u64 {
    (vectors as u64).div_ceil(64) * 8
}

/// `command`'s memory space enable: a base address register answers for nothing until
/// software has said it may. PCI Local Bus Specification 3.0, 6.2.2.
const COMMAND_MEMORY: u16 = 1 << 1;
/// The bits of `command` that are ours to keep. The rest are for capabilities and bus
/// behaviours this root complex does not have.
const COMMAND_MASK: u16 = COMMAND_MEMORY | (1 << 0) | (1 << 2) | (1 << 10);

/// `status`, which is read-only here and says only whether there is a capability list
/// to follow. PCI Local Bus Specification 3.0, 6.2.3.
const STATUS_CAPABILITIES: u32 = 1 << 4;

/// What a function's base address register asks for.
/// PCI Local Bus Specification 3.0, 6.2.5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    /// The register does not exist: it reads as zero and a write to it is dropped,
    /// which is how software is told there is nothing here to place.
    None,
    /// A window of `size` bytes, which has to be a power of two: the size is reported
    /// by the bits the base cannot set, so anything else could not be expressed.
    Memory {
        size: u64,
        prefetchable: bool,
        /// Whether it takes two registers and can be placed above four gigabytes. The
        /// register after a wide one is its high half and is not a register of its own.
        wide: bool,
    },
    /// The high half of the wide register before it.
    High,
}

impl Bar {
    /// The low bits of the register that describe it rather than locate it.
    fn flags(self) -> u64 {
        match self {
            Self::Memory {
                prefetchable, wide, ..
            } => ((wide as u64) << 2) | ((prefetchable as u64) << 3),
            _ => 0,
        }
    }
}

/// What a function is, before software has decided where it goes.
#[derive(Debug, Clone)]
pub struct Header {
    pub vendor: u16,
    pub device: u16,
    /// Class, subclass and programming interface in the top three bytes, revision in
    /// the low one, which is the order the register holds them in.
    pub class: u32,
    pub subsystem_vendor: u16,
    pub subsystem: u16,
    pub bars: [Bar; 6],
    /// Which of the four wires this function pulls, counting from one, or zero for a
    /// function that never interrupts.
    pub pin: u8,
    /// The messages it can send instead, if it can send any.
    pub msix: Option<MsiX>,
}

/// What a function's message-signalled interrupts look like: how many there are, and
/// where in its own windows the table of them and the array of the ones it could not
/// send are. Both are storage the root complex keeps, since every function that has
/// them has the same ones; what a function decides is only which vector to raise.
/// PCI Local Bus Specification 3.0, 6.8.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiX {
    pub vectors: usize,
    /// The window each lives in, and where in it.
    pub table: (usize, u64),
    pub pending: (usize, u64),
}

impl Default for Header {
    fn default() -> Self {
        Self {
            vendor: 0,
            device: 0,
            class: 0,
            subsystem_vendor: 0,
            subsystem: 0,
            bars: [Bar::None; 6],
            pin: 0,
            msix: None,
        }
    }
}

/// A function on the bus: what it is, and what its windows answer.
///
/// It never learns where its windows were put. An access arrives as the register it
/// landed in and an offset from the base of that register's window, which is the same
/// bargain a `Device` gets from the bus and for the same reason: moving a function is
/// then software's business and not the model's.
pub trait Function: std::fmt::Debug + Send {
    fn header(&self) -> Header;

    fn load(&mut self, bar: usize, offset: u64, size: u64) -> Result<u64, Exception>;

    fn store(&mut self, bar: usize, offset: u64, size: u64, value: u64) -> Result<(), Exception>;
}

/// One device number's worth of bus: the function there, and what software has
/// written about it.
#[derive(Debug)]
struct Slot {
    function: Box<dyn Function>,
    header: Header,
    /// Where each register has been told to answer. A wide register keeps its whole
    /// address in the low half of the pair.
    bars: [u64; 6],
    command: u16,
    /// Which interrupt the function was told it is on. Nothing here reads it: it is
    /// storage software uses to remember what the device tree already said.
    interrupt_line: u8,
    /// What software wrote in the function's message control register: whether it may
    /// send messages at all, and whether every one of them is masked.
    msix_control: u32,
    /// One entry per vector, four words each, and a bit per vector for the ones that
    /// were raised while masked.
    vectors: Vec<[u32; 4]>,
    blocked: Vec<bool>,
}

impl Slot {
    /// Whether an access to `bar` at `offset` lands in the vector table or the array
    /// of blocked vectors rather than in the function, and which vector it names.
    /// Both are storage here, so a function never sees an access to either.
    fn msix(&self, bar: usize, offset: u64, size: u64) -> Option<Region> {
        let msix = self.header.msix?;
        let within = |(at, from): (usize, u64), bytes: u64| {
            (at == bar && offset >= from && offset + size / 8 <= from + bytes)
                .then(|| offset - from)
        };
        if let Some(at) = within(msix.table, msix.vectors as u64 * VECTOR) {
            return Some(Region::Table((at / VECTOR) as usize, at % VECTOR));
        }
        within(msix.pending, pending_bytes(msix.vectors)).map(Region::Pending)
    }

    /// The window register `bar` answers for, if it has one and it is turned on.
    fn window(&self, bar: usize) -> Option<(u64, u64)> {
        let Bar::Memory { size, .. } = self.header.bars[bar] else {
            return None;
        };
        (self.command & COMMAND_MEMORY != 0).then(|| (self.bars[bar], size))
    }
}

/// What an access inside a function's window is really reaching, when it is not
/// reaching the function.
#[derive(Debug, Clone, Copy)]
enum Region {
    /// A word of one vector's entry in the table.
    Table(usize, u64),
    /// A byte of the array saying which vectors were raised while masked.
    Pending(u64),
}

/// The register a capability reports a table or an array in, which is the window's
/// number in the low three bits and the offset into it above them.
/// PCI Local Bus Specification 3.0, 6.8.2.
fn place((bar, offset): (usize, u64)) -> u32 {
    offset as u32 | bar as u32
}

/// The root complex, and everything below it.
#[derive(Debug)]
pub struct Complex {
    slots: Vec<Option<Slot>>,
    /// The four wires, which every function's pin is swizzled onto.
    lines: [Line; PINS],
    /// Where a message a function sends is posted.
    msi: Msi,
}

impl Complex {
    fn new(lines: [Line; PINS], msi: Msi) -> Self {
        Self {
            slots: (0..DEVICES).map(|_| None).collect(),
            lines,
            msi,
        }
    }

    /// Raise vector `vector` of the function at `device`.
    ///
    /// A message goes out if the function has been turned on and neither the function
    /// nor the vector is masked. One that cannot go out is remembered rather than
    /// dropped, and goes as soon as whatever masked it stops.
    /// PCI Local Bus Specification 3.0, 6.8.2.9.
    fn message(&mut self, device: usize, vector: usize) {
        let Some(slot) = self.slots.get_mut(device).and_then(Option::as_mut) else {
            return;
        };
        if slot.msix_control & MSIX_ENABLE == 0 || vector >= slot.vectors.len() {
            return;
        }
        let entry = slot.vectors[vector];
        if slot.msix_control & MSIX_MASK != 0 || entry[VECTOR_CONTROL] & VECTOR_MASKED != 0 {
            slot.blocked[vector] = true;
            return;
        }
        slot.blocked[vector] = false;
        let address = ((entry[VECTOR_ADDRESS_HIGH] as u64) << 32) | entry[VECTOR_ADDRESS] as u64;
        self.msi.send(address, entry[VECTOR_DATA]);
    }

    /// Send whatever a function was holding because it was masked, which is what
    /// unmasking either the function or one of its vectors has to do.
    fn release(&mut self, device: usize) {
        let Some(slot) = self.slots.get(device).and_then(Option::as_ref) else {
            return;
        };
        let blocked: Vec<usize> = (0..slot.blocked.len())
            .filter(|&vector| slot.blocked[vector])
            .collect();
        for vector in blocked {
            self.message(device, vector);
        }
    }

    /// Put `function` at `device` on bus zero.
    fn plug(&mut self, device: usize, function: Box<dyn Function>) {
        assert!(device < DEVICES, "device {device} is not on the bus");
        assert!(
            self.slots[device].is_none(),
            "device {device} is already taken"
        );
        let header = function.header();
        assert!(
            header.bars.iter().enumerate().all(|(n, bar)| match bar {
                // A size is reported by the bits the base cannot set, so anything but
                // a power of two could not be expressed, and anything under sixteen
                // bytes would run into the bits that say what kind of window it is.
                Bar::Memory { size, wide, .. } =>
                    size.is_power_of_two() && *size >= 16 && (!*wide || n < 5),
                // The high half of a wide register has to follow one.
                Bar::High => matches!(
                    n.checked_sub(1).map(|before| header.bars[before]),
                    Some(Bar::Memory { wide: true, .. })
                ),
                Bar::None => true,
            }),
            "device {device} asks for a window it cannot describe"
        );
        assert!(
            header.msix.is_none_or(|msix| {
                // A table and an array have to be inside windows that exist, and there
                // has to be at least one vector for the size field to describe.
                let fits = |(bar, offset): (usize, u64), bytes: u64| match header.bars.get(bar) {
                    Some(Bar::Memory { size, .. }) => offset + bytes <= *size,
                    _ => false,
                };
                (1..=MSIX_SIZE as usize + 1).contains(&msix.vectors)
                    // The low three bits of each register hold the window's number,
                    // so neither offset can reach into them.
                    && msix.table.1.is_multiple_of(8)
                    && msix.pending.1.is_multiple_of(8)
                    && fits(msix.table, msix.vectors as u64 * VECTOR)
                    && fits(msix.pending, pending_bytes(msix.vectors))
            }),
            "device {device} puts its vector table somewhere it does not have"
        );
        let vectors = header.msix.map_or(0, |msix| msix.vectors);
        self.slots[device] = Some(Slot {
            function,
            header,
            bars: [0; 6],
            command: 0,
            interrupt_line: 0,
            msix_control: 0,
            // A vector starts masked, which is what stops a function interrupting
            // before software has said where to send it.
            // PCI Local Bus Specification 3.0, 6.8.2.9.
            vectors: vec![[0, 0, 0, VECTOR_MASKED]; vectors],
            blocked: vec![false; vectors],
        });
    }

    /// The register `reg` of the function at `bus`, `device`, `function` reads as.
    ///
    /// Everything that is not there reads as all ones, which is how enumeration knows
    /// when to stop: there is no bus signal for "nothing answered", so the absence of
    /// a device is a vendor id no device may have.
    fn read(&mut self, bus: u8, device: usize, function: u8, reg: u64) -> u32 {
        let Some(slot) = self.slot(bus, device, function) else {
            return u32::MAX;
        };
        let header = &slot.header;
        match reg {
            VENDOR => (header.vendor as u32) | ((header.device as u32) << 16),
            COMMAND => {
                let status = match header.msix {
                    Some(_) => STATUS_CAPABILITIES,
                    None => 0,
                };
                (slot.command as u32) | (status << 16)
            }
            CLASS => header.class,
            // A single-function device with a type 0 header, and no built-in self test.
            HEADER_TYPE => 0,
            SUBSYSTEM => (header.subsystem_vendor as u32) | ((header.subsystem as u32) << 16),
            // Where the capability list starts, or nothing. The status register says
            // the same thing, and the two have to agree or software follows a pointer
            // into nothing.
            CAPABILITIES => match header.msix {
                Some(_) => MSIX as u32,
                None => 0,
            },
            INTERRUPT => (slot.interrupt_line as u32) | ((header.pin as u32) << 8),
            _ if (BAR0..BAR0 + 24).contains(&reg) => {
                let n = ((reg - BAR0) / 4) as usize;
                match header.bars[n] {
                    Bar::None => 0,
                    Bar::High => (slot.bars[n - 1] >> 32) as u32,
                    bar @ Bar::Memory { .. } => (slot.bars[n] | bar.flags()) as u32,
                }
            }
            _ if reg < HEADER_END => 0,
            // Beyond the header is the capability list, which is one capability long
            // and only exists for a function that has messages to send.
            MSIX..MSIX_END => match header.msix {
                Some(msix) => match reg {
                    // The identity, no next capability, and how many vectors there are,
                    // reported one less than there are.
                    MSIX_CONTROL => {
                        MSIX_ID
                            | ((msix.vectors as u32 - 1) << 16)
                            | (slot.msix_control & MSIX_WRITABLE)
                    }
                    MSIX_TABLE => place(msix.table),
                    MSIX_PBA => place(msix.pending),
                    _ => 0,
                },
                None => 0,
            },
            _ => 0,
        }
    }

    /// Write the bits of `reg` that `mask` selects, which is how a byte or halfword
    /// access reaches a register that is defined as a word.
    fn write(&mut self, bus: u8, device: usize, function: u8, reg: u64, value: u32, mask: u32) {
        let Some(slot) = self.slot_mut(bus, device, function) else {
            return;
        };
        let merge = |old: u32| (old & !mask) | (value & mask);
        match reg {
            COMMAND => slot.command = merge(slot.command as u32) as u16 & COMMAND_MASK,
            // Only the two bits that turn messages on and off are software's; the rest
            // of the capability describes what the function is.
            MSIX_CONTROL if slot.header.msix.is_some() => {
                slot.msix_control = merge(slot.msix_control) & MSIX_WRITABLE;
                self.release(device);
            }
            INTERRUPT => slot.interrupt_line = merge(slot.interrupt_line as u32) as u8,
            _ if (BAR0..BAR0 + 24).contains(&reg) => {
                let n = ((reg - BAR0) / 4) as usize;
                match slot.header.bars[n] {
                    // Only the bits above the size are the base's to set, so writing
                    // all ones and reading back reports the size in the bits that
                    // stayed zero. That is the whole of the sizing protocol; there is
                    // no mode to enter.
                    // PCI Local Bus Specification 3.0, 6.2.5.1.
                    Bar::Memory { size, wide, .. } => {
                        let low = merge(slot.bars[n] as u32) as u64;
                        let high = slot.bars[n] & !0xffff_ffff;
                        slot.bars[n] =
                            (high | low) & !(size - 1) & if wide { !0 } else { 0xffff_ffff };
                    }
                    Bar::High => {
                        let high = merge((slot.bars[n - 1] >> 32) as u32) as u64;
                        let low = slot.bars[n - 1] & 0xffff_ffff;
                        let Bar::Memory { size, .. } = slot.header.bars[n - 1] else {
                            return;
                        };
                        slot.bars[n - 1] = ((high << 32) | low) & !(size - 1);
                    }
                    Bar::None => {}
                }
            }
            _ => {}
        }
    }

    /// The slot an address names, if a function lives there. Only bus zero exists,
    /// since nothing here is a bridge, and only function zero of each device, since
    /// nothing here is multi-function.
    fn slot(&self, bus: u8, device: usize, function: u8) -> Option<&Slot> {
        (bus == 0 && function == 0)
            .then(|| self.slots.get(device))
            .flatten()?
            .as_ref()
    }

    fn slot_mut(&mut self, bus: u8, device: usize, function: u8) -> Option<&mut Slot> {
        (bus == 0 && function == 0)
            .then(|| self.slots.get_mut(device))
            .flatten()?
            .as_mut()
    }

    /// The function and register an address in one of the windows lands in.
    ///
    /// Found by address rather than by which window asked, since a base address
    /// register holds an address and it is software that decided which window that
    /// address is in.
    fn route(&self, addr: u64, size: u64) -> Option<(usize, usize, u64)> {
        let end = addr.checked_add(size / 8)?;
        self.slots.iter().enumerate().find_map(|(device, slot)| {
            let slot = slot.as_ref()?;
            (0..6).find_map(|bar| {
                let (base, len) = slot.window(bar)?;
                let offset = addr.checked_sub(base)?;
                (end <= base.checked_add(len)?).then_some((device, bar, offset))
            })
        })
    }
}

/// Which wire a function's pin ends up on, and so which interrupt the controller sees.
///
/// Every device would otherwise put its only pin on the same wire and four devices
/// would share one interrupt, so the mapping is rotated by the device number. This is
/// the rotation the `virt` machine's `interrupt-map` publishes, and the device tree
/// and this function have to agree or an interrupt arrives as the wrong one.
pub fn swizzle(device: usize, pin: u8) -> usize {
    (device + pin as usize - 1) % PINS
}

/// The root complex, shared between the ranges it answers for.
///
/// Config space and the windows are one model seen at several addresses: a write to a
/// base address register in the first decides what the others answer for, so they
/// cannot be separate devices holding separate state.
#[derive(Debug, Clone)]
pub struct Root(Arc<Mutex<Complex>>);

impl Root {
    /// A root complex driving `lines`, which are the four wires its functions
    /// interrupt on, and posting through `msi`, which is where the ones that send
    /// messages instead send them.
    pub fn new(lines: [Line; PINS], msi: Msi) -> Self {
        Self(Arc::new(Mutex::new(Complex::new(lines, msi))))
    }

    /// Put `function` at `device` on bus zero.
    pub fn plug(&self, device: usize, function: Box<dyn Function>) {
        self.0.lock().unwrap().plug(device, function);
    }

    /// Raise or lower the wire `device`'s pin is swizzled onto.
    pub fn interrupt(&self, device: usize, pin: u8, raised: bool) {
        let complex = self.0.lock().unwrap();
        complex.lines[swizzle(device, pin)].set(raised);
    }

    /// Raise vector `vector` of the function at `device`, which is what a function
    /// that sends messages does instead of pulling a wire.
    pub fn message(&self, device: usize, vector: usize) {
        self.0.lock().unwrap().message(device, vector);
    }

    /// Config space, as a device on the bus above.
    pub fn config(&self) -> Ecam {
        Ecam(self.clone())
    }

    /// One of the windows its base address registers hand out addresses from.
    pub fn window(&self, base: u64) -> Window {
        Window {
            root: self.clone(),
            base,
        }
    }
}

/// Config space at its own address, which is the only way to reach a function that has
/// not been given a window yet.
#[derive(Debug)]
pub struct Ecam(Root);

impl Ecam {
    /// Which function, and which of its registers, an offset into config space names.
    /// PCI Express Base Specification, 7.2.2.
    fn decode(offset: u64) -> (u8, usize, u8, u64) {
        let bus = (offset >> 20) as u8;
        let device = ((offset >> 15) & 0x1f) as usize;
        let function = ((offset >> 12) & 0x7) as u8;
        (bus, device, function, offset & 0xfff)
    }
}

impl Device for Ecam {
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        if !matches!(size, 8 | 16 | 32) {
            return Err(Exception::LoadAccessFault(offset));
        }
        let (bus, device, function, reg) = Self::decode(offset);
        let word = self
            .0
            .0
            .lock()
            .unwrap()
            .read(bus, device, function, reg & !3);
        // A register is a word, and a narrower access reads the part of it that its
        // address selects.
        let shift = (reg & 3) * 8;
        Ok(((word as u64) >> shift) & (u64::MAX >> (64 - size)))
    }

    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if !matches!(size, 8 | 16 | 32) {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        let (bus, device, function, reg) = Self::decode(offset);
        let shift = (reg & 3) * 8;
        let mask = ((u64::MAX >> (64 - size)) << shift) as u32;
        self.0.0.lock().unwrap().write(
            bus,
            device,
            function,
            reg & !3,
            (value << shift) as u32,
            mask,
        );
        Ok(())
    }
}

/// One of the windows, which answers for whichever function's base address register
/// has been pointed at the address.
#[derive(Debug)]
pub struct Window {
    root: Root,
    /// The address this window starts at, since a device is told an offset and a base
    /// address register holds an address.
    base: u64,
}

impl Device for Window {
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        let addr = self.base + offset;
        let mut complex = self.root.0.lock().unwrap();
        let Some((device, bar, at)) = complex.route(addr, size) else {
            // Nothing is mapped here. A read of all ones is what a bus with no
            // responder gives, and it is what a driver reads when it looks at a
            // window it has not been given.
            return Ok(u64::MAX >> (64 - size));
        };
        let slot = complex.slots[device].as_mut().expect("routed to a slot");
        match slot.msix(bar, at, size) {
            Some(Region::Table(vector, word)) => Ok(read(&slot.vectors[vector], word, size)),
            Some(Region::Pending(byte)) => Ok(blocked(&slot.blocked, byte, size)),
            None => slot.function.load(bar, at, size),
        }
    }

    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        let addr = self.base + offset;
        let mut complex = self.root.0.lock().unwrap();
        let Some((device, bar, at)) = complex.route(addr, size) else {
            return Ok(());
        };
        let slot = complex.slots[device].as_mut().expect("routed to a slot");
        match slot.msix(bar, at, size) {
            Some(Region::Table(vector, word)) => {
                write(&mut slot.vectors[vector], word, size, value);
                // Unmasking is what lets a message raised while masked finally go.
                complex.release(device);
                Ok(())
            }
            // Which vectors are waiting is the root complex's to say, not software's.
            Some(Region::Pending(_)) => Ok(()),
            None => slot.function.store(bar, at, size, value),
        }
    }
}

/// A word or part of one out of a vector's four, since a table is defined in words and
/// software may read it more narrowly.
fn read(entry: &[u32; 4], offset: u64, size: u64) -> u64 {
    let word = entry[(offset / 4) as usize] as u64;
    (word >> ((offset % 4) * 8)) & (u64::MAX >> (64 - size))
}

fn write(entry: &mut [u32; 4], offset: u64, size: u64, value: u64) {
    let shift = (offset % 4) * 8;
    let mask = ((u64::MAX >> (64 - size)) << shift) as u32;
    let word = &mut entry[(offset / 4) as usize];
    *word = (*word & !mask) | ((value << shift) as u32 & mask);
}

/// The array of vectors raised while masked, as the bits an access at `byte` covers.
fn blocked(vectors: &[bool], byte: u64, size: u64) -> u64 {
    (0..size)
        .filter(|bit| {
            let vector = (byte * 8 + bit) as usize;
            vectors.get(vector).copied().unwrap_or(false)
        })
        .fold(0, |bits, bit| bits | 1 << bit)
}

/// The port window, which exists to be mapped. Nothing on this machine has a port
/// register, and RISC-V has no instruction that would reach one if it did.
#[derive(Debug, Default)]
pub struct Ports;

impl Device for Ports {
    fn load(&mut self, _offset: u64, size: u64) -> Result<u64, Exception> {
        Ok(u64::MAX >> (64 - size))
    }

    fn store(&mut self, _offset: u64, _size: u64, _value: u64) -> Result<(), Exception> {
        Ok(())
    }
}

/// The host bridge itself, which is device zero of every PCI bus: the thing the rest
/// of the bus hangs off, reported so that software enumerating the bus finds something
/// where it looks first.
#[derive(Debug, Default)]
pub struct HostBridge;

impl Function for HostBridge {
    fn header(&self) -> Header {
        Header {
            // The identity QEMU's generic PCIe host bridge reports, which is what a
            // kernel built for the `virt` machine has already seen.
            vendor: 0x1b36,
            device: 0x0008,
            // Bridge, host bridge, no programming interface, revision zero.
            class: 0x0600_0000,
            ..Header::default()
        }
    }

    fn load(&mut self, _bar: usize, offset: u64, _size: u64) -> Result<u64, Exception> {
        Err(Exception::LoadAccessFault(offset))
    }

    fn store(
        &mut self,
        _bar: usize,
        offset: u64,
        _size: u64,
        _value: u64,
    ) -> Result<(), Exception> {
        Err(Exception::StoreAmoAccessFault(offset))
    }
}
