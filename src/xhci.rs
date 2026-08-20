//! An xHCI host controller: the four register regions, the rings software hands it work
//! on, and the ring it hands answers back on.
//!
//! Almost none of this controller's state is its own. The slots, the endpoints, where
//! each endpoint's ring has got to and what state it is in all live in guest memory, in
//! the contexts the driver wrote and the controller is required to write back, so the
//! model reads them there rather than keeping a second copy that could disagree. What it
//! does own is which slot numbers are taken, where its own event ring has got to, and
//! which endpoints are waiting for a device to have something to say.
//!
//! A transfer finishes inside the doorbell write that started it. There is no bus below
//! this, no frame to schedule against and no packet to send, so a controller that answers
//! immediately is not distinguishable from an extremely fast one -- except for an
//! interrupt IN endpoint with nothing to report, which is the whole point of a keyboard:
//! that transfer stays outstanding and finishes later, out of `poll`, when a key has been
//! pressed.
//!
//! eXtensible Host Controller Interface for Universal Serial Bus 1.2 defines the
//! registers, the contexts and the transfer request blocks. It is not redistributable, so
//! where a field needs explaining the explanation is here; `drivers/usb/host/xhci.h` is
//! the same layout as the driver reads it.

use std::collections::BTreeSet;

use crate::{
    device::{Dma, Field, Value, field},
    pci::{Asserted, Bar, Function, Header, MsiX},
    trap::Exception,
    usb::{self, Setup},
};

/// The identity this reports. Nothing binds on it: `xhci-pci` matches the class, so
/// this is only what `lspci` prints, and it is the one QEMU's own controller reports
/// so that the two machines name the same device.
const VENDOR: u16 = 0x1b36;
const DEVICE: u16 = 0x000d;
/// Serial bus controller, USB, xHCI programming interface, revision one.
const CLASS: u32 = 0x0c03_3001;

/// The one window, and what is where in it.
///
/// Four regions, each found through a register in the first: the capability registers say
/// how long they are and where the other three begin. Nothing outside them answers.
pub const WINDOW: u64 = 0x2000;
const CAPABILITY: u64 = 0x20;
const OPERATIONAL: u64 = CAPABILITY;
const PORTSC: u64 = OPERATIONAL + 0x400;
const EXTENDED: u64 = 0x500;
const RUNTIME: u64 = 0x600;
const DOORBELLS: u64 = 0x800;
/// Where the table of messages and the array of the ones that could not be sent go,
/// which is above everything the controller itself answers for.
const MSIX_TABLE: u64 = 0x1000;
const MSIX_PENDING: u64 = 0x1800;

/// How many devices may be addressed at once, how many interrupters there are, and how
/// many ports.
///
/// One interrupter, because nothing here has a reason for two: an interrupter is a queue
/// of events and a vector to raise, and spreading a keyboard and a mouse over several of
/// them would only make the model larger. A driver asks for as many as this says.
const SLOTS: usize = 32;
const INTERRUPTERS: usize = 1;
/// Two ports carrying the devices and two that carry nothing, which is the shape every
/// real controller has: a root hub for each speed the bus has had.
const USB2_PORTS: usize = 2;
const USB3_PORTS: usize = 2;
const PORTS: usize = USB2_PORTS + USB3_PORTS;

/// How many segments an event ring may have, as the power of two the register holds.
const ERST_MAX: u32 = 4;

/// Version 1.2 of the interface, which is what the capability register reports.
const VERSION: u32 = 0x0120;

// The operational registers, from `OPERATIONAL`.
const USBCMD: u64 = 0x00;
const USBSTS: u64 = 0x04;
const PAGESIZE: u64 = 0x08;
const DNCTRL: u64 = 0x14;
const CRCR: u64 = 0x18;
const DCBAAP: u64 = 0x30;
const CONFIG: u64 = 0x38;

/// Bits of `USBCMD`: run, reset, and the two that let an interrupt out.
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
/// The bits of it that are storage. Save and restore state are not offered, and the
/// wrap event needs a frame counter this controller does not run.
const CMD_MASK: u32 = CMD_RUN | CMD_INTE | (1 << 3) | (1 << 7) | (1 << 10) | (1 << 11);

/// Bits of `USBSTS`: halted, an event is waiting, a port changed, and an error nothing
/// recovers from.
const STS_HALTED: u32 = 1 << 0;
const STS_EINT: u32 = 1 << 3;
const STS_PORT_CHANGE: u32 = 1 << 4;
const STS_HOST_ERROR: u32 = 1 << 12;
/// The bits of it a write of one clears.
const STS_CLEARABLE: u32 = (1 << 2) | STS_EINT | STS_PORT_CHANGE | (1 << 10);

/// Bits of `CRCR`: which cycle the ring starts in, the two ways to stop it, and whether
/// it is running. Only the last is readable.
const CRCR_CYCLE: u64 = 1 << 0;
const CRCR_STOP: u64 = 1 << 1;
const CRCR_ABORT: u64 = 1 << 2;
const CRCR_RUNNING: u64 = 1 << 3;
const CRCR_POINTER: u64 = !0x3f;

/// The runtime registers: the frame counter, then one set per interrupter.
const MFINDEX: u64 = 0x00;
const INTERRUPTER: u64 = 0x20;
const INTERRUPTER_SIZE: u64 = 0x20;
const IMAN: u64 = 0x00;
const IMOD: u64 = 0x04;
const ERSTSZ: u64 = 0x08;
const ERSTBA: u64 = 0x10;
const ERDP: u64 = 0x18;

/// Bits of `IMAN`: an interrupt is waiting, and whether this interrupter may raise one.
const IMAN_PENDING: u32 = 1 << 0;
const IMAN_ENABLE: u32 = 1 << 1;
/// And of `ERDP`: the bit software writes to say it has finished with what it took. The
/// three below it are its own note of which segment it stopped in, kept as written and
/// read by nothing here.
const ERDP_BUSY: u64 = 1 << 3;
const ERDP_POINTER: u64 = !0xf;

/// One entry of the event ring segment table: where a segment is and how long.
const ERST_ENTRY: u64 = 16;

/// How long a transfer request block is, which every ring is made of.
const TRB: u64 = 16;

/// The transfer request block types this controller answers for.
/// xHCI 1.2, table 6-91.
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_EVENT_DATA: u32 = 7;
const TRB_NOOP: u32 = 8;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_RESET_ENDPOINT: u32 = 14;
const TRB_STOP_ENDPOINT: u32 = 15;
const TRB_SET_DEQUEUE: u32 = 16;
const TRB_RESET_DEVICE: u32 = 17;
const TRB_NOOP_COMMAND: u32 = 23;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_EVENT: u32 = 33;
const TRB_PORT_EVENT: u32 = 34;

/// Bits of a transfer request block's last word: whose turn it is, whether the next one
/// belongs to the same transfer, whether to say when it is done, whether to say when it
/// came up short, and whether the parameter is data rather than an address.
const CYCLE: u32 = 1 << 0;
const TOGGLE: u32 = 1 << 1;
const SHORT: u32 = 1 << 2;
const CHAIN: u32 = 1 << 4;
const INTERRUPT_ON_COMPLETION: u32 = 1 << 5;
const IMMEDIATE: u32 = 1 << 6;
/// The bit of an event saying its parameter is a word the driver put in a block rather
/// than the address of the block itself.
const EVENT_DATA: u32 = 1 << 2;
/// The two commands that mean something by bit nine.
const BLOCK_SET_ADDRESS: u32 = 1 << 9;
const DECONFIGURE: u32 = 1 << 9;

/// The completion codes an answer carries. xHCI 1.2, table 6-90.
const SUCCESS: u32 = 1;
const TRB_ERROR: u32 = 5;
const STALL: u32 = 6;
const NO_SLOTS: u32 = 9;
const SLOT_NOT_ENABLED: u32 = 11;
const ENDPOINT_NOT_ENABLED: u32 = 12;
const SHORT_PACKET: u32 = 13;
const PARAMETER_ERROR: u32 = 17;
const CONTEXT_STATE_ERROR: u32 = 19;

/// How long a context is. Thirty-two rather than sixty-four, which is what the capability
/// register says and what halves every offset below.
const CONTEXT: u64 = 32;

/// The slot states a device context reports. xHCI 1.2, figure 4-2.
const SLOT_ENABLED: u32 = 0;
const SLOT_DEFAULT: u32 = 1;
const SLOT_ADDRESSED: u32 = 2;
const SLOT_CONFIGURED: u32 = 3;

/// And the endpoint states.
const ENDPOINT_DISABLED: u32 = 0;
const ENDPOINT_RUNNING: u32 = 1;
const ENDPOINT_HALTED: u32 = 2;
const ENDPOINT_STOPPED: u32 = 3;

/// Bits of `PORTSC`: what is attached and what state it is in.
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_LINK: u32 = 0xf << 5;
const PORT_POWER: u32 = 1 << 9;
const PORT_LINK_STROBE: u32 = 1 << 16;
const PORT_WARM_RESET: u32 = 1 << 31;
/// The change bits, each set by the controller and cleared by writing a one.
const PORT_CONNECT_CHANGE: u32 = 1 << 17;
const PORT_ENABLE_CHANGE: u32 = 1 << 18;
const PORT_WARM_RESET_CHANGE: u32 = 1 << 19;
const PORT_OVERCURRENT_CHANGE: u32 = 1 << 20;
const PORT_RESET_CHANGE: u32 = 1 << 21;
const PORT_LINK_CHANGE: u32 = 1 << 22;
const PORT_CONFIG_ERROR_CHANGE: u32 = 1 << 23;
const PORT_CHANGES: u32 = PORT_CONNECT_CHANGE
    | PORT_ENABLE_CHANGE
    | PORT_WARM_RESET_CHANGE
    | PORT_OVERCURRENT_CHANGE
    | PORT_RESET_CHANGE
    | PORT_LINK_CHANGE
    | PORT_CONFIG_ERROR_CHANGE;
/// The wake enables, which are storage: nothing here is ever asleep to be woken.
const PORT_WAKE: u32 = 0x7 << 25;

/// The link states a port reports. xHCI 1.2, table 5-27.
const LINK_U0: u32 = 0;
const LINK_DISABLED: u32 = 4;
const LINK_RX_DETECT: u32 = 5;
const LINK_POLLING: u32 = 7;

/// A ring the controller reads: where it has got to, and whose turn the entry there is.
///
/// The cycle bit is the whole of how a producer and a consumer share a ring with no
/// count between them. Software writes entries with the bit set to the state it is in
/// and the controller reads while what it finds matches; a link back to the start
/// carries a bit that says to flip.
#[derive(Debug, Clone, Copy)]
struct Ring {
    at: u64,
    cycle: bool,
}

/// One interrupter: a queue of events, and one vector to raise about them.
#[derive(Debug, Default)]
struct Interrupter {
    iman: u32,
    imod: u32,
    erstsz: u32,
    erstba: u64,
    /// Where software has got to reading, which is what says the ring is not full.
    erdp: u64,
    /// Where the controller has got to writing: which segment of the table, how far
    /// into it, and which cycle an entry written there carries.
    segment: u64,
    offset: u64,
    cycle: bool,
}

/// One port of the root hub: what is plugged into it, and what software has said about
/// it.
///
/// The register is worked out from these when read rather than stored, for the same
/// reason the display's mode is: most of its bits are the controller's to say, and
/// keeping a word software could half-write would let it disagree with what is attached.
#[derive(Debug)]
struct Port {
    /// Which of the two root hubs the port belongs to, which decides only what the
    /// supported protocol capability says about it and what an empty one reports.
    usb3: bool,
    device: Option<Box<dyn usb::Device>>,
    /// Set by a reset, cleared by software writing a one over it.
    enabled: bool,
    /// Which of the change bits are set.
    changes: u32,
    link: u32,
    /// The three wake enables, which are storage: nothing here is ever asleep to be
    /// woken, and a driver that set one reads back what it set.
    wake: u32,
}

impl Port {
    fn new(usb3: bool, device: Option<Box<dyn usb::Device>>) -> Self {
        let attached = device.is_some();
        Self {
            usb3,
            device,
            enabled: false,
            // A port that has something plugged into it comes up saying so, which is
            // what a hub driver finds when it first looks rather than being told.
            changes: if attached { PORT_CONNECT_CHANGE } else { 0 },
            link: match (attached, usb3) {
                (true, _) => LINK_POLLING,
                (false, true) => LINK_RX_DETECT,
                (false, false) => LINK_DISABLED,
            },
            wake: 0,
        }
    }

    /// What the register reads as.
    fn read(&self) -> u32 {
        let speed = match (&self.device, self.enabled) {
            // The speed of what is attached is not known until a reset has found it
            // out, which is what the field reporting nothing before one means.
            (Some(device), true) => device.speed().code(),
            _ => 0,
        };
        // Port power is always on: this controller does not offer port power control,
        // so there is nothing for software to turn off.
        PORT_POWER
            | match self.device.is_some() {
                true => PORT_CONNECTED,
                false => 0,
            }
            | ((self.enabled as u32) << 1)
            | (self.link << 5)
            | (speed << 10)
            | self.wake
            | self.changes
    }

    /// Reset it, which is what finds out what is attached and enables the port.
    /// A port with nothing in it enables nothing and only reports that the reset
    /// finished. xHCI 1.2, 4.19.5.
    fn reset(&mut self, warm: bool) {
        self.enabled = self.device.is_some();
        self.link = match self.enabled {
            true => LINK_U0,
            false if self.usb3 => LINK_RX_DETECT,
            false => LINK_DISABLED,
        };
        self.changes |= match warm {
            true => PORT_WARM_RESET_CHANGE,
            false => PORT_RESET_CHANGE,
        };
    }
}

/// The controller.
#[derive(Debug)]
pub struct Xhci {
    /// Where the rings and the contexts are, which is guest memory and nothing else.
    dma: Dma,
    usbcmd: u32,
    usbsts: u32,
    dnctrl: u32,
    crcr: u64,
    dcbaap: u64,
    config: u32,
    ports: Vec<Port>,
    interrupters: [Interrupter; INTERRUPTERS],
    /// Where the command ring has got to, and whether it is running at all.
    command: Option<Ring>,
    /// Which slot numbers are taken. Slot zero is not a slot: the doorbell at that
    /// index is the command ring's.
    slots: [bool; SLOTS + 1],
    /// The endpoints that were rung and had nothing to carry yet, which is every
    /// interrupt IN endpoint most of the time. `poll` is what tries them again.
    waiting: BTreeSet<(usize, usize)>,
    /// What this function is asserting, which the root complex takes and delivers.
    asserted: Asserted,
}

impl Xhci {
    /// A controller with a keyboard and a mouse plugged into its two low-speed ports,
    /// and nothing in the two above them.
    pub fn new(dma: Dma, devices: Vec<Box<dyn usb::Device>>) -> Self {
        assert!(
            devices.len() <= USB2_PORTS,
            "a controller with {USB2_PORTS} ports for them"
        );
        let mut attached = devices.into_iter().map(Some);
        let ports = (0..PORTS)
            .map(|port| match port < USB2_PORTS {
                true => Port::new(false, attached.next().flatten()),
                false => Port::new(true, None),
            })
            .collect();
        let mut xhci = Self {
            dma,
            usbcmd: 0,
            usbsts: 0,
            dnctrl: 0,
            crcr: 0,
            dcbaap: 0,
            config: 0,
            ports,
            interrupters: Default::default(),
            command: None,
            slots: [false; SLOTS + 1],
            waiting: BTreeSet::new(),
            asserted: Asserted::default(),
        };
        xhci.reset();
        xhci
    }

    /// Put everything back the way it powers up, which is what a write of the reset bit
    /// asks for. What is plugged into a port is not software's to change, so the ports
    /// are reset rather than emptied.
    fn reset(&mut self) {
        self.usbcmd = 0;
        // Halted, since nothing is running until software says to run.
        self.usbsts = STS_HALTED;
        self.dnctrl = 0;
        self.crcr = 0;
        self.dcbaap = 0;
        self.config = 0;
        self.interrupters = Default::default();
        self.command = None;
        self.slots = [false; SLOTS + 1];
        self.waiting.clear();
        for port in &mut self.ports {
            let device = port.device.take();
            *port = Port::new(port.usb3, device);
        }
    }

    fn running(&self) -> bool {
        self.usbcmd & CMD_RUN != 0
    }

    // ------------------------------------------------------------ capability registers

    /// The first region, which is the only one at a fixed place: everything else is
    /// found through it.
    fn capability(&self, offset: u64) -> u32 {
        match offset {
            0x00 => CAPABILITY as u32 | (VERSION << 16),
            // How many slots, how many interrupters, how many ports.
            0x04 => SLOTS as u32 | ((INTERRUPTERS as u32) << 8) | ((PORTS as u32) << 24),
            // No scheduling threshold to meet, this many event ring segments, and no
            // scratchpad: a controller with nowhere of its own to put things needs
            // none of the guest's memory lent to it.
            0x08 => ERST_MAX << 4,
            // No link power states to come out of, so no latency to come out of them.
            0x0c => 0,
            // Sixty-four bit addressing, thirty-two byte contexts, no port power
            // control, no light reset, and where the extended capabilities are, in
            // units of four bytes from the base of the window.
            0x10 => 1 | ((EXTENDED as u32 / 4) << 16),
            0x14 => DOORBELLS as u32,
            0x18 => RUNTIME as u32,
            // None of the capabilities version 1.1 added.
            0x1c => 0,
            _ => 0,
        }
    }

    /// The extended capabilities, which is one list with one kind of entry on it: what
    /// each range of ports speaks.
    ///
    /// A controller says nothing else here. There is no legacy owner to take the
    /// controller from, since nothing ran before the kernel, and no power management or
    /// virtualisation to describe. xHCI 1.2, 7.2.
    fn extended(&self, offset: u64) -> u32 {
        /// The supported protocol capability, and how long one is.
        const PROTOCOL: u32 = 2;
        const LENGTH: u64 = 0x10;

        let (usb2, usb3) = (0, LENGTH);
        let entry = |at: u64, major: u32, first: usize, count: usize, next: u64| match offset - at {
            // What it is, where the next one is, and which revision of the bus these
            // ports speak.
            0x0 => PROTOCOL | ((next as u32) << 8) | (major << 24),
            // "USB ", which is what says the revision above is a USB one.
            0x4 => u32::from_le_bytes(*b"USB "),
            // The first port this covers, counting from one, and how many.
            0x8 => (first as u32 + 1) | ((count as u32) << 8),
            // No protocol speed identifiers of its own, so the speeds are the ones the
            // port register's field already names.
            _ => 0,
        };
        match offset {
            _ if offset < LENGTH => entry(usb2, 2, 0, USB2_PORTS, LENGTH / 4),
            _ if offset < 2 * LENGTH => entry(usb3, 3, USB2_PORTS, USB3_PORTS, 0),
            _ => 0,
        }
    }

    // ----------------------------------------------------------- operational registers

    fn operational(&self, offset: u64) -> u32 {
        match offset {
            USBCMD => self.usbcmd,
            USBSTS => self.usbsts,
            // Four kibibyte pages, which is the smallest a controller may ask for and
            // the only size anything here uses.
            PAGESIZE => 1,
            DNCTRL => self.dnctrl,
            // The pointer is not readable: what software may learn is whether the ring
            // is running. xHCI 1.2, 5.4.5.
            CRCR => match self.command.is_some() {
                true => CRCR_RUNNING as u32,
                false => 0,
            },
            CRCR_HIGH => 0,
            DCBAAP => self.dcbaap as u32,
            DCBAAP_HIGH => (self.dcbaap >> 32) as u32,
            CONFIG => self.config,
            _ => 0,
        }
    }

    fn write_operational(&mut self, offset: u64, value: u32) {
        match offset {
            USBCMD => {
                if value & CMD_RESET != 0 {
                    self.reset();
                    return;
                }
                let was = self.running();
                self.usbcmd = value & CMD_MASK;
                match (was, self.running()) {
                    (false, true) => self.usbsts &= !STS_HALTED,
                    (true, false) => {
                        self.usbsts |= STS_HALTED;
                        self.command = None;
                    }
                    _ => {}
                }
                self.signal();
            }
            // Every bit of the status that software may change it cleared by writing a
            // one over it, and every other bit is the controller's.
            USBSTS => {
                self.usbsts &= !(value & STS_CLEARABLE);
                self.signal();
            }
            DNCTRL => self.dnctrl = value & 0xffff,
            CRCR => {
                self.crcr = (self.crcr & !0xffff_ffff) | value as u64;
                self.take_command_ring(value as u64);
            }
            CRCR_HIGH => {
                self.crcr = (self.crcr & 0xffff_ffff) | ((value as u64) << 32);
                self.take_command_ring(self.crcr);
            }
            DCBAAP => self.dcbaap = (self.dcbaap & !0xffff_ffff) | (value as u64 & !0x3f),
            DCBAAP_HIGH => self.dcbaap = (self.dcbaap & 0xffff_ffff) | ((value as u64) << 32),
            CONFIG => self.config = value & 0xff,
            _ => {}
        }
    }

    /// What a write to the command ring control register does.
    ///
    /// Writing a pointer while the ring is stopped is what starts it, and the two stop
    /// bits are what stops it. Neither of the stops is asynchronous here, since a
    /// command finishes inside the write that rang for it, so there is never a command
    /// in flight to abort.
    fn take_command_ring(&mut self, low: u64) {
        if low & (CRCR_STOP | CRCR_ABORT) != 0 {
            self.command = None;
            return;
        }
        // The high half arrives second, so the pointer is only a pointer once both
        // halves have. A driver writes the low half first and the ring starts when the
        // whole address is there.
        let at = self.crcr & CRCR_POINTER;
        if at != 0 {
            self.command = Some(Ring {
                at,
                cycle: self.crcr & CRCR_CYCLE != 0,
            });
        }
    }

    // -------------------------------------------------------------------- port registers

    /// Which port a register in the port region belongs to, and which of its four.
    fn port(offset: u64) -> (usize, u64) {
        (((offset / 0x10) as usize), offset % 0x10)
    }

    fn read_port(&self, port: usize, register: u64) -> u32 {
        let Some(port) = self.ports.get(port) else {
            return 0;
        };
        match register {
            0x0 => port.read(),
            // Power management, link information and hardware link power management,
            // none of which describes anything this controller does.
            _ => 0,
        }
    }

    fn write_port(&mut self, port: usize, register: u64, value: u32) {
        if register != 0 || port >= self.ports.len() {
            return;
        }
        let changed = {
            let entry = &mut self.ports[port];
            let before = entry.read();
            // Writing a one over a change bit clears it, and writing a one over the
            // enable bit disables the port. Both are the only way either moves.
            entry.changes &= !(value & PORT_CHANGES);
            entry.wake = value & PORT_WAKE;
            if value & PORT_ENABLED != 0 {
                entry.enabled = false;
            }
            // A link state write only lands if the strobe says it is one.
            if value & PORT_LINK_STROBE != 0 {
                entry.link = (value & PORT_LINK) >> 5;
            }
            if value & (PORT_RESET | PORT_WARM_RESET) != 0 {
                entry.reset(value & PORT_WARM_RESET != 0);
            }
            entry.read() != before
        };
        if changed {
            self.port_changed(port);
        }
    }

    /// Say that a port's register moved, which is one bit of the status and one event.
    fn port_changed(&mut self, port: usize) {
        if self.ports[port].changes == 0 {
            return;
        }
        self.usbsts |= STS_PORT_CHANGE;
        // The port number, counting from one, is the whole of what the event carries:
        // software reads the register to find out what about it changed.
        self.post(Trb {
            parameter: (port as u64 + 1) << 24,
            status: SUCCESS << 24,
            control: TRB_PORT_EVENT << 10,
        });
    }

    // ---------------------------------------------------------------- runtime registers

    fn runtime(&self, offset: u64) -> u32 {
        if offset == MFINDEX {
            // The frame this controller is in, which is always the first: nothing here
            // is scheduled against a frame, since every transfer finishes in the write
            // that started it and no endpoint is isochronous.
            return 0;
        }
        let Some((interrupter, register)) = Self::interrupter(offset) else {
            return 0;
        };
        let Some(set) = self.interrupters.get(interrupter) else {
            return 0;
        };
        match register {
            IMAN => set.iman,
            IMOD => set.imod,
            ERSTSZ => set.erstsz,
            ERSTBA => set.erstba as u32,
            ERSTBA_HIGH => (set.erstba >> 32) as u32,
            ERDP => set.erdp as u32,
            ERDP_HIGH => (set.erdp >> 32) as u32,
            _ => 0,
        }
    }

    fn write_runtime(&mut self, offset: u64, value: u32) {
        let Some((interrupter, register)) = Self::interrupter(offset) else {
            return;
        };
        if interrupter >= self.interrupters.len() {
            return;
        }
        let set = &mut self.interrupters[interrupter];
        match register {
            // The enable is storage and the pending bit is cleared by writing a one
            // over it, which is how a handler says it has been round the ring.
            IMAN => {
                set.iman = (set.iman & !IMAN_ENABLE) | (value & IMAN_ENABLE);
                set.iman &= !(value & IMAN_PENDING);
            }
            // How long to wait before raising another, which this controller does not:
            // an event here costs what a function call costs, so there is nothing to
            // moderate and nothing that would notice if there were.
            IMOD => set.imod = value,
            ERSTSZ => {
                set.erstsz = value & 0xffff;
                // Resizing the table starts the ring again from its first segment,
                // which is what makes writing the table and then its size an order
                // that works. xHCI 1.2, 5.5.2.3.1.
                set.segment = 0;
                set.offset = 0;
                set.cycle = true;
            }
            ERSTBA => set.erstba = (set.erstba & !0xffff_ffff) | (value as u64 & !0x3f),
            ERSTBA_HIGH => {
                set.erstba = (set.erstba & 0xffff_ffff) | ((value as u64) << 32);
                set.segment = 0;
                set.offset = 0;
                set.cycle = true;
            }
            ERDP => {
                let busy = set.erdp & !(value as u64) & ERDP_BUSY;
                set.erdp = (set.erdp & !0xffff_ffff) | (value as u64) | busy;
                // The busy bit is cleared by writing a one over it, which says software
                // has finished with everything up to the pointer it wrote.
                set.erdp &= !(value as u64 & ERDP_BUSY);
            }
            ERDP_HIGH => set.erdp = (set.erdp & 0xffff_ffff) | ((value as u64) << 32),
            _ => {}
        }
        self.signal();
    }

    /// Which interrupter a runtime register belongs to, and which of its registers.
    fn interrupter(offset: u64) -> Option<(usize, u64)> {
        let at = offset.checked_sub(INTERRUPTER)?;
        Some((
            (at / INTERRUPTER_SIZE) as usize,
            (at % INTERRUPTER_SIZE) & !3,
        ))
    }

    // ------------------------------------------------------------------- the event ring

    /// Put `event` on the event ring of the one interrupter, and say so.
    ///
    /// The cycle bit goes in here rather than being the caller's, so that nothing that
    /// posts an event has to know where the ring has got to.
    fn post(&mut self, mut event: Trb) {
        let set = &mut self.interrupters[0];
        let Some(at) = segment(&self.dma, set) else {
            // A ring nothing has told the controller about is not a ring. Before
            // software has written the table this is every event, which is why the
            // events that matter are all raised after it has.
            return;
        };
        // The enqueue pointer catching the dequeue pointer with events still unread is
        // a ring with nowhere left to put one. There is nothing to do about it but say
        // so: dropping the event would leave a transfer that nothing ever completes,
        // and a host controller error is what a driver resets the controller over.
        if at == set.erdp & ERDP_POINTER && set.erdp & ERDP_BUSY != 0 {
            self.usbsts |= STS_HOST_ERROR;
            return;
        }
        event.control = (event.control & !CYCLE) | (set.cycle as u32);
        write_trb(&self.dma, at, event);

        // On past the end of this segment and, at the end of the table, back to the
        // first with the cycle turned over.
        let length = segment_length(&self.dma, set, set.segment).unwrap_or(0);
        set.offset += 1;
        if set.offset >= length {
            set.offset = 0;
            set.segment += 1;
            if set.segment as u32 >= set.erstsz {
                set.segment = 0;
                set.cycle = !set.cycle;
            }
        }
        // The busy bit says there is something on the ring that software has not taken.
        set.erdp |= ERDP_BUSY;
        set.iman |= IMAN_PENDING;
        self.usbsts |= STS_EINT;
        self.signal();
    }

    /// Work out what this controller is asserting and say so.
    ///
    /// A wire is held up for as long as an interrupt is waiting and both enables are on,
    /// so software lowers it by clearing the pending bit. A message has no such thing as
    /// being held, so one goes out when the wire would have gone up and not again until
    /// it has gone down: `sent` is what remembers which.
    fn signal(&mut self) {
        let raised = self.usbcmd & CMD_INTE != 0
            && self.interrupters[0].iman & (IMAN_PENDING | IMAN_ENABLE)
                == (IMAN_PENDING | IMAN_ENABLE);
        if raised && !self.asserted.pin {
            self.asserted.messages |= 1;
        }
        self.asserted.pin = raised;
    }

    // ---------------------------------------------------------------- the command ring

    /// Run every command waiting on the command ring, which is what ringing doorbell
    /// zero asks for.
    fn commands(&mut self) {
        if !self.running() {
            return;
        }
        // A ring of commands that each queue another would never end; a driver does not
        // write one, and a guest that did would otherwise hold the hart forever.
        for _ in 0..COMMANDS {
            let Some(ring) = self.command else { return };
            let Some(trb) = read_trb(&self.dma, ring.at) else {
                return;
            };
            if trb.cycle() != ring.cycle {
                return;
            }
            if trb.kind() == TRB_LINK {
                self.command = Some(follow(ring, trb));
                continue;
            }
            let at = ring.at;
            let (code, slot) = self.command(trb);
            self.command = Some(Ring {
                at: at + TRB,
                ..ring
            });
            self.post(Trb {
                parameter: at,
                status: code << 24,
                control: (TRB_COMMAND_EVENT << 10) | ((slot as u32) << 24),
            });
        }
    }

    /// Carry out one command, and say how it went and which slot it was about.
    fn command(&mut self, trb: Trb) -> (u32, usize) {
        let slot = (trb.control >> 24) as usize;
        match trb.kind() {
            TRB_ENABLE_SLOT => match (1..=self.enabled_slots()).find(|slot| !self.slots[*slot]) {
                Some(slot) => {
                    self.slots[slot] = true;
                    (SUCCESS, slot)
                }
                None => (NO_SLOTS, 0),
            },
            TRB_DISABLE_SLOT => match self.holds(slot) {
                true => {
                    self.slots[slot] = false;
                    self.waiting.retain(|(waiting, _)| *waiting != slot);
                    if let Some(context) = self.context(slot) {
                        self.set_slot_state(context, SLOT_ENABLED);
                    }
                    (SUCCESS, slot)
                }
                false => (SLOT_NOT_ENABLED, slot),
            },
            TRB_ADDRESS_DEVICE => (self.address(slot, trb), slot),
            TRB_CONFIGURE_ENDPOINT => (self.configure(slot, trb), slot),
            TRB_EVALUATE_CONTEXT => (self.evaluate(slot, trb), slot),
            // A halted endpoint becomes a stopped one, which is what lets software put
            // its dequeue pointer somewhere and start it again.
            TRB_RESET_ENDPOINT | TRB_STOP_ENDPOINT => {
                let endpoint = ((trb.control >> 16) & 0x1f) as usize;
                match self.endpoint(slot, endpoint) {
                    Some(at) if self.endpoint_state(at) == ENDPOINT_DISABLED => {
                        (ENDPOINT_NOT_ENABLED, slot)
                    }
                    Some(at) => {
                        self.set_endpoint_state(at, ENDPOINT_STOPPED);
                        self.waiting.remove(&(slot, endpoint));
                        (SUCCESS, slot)
                    }
                    None => (SLOT_NOT_ENABLED, slot),
                }
            }
            TRB_SET_DEQUEUE => {
                let endpoint = ((trb.control >> 16) & 0x1f) as usize;
                match self.endpoint(slot, endpoint) {
                    // Only an endpoint that is not running has a dequeue pointer
                    // software may move: one that is has the controller reading it.
                    Some(at) if self.endpoint_state(at) == ENDPOINT_RUNNING => {
                        (CONTEXT_STATE_ERROR, slot)
                    }
                    Some(at) if self.endpoint_state(at) == ENDPOINT_DISABLED => {
                        (ENDPOINT_NOT_ENABLED, slot)
                    }
                    Some(at) => {
                        // Both halves of it: where the ring is, and which cycle state
                        // software left it in, which live in the same doubleword.
                        self.dma.store(at + 8, 64, trb.parameter & !0xe);
                        (SUCCESS, slot)
                    }
                    None => (SLOT_NOT_ENABLED, slot),
                }
            }
            TRB_RESET_DEVICE => match self.context(slot) {
                Some(context) => {
                    for endpoint in 2..32 {
                        self.set_endpoint_state(context + endpoint * CONTEXT, ENDPOINT_DISABLED);
                    }
                    self.set_slot_state(context, SLOT_DEFAULT);
                    self.waiting.retain(|(waiting, _)| *waiting != slot);
                    (SUCCESS, slot)
                }
                None => (SLOT_NOT_ENABLED, slot),
            },
            TRB_NOOP_COMMAND => (SUCCESS, 0),
            // A command this controller does not have is refused by name rather than
            // ignored, so a driver waiting for its completion is not left waiting.
            _ => (TRB_ERROR, slot),
        }
    }

    /// How many slots software said it would use, which is what limits what may be
    /// handed out. xHCI 1.2, 5.4.7.
    fn enabled_slots(&self) -> usize {
        (self.config as usize & 0xff).min(SLOTS)
    }

    fn holds(&self, slot: usize) -> bool {
        self.slots.get(slot).copied().unwrap_or(false)
    }

    /// Give a slot an address and start its control endpoint.
    ///
    /// The input context says which port the device is on and where its endpoint zero
    /// ring is; the output context is what the controller writes back and what
    /// everything after this reads.
    fn address(&mut self, slot: usize, trb: Trb) -> u32 {
        let Some(context) = self.context(slot) else {
            return SLOT_NOT_ENABLED;
        };
        let input = trb.parameter & !0xf;
        // The slot context and endpoint zero's, which are the second and third contexts
        // of an input context: the first says which of them to look at.
        self.copy(input + CONTEXT, context, CONTEXT);
        self.copy(input + 2 * CONTEXT, context + CONTEXT, CONTEXT);

        let port = ((self.dma.load(context + 4, 32).unwrap_or(0) >> 16) & 0xff) as usize;
        if port == 0 || port > self.ports.len() {
            return PARAMETER_ERROR;
        }
        // With the block bit set no address is assigned and the device is left in the
        // state a reset leaves it in, which is what a driver does when all it wants is
        // somewhere to send a control transfer. xHCI 1.2, 4.6.5.
        let blocked = trb.control & BLOCK_SET_ADDRESS != 0;
        let (state, address) = match blocked {
            true => (SLOT_DEFAULT, 0),
            false => (SLOT_ADDRESSED, slot as u64),
        };
        if !blocked && self.ports[port - 1].device.is_none() {
            return PARAMETER_ERROR;
        }
        if !blocked {
            // The device is told its address, which is the one request a device answers
            // and then never uses: what the address routes is a packet, and there are
            // no packets here.
            if let Some(device) = &mut self.ports[port - 1].device {
                let _ = device.control(
                    Setup {
                        request_type: 0,
                        request: usb::SET_ADDRESS,
                        value: slot as u16,
                        index: 0,
                        length: 0,
                    },
                    &[],
                );
            }
        }
        let device_state = self.dma.load(context + 12, 32).unwrap_or(0) as u32;
        self.dma.store(
            context + 12,
            32,
            ((device_state & !0xff & !(0x1f << 27)) | address as u32 | (state << 27)) as u64,
        );
        self.set_endpoint_state(context + CONTEXT, ENDPOINT_RUNNING);
        SUCCESS
    }

    /// Add and drop endpoints, which is what a driver does when a configuration is
    /// chosen. xHCI 1.2, 4.6.6.
    fn configure(&mut self, slot: usize, trb: Trb) -> u32 {
        let Some(context) = self.context(slot) else {
            return SLOT_NOT_ENABLED;
        };
        // Deconfiguring puts the device back the way addressing left it, with every
        // endpoint but the control one gone.
        if trb.control & DECONFIGURE != 0 {
            for endpoint in 2..32 {
                self.set_endpoint_state(context + endpoint * CONTEXT, ENDPOINT_DISABLED);
                self.waiting.remove(&(slot, endpoint as usize));
            }
            self.set_slot_state(context, SLOT_ADDRESSED);
            return SUCCESS;
        }
        let input = trb.parameter & !0xf;
        let drop = self.dma.load(input, 32).unwrap_or(0) as u32;
        let add = self.dma.load(input + 4, 32).unwrap_or(0) as u32;
        for endpoint in 2..32u64 {
            let at = context + endpoint * CONTEXT;
            if drop & (1 << endpoint) != 0 {
                self.set_endpoint_state(at, ENDPOINT_DISABLED);
                self.waiting.remove(&(slot, endpoint as usize));
            }
            if add & (1 << endpoint) != 0 {
                self.copy(input + (endpoint + 1) * CONTEXT, at, CONTEXT);
                self.set_endpoint_state(at, ENDPOINT_RUNNING);
            }
        }
        // The slot context comes with it, since how many contexts are in use is one of
        // its fields and the driver has just changed it.
        if add & 1 != 0 {
            let state = self.dma.load(context + 12, 32).unwrap_or(0);
            self.copy(input + CONTEXT, context, CONTEXT);
            self.dma.store(context + 12, 32, state);
        }
        self.set_slot_state(context, SLOT_CONFIGURED);
        SUCCESS
    }

    /// Change what a driver has learned since it last said: the largest control packet
    /// the device turned out to take, which it only knows after reading the first eight
    /// bytes of the device descriptor. xHCI 1.2, 4.6.7.
    fn evaluate(&mut self, slot: usize, trb: Trb) -> u32 {
        let Some(context) = self.context(slot) else {
            return SLOT_NOT_ENABLED;
        };
        let input = trb.parameter & !0xf;
        let add = self.dma.load(input + 4, 32).unwrap_or(0) as u32;
        if add & 2 != 0 {
            let from = self.dma.load(input + 2 * CONTEXT + 4, 32).unwrap_or(0) as u32;
            let to = self.dma.load(context + CONTEXT + 4, 32).unwrap_or(0) as u32;
            const MAX_PACKET: u32 = 0xffff << 16;
            self.dma.store(
                context + CONTEXT + 4,
                32,
                ((to & !MAX_PACKET) | (from & MAX_PACKET)) as u64,
            );
        }
        // The only field of the slot context this command may change describes waking a
        // link up, and there are no links here.
        SUCCESS
    }

    // -------------------------------------------------------------------- the contexts

    /// Where the device context of `slot` is, which the driver told the controller by
    /// writing a table of them.
    fn context(&self, slot: usize) -> Option<u64> {
        if !self.holds(slot) || self.dcbaap == 0 {
            return None;
        }
        match self.dma.load(self.dcbaap + slot as u64 * 8, 64)? & !0x3f {
            0 => None,
            at => Some(at),
        }
    }

    /// And where one of its endpoint contexts is, if the slot has one.
    fn endpoint(&self, slot: usize, endpoint: usize) -> Option<u64> {
        (1..32)
            .contains(&endpoint)
            .then(|| self.context(slot))?
            .map(|context| context + endpoint as u64 * CONTEXT)
    }

    fn set_slot_state(&self, context: u64, state: u32) {
        let word = self.dma.load(context + 12, 32).unwrap_or(0) as u32;
        self.dma.store(
            context + 12,
            32,
            ((word & !(0x1f << 27)) | (state << 27)) as u64,
        );
    }

    fn set_endpoint_state(&self, at: u64, state: u32) {
        let word = self.dma.load(at, 32).unwrap_or(0) as u32;
        self.dma.store(at, 32, ((word & !7) | state) as u64);
    }

    fn endpoint_state(&self, at: u64) -> u32 {
        self.dma.load(at, 32).unwrap_or(0) as u32 & 7
    }

    /// Move `bytes` of context from one place in guest memory to another, which is the
    /// whole of what an input context does: software describes what it wants and the
    /// controller copies it into the context everything afterwards reads.
    fn copy(&self, from: u64, to: u64, bytes: u64) {
        let mut buffer = vec![0u8; bytes as usize];
        if self.dma.read(from, &mut buffer) {
            self.dma.write(to, &buffer);
        }
    }

    // ------------------------------------------------------------------- the transfers

    /// Run whatever is waiting on one endpoint's ring.
    ///
    /// Answers whether it got to the end of what was there. An endpoint that ran out of
    /// things to report stops with its dequeue pointer on the transfer that could not be
    /// finished, and is asked again once something has changed.
    fn transfers(&mut self, slot: usize, endpoint: usize) -> bool {
        let Some(at) = self.endpoint(slot, endpoint) else {
            return true;
        };
        match self.endpoint_state(at) {
            ENDPOINT_RUNNING => {}
            // Ringing the doorbell of a stopped endpoint is what starts it again, which
            // is the last step of every recovery from a stall: the driver resets the
            // endpoint, puts its dequeue pointer past whatever failed, and rings.
            // xHCI 1.2, figure 4-3.
            ENDPOINT_STOPPED => self.set_endpoint_state(at, ENDPOINT_RUNNING),
            // A halted endpoint has to be reset first, and a disabled one is not there.
            _ => return true,
        }
        let Some(word) = self.dma.load(at + 8, 64) else {
            return true;
        };
        let mut ring = Ring {
            at: word & !0xf,
            cycle: word & 1 != 0,
        };
        if ring.at == 0 {
            return true;
        }
        for _ in 0..TRANSFERS {
            let Some(trb) = read_trb(&self.dma, ring.at) else {
                break;
            };
            if trb.cycle() != ring.cycle {
                break;
            }
            if trb.kind() == TRB_LINK {
                ring = follow(ring, trb);
                continue;
            }
            match self.transfer(slot, endpoint, ring) {
                Some(next) => ring = next,
                // Nothing to send yet. The dequeue pointer stays where it is, so the
                // same transfer is the one that runs when there is.
                None => {
                    self.save(at, ring);
                    return false;
                }
            }
        }
        self.save(at, ring);
        true
    }

    /// Remember where an endpoint's ring has got to, in the endpoint context, which is
    /// where software looks for it.
    fn save(&self, at: u64, ring: Ring) {
        self.dma.store(at + 8, 64, ring.at | ring.cycle as u64);
    }

    /// Carry out the one transfer starting at `ring`, and say where the ring is after
    /// it. Nothing means it could not be carried out yet.
    fn transfer(&mut self, slot: usize, endpoint: usize, ring: Ring) -> Option<Ring> {
        let trbs = self.gather(endpoint, ring)?;
        match endpoint {
            1 => self.control(slot, &trbs),
            _ => self.interrupt(slot, endpoint, &trbs),
        }
    }

    /// Every transfer request block of the transfer starting at `ring`, or nothing if
    /// the whole of it is not there yet.
    ///
    /// What says where a transfer ends is not the same question on the control endpoint
    /// as anywhere else. Everywhere else it is the chain bit: a block, and each one
    /// after it that the one before said belongs with it. A control transfer is three
    /// stages the specification makes three transfer descriptors of, so a driver chains
    /// nothing and what ends it is the status stage. xHCI 1.2, 6.4.1.2.
    fn gather(&self, endpoint: usize, mut ring: Ring) -> Option<Vec<(u64, Trb)>> {
        let control = endpoint == 1;
        let mut trbs: Vec<(u64, Trb)> = Vec::new();
        for _ in 0..TRANSFERS {
            let trb = read_trb(&self.dma, ring.at)?;
            if trb.cycle() != ring.cycle {
                break;
            }
            if trb.kind() == TRB_LINK {
                ring = follow(ring, trb);
                continue;
            }
            trbs.push((ring.at, trb));
            ring.at += TRB;
            // A control ring carrying something that is not a setup stage is carrying
            // one block, since there is no transfer for it to be part of.
            if control && trbs.len() == 1 && trb.kind() != TRB_SETUP {
                return Some(trbs);
            }
            let ends = match control {
                true => trb.kind() == TRB_STATUS,
                false => trb.control & CHAIN == 0,
            };
            if ends {
                return Some(trbs);
            }
        }
        // A transfer whose last block has not been written yet is one to carry out when
        // it has, which is what leaving the dequeue pointer where it is arranges.
        match control {
            true => None,
            false => (!trbs.is_empty()).then_some(trbs),
        }
    }

    /// Where the ring is after a transfer of `trbs` blocks starting at `ring`, following
    /// the links it crossed.
    fn after(&self, mut ring: Ring, trbs: usize) -> Ring {
        let mut left = trbs;
        for _ in 0..TRANSFERS {
            if left == 0 {
                return ring;
            }
            match read_trb(&self.dma, ring.at) {
                Some(trb) if trb.kind() == TRB_LINK => ring = follow(ring, trb),
                _ => {
                    ring.at += TRB;
                    left -= 1;
                }
            }
        }
        ring
    }

    /// A control transfer: a setup stage, an optional data stage, and a status stage.
    fn control(&mut self, slot: usize, trbs: &[(u64, Trb)]) -> Option<Ring> {
        let (_, setup_trb) = trbs.first()?;
        // A transfer that does not start with a setup stage is not a control transfer.
        // The ring is moved past it either way, so a driver that queued one is not
        // stuck behind it forever.
        let ring = self.after(
            Ring {
                at: trbs[0].0,
                cycle: setup_trb.cycle(),
            },
            trbs.len(),
        );
        if setup_trb.kind() != TRB_SETUP {
            // A block that is not a stage of a control transfer is not part of one. A
            // no-operation is what a driver leaves behind when it takes a transfer
            // back, and it still finishes.
            if matches!(setup_trb.kind(), TRB_NOOP | TRB_EVENT_DATA)
                && setup_trb.control & INTERRUPT_ON_COMPLETION != 0
            {
                self.event_for(trbs[0].0, *setup_trb, slot, 1, SUCCESS, 0);
            }
            return Some(ring);
        }
        let setup = Setup::parse(setup_trb.parameter.to_le_bytes());

        // What goes out with it, gathered from the data stages before the device is
        // asked anything, since a device answers the whole transfer at once.
        let mut out = Vec::new();
        if !setup.to_host() {
            for (_, trb) in trbs.iter().filter(|(_, trb)| trb.kind() == TRB_DATA) {
                out.extend(self.buffer(*trb));
            }
        }

        let answer = match self.device(slot) {
            Some(device) => device.control(setup, &out),
            None => Err(usb::Stall),
        };
        let data = match answer {
            Ok(data) => data,
            Err(usb::Stall) => {
                if let Some(at) = self.endpoint(slot, 1) {
                    self.set_endpoint_state(at, ENDPOINT_HALTED);
                }
                self.transfer_event(trbs.last().copied(), slot, 1, STALL, 0);
                return Some(ring);
            }
        };

        // What comes back, into the data stages in the order they were queued. A stage
        // that took less than it asked for is one the transfer came up short in, and
        // saying so is what tells a driver how much of its buffer holds an answer.
        let mut left = data.as_slice();
        for (at, trb) in trbs {
            if trb.kind() != TRB_DATA {
                continue;
            }
            let wanted = (trb.status & 0x1ffff) as usize;
            let taken = left.len().min(wanted);
            if setup.to_host() {
                self.dma.write(trb.parameter, &left[..taken]);
            }
            left = &left[taken..];
            let remaining = (wanted - taken) as u32;
            if remaining > 0 && trb.control & (SHORT | INTERRUPT_ON_COMPLETION) != 0 {
                self.event_for(*at, *trb, slot, 1, SHORT_PACKET, remaining);
            } else if trb.control & INTERRUPT_ON_COMPLETION != 0 {
                self.event_for(*at, *trb, slot, 1, SUCCESS, 0);
            }
        }

        // And the status stage, which is what a driver actually waits on.
        if let Some(status) = trbs
            .iter()
            .find(|(_, trb)| trb.kind() == TRB_STATUS && trb.control & INTERRUPT_ON_COMPLETION != 0)
        {
            self.transfer_event(Some(*status), slot, 1, SUCCESS, 0);
        }
        Some(ring)
    }

    /// A transfer on an endpoint that is not the control one, which here is always an
    /// interrupt endpoint carrying reports in.
    fn interrupt(&mut self, slot: usize, endpoint: usize, trbs: &[(u64, Trb)]) -> Option<Ring> {
        let (first, trb) = *trbs.first()?;
        let ring = self.after(
            Ring {
                at: first,
                cycle: trb.cycle(),
            },
            trbs.len(),
        );
        // A block carrying no data still ends where it says it does: a no-operation is
        // what a driver leaves behind when it takes a transfer back, and an event data
        // block asks for an event carrying a word of the driver's own rather than an
        // address. Both complete without asking the device for anything.
        if trb.kind() == TRB_NOOP || trb.kind() == TRB_EVENT_DATA {
            for (at, trb) in trbs {
                if trb.control & INTERRUPT_ON_COMPLETION != 0 {
                    self.event_for(*at, *trb, slot, endpoint, SUCCESS, 0);
                }
            }
            return Some(ring);
        }
        if trb.kind() != TRB_NORMAL {
            return Some(ring);
        }
        // Which endpoint of the device this is: the identifier counts both directions
        // of every endpoint, and an address names the number and the direction.
        let address = ((endpoint as u8) / 2) | (((endpoint & 1) as u8) << 7);
        // Nothing on this machine has an endpoint carrying data out, so a transfer on
        // one is refused rather than left outstanding: a driver waiting forever for a
        // transfer the controller has no way to make is worse than one told so.
        if endpoint.is_multiple_of(2) {
            self.transfer_event(trbs.last().copied(), slot, endpoint, TRB_ERROR, 0);
            return Some(ring);
        }
        let report = self.device(slot)?.read(address)?;

        let mut left = report.as_slice();
        for (at, trb) in trbs {
            if trb.kind() != TRB_NORMAL {
                continue;
            }
            let wanted = (trb.status & 0x1ffff) as usize;
            let taken = left.len().min(wanted);
            self.dma.write(trb.parameter, &left[..taken]);
            left = &left[taken..];
            let remaining = (wanted - taken) as u32;
            let code = match remaining > 0 {
                true => SHORT_PACKET,
                false => SUCCESS,
            };
            if trb.control & INTERRUPT_ON_COMPLETION != 0
                || (remaining > 0 && trb.control & SHORT != 0)
            {
                self.event_for(*at, *trb, slot, endpoint, code, remaining);
            }
        }
        Some(ring)
    }

    /// The bytes a data stage is carrying out, which are either in memory or in the
    /// block itself when there are few enough of them to fit.
    fn buffer(&self, trb: Trb) -> Vec<u8> {
        let length = (trb.status & 0x1ffff) as usize;
        if trb.control & IMMEDIATE != 0 {
            return trb.parameter.to_le_bytes()[..length.min(8)].to_vec();
        }
        let mut bytes = vec![0u8; length];
        match self.dma.read(trb.parameter, &mut bytes) {
            true => bytes,
            false => Vec::new(),
        }
    }

    /// Say a transfer finished: which block it finished on, how it went, and how much of
    /// what was asked for did not arrive.
    ///
    /// An event data block is what it points at rather than where it was: the word in it
    /// is the driver's own and the event carries it back, which is what the flag saying
    /// so is for.
    fn event_for(&mut self, at: u64, trb: Trb, slot: usize, endpoint: usize, code: u32, left: u32) {
        let (parameter, data) = match trb.kind() == TRB_EVENT_DATA {
            true => (trb.parameter, EVENT_DATA),
            false => (at, 0),
        };
        self.post(Trb {
            parameter,
            status: (left & 0xffffff) | (code << 24),
            control: (TRB_TRANSFER_EVENT << 10)
                | data
                | ((endpoint as u32) << 16)
                | ((slot as u32) << 24),
        });
    }

    fn transfer_event(
        &mut self,
        trb: Option<(u64, Trb)>,
        slot: usize,
        endpoint: usize,
        code: u32,
        remaining: u32,
    ) {
        if let Some((at, trb)) = trb {
            self.event_for(at, trb, slot, endpoint, code, remaining);
        }
    }

    /// The device a slot's context says it is on.
    fn device(&mut self, slot: usize) -> Option<&mut Box<dyn usb::Device>> {
        let context = self.context(slot)?;
        let port = ((self.dma.load(context + 4, 32)? >> 16) & 0xff) as usize;
        self.ports.get_mut(port.checked_sub(1)?)?.device.as_mut()
    }

    // --------------------------------------------------------------------- the doorbells

    fn ring(&mut self, slot: usize, value: u32) {
        if !self.running() {
            return;
        }
        if slot == 0 {
            self.commands();
            return;
        }
        let endpoint = (value & 0xff) as usize;
        if !self.holds(slot) || !(1..32).contains(&endpoint) {
            return;
        }
        if !self.transfers(slot, endpoint) {
            self.waiting.insert((slot, endpoint));
        } else {
            self.waiting.remove(&(slot, endpoint));
        }
    }
}

/// How many commands one doorbell may run, and how many blocks one transfer may be made
/// of. Both are here so that a ring a guest built as a circle ends rather than holding
/// the hart that rang it forever.
const COMMANDS: usize = 4096;
const TRANSFERS: usize = 4096;

/// The registers whose second half is a register of its own, since every access here is
/// thirty-two bits wide and the addresses these hold are sixty-four.
const CRCR_HIGH: u64 = CRCR + 4;
const DCBAAP_HIGH: u64 = DCBAAP + 4;
const ERSTBA_HIGH: u64 = ERSTBA + 4;
const ERDP_HIGH: u64 = ERDP + 4;

/// One transfer request block: sixteen bytes, of which the first eight are an address or
/// the data itself, and the last four say what it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    fn cycle(&self) -> bool {
        self.control & CYCLE != 0
    }
}

fn read_trb(dma: &Dma, at: u64) -> Option<Trb> {
    Some(Trb {
        parameter: dma.load(at, 64)?,
        status: dma.load(at + 8, 32)? as u32,
        control: dma.load(at + 12, 32)? as u32,
    })
}

fn write_trb(dma: &Dma, at: u64, trb: Trb) {
    dma.store(at, 64, trb.parameter);
    dma.store(at + 8, 32, trb.status as u64);
    dma.store(at + 12, 32, trb.control as u64);
}

/// Where a link leads, and what it says about whose turn the entries there are.
fn follow(ring: Ring, link: Trb) -> Ring {
    Ring {
        at: link.parameter & !0xf,
        cycle: ring.cycle ^ (link.control & TOGGLE != 0),
    }
}

/// Where the next event goes: the segment the interrupter is in, as far into it as it
/// has got.
fn segment(dma: &Dma, set: &Interrupter) -> Option<u64> {
    if set.erstba == 0 || set.erstsz == 0 || set.segment as u32 >= set.erstsz {
        return None;
    }
    let entry = set.erstba + set.segment * ERST_ENTRY;
    let base = dma.load(entry, 64)? & !0x3f;
    (base != 0).then(|| base + set.offset * TRB)
}

/// How many blocks a segment of the table holds.
fn segment_length(dma: &Dma, set: &Interrupter, segment: u64) -> Option<u64> {
    if set.erstba == 0 || segment as u32 >= set.erstsz {
        return None;
    }
    match dma.load(set.erstba + segment * ERST_ENTRY + 8, 32)? & 0xffff {
        0 => None,
        length => Some(length),
    }
}

impl Function for Xhci {
    fn describe(&self) -> Vec<Field> {
        let connected = self
            .ports
            .iter()
            .filter(|port| port.device.is_some())
            .count();
        vec![
            field("usbcmd", Value::Bits(u64::from(self.usbcmd))),
            field("usbsts", Value::Bits(u64::from(self.usbsts))),
            field("command ring", Value::Flag(self.command.is_some())),
            field("device context base", Value::Bits(self.dcbaap)),
            field("ports", Value::Count(self.ports.len() as u64)),
            field("attached", Value::Count(connected as u64)),
            field(
                "slots in use",
                Value::Count(self.slots.iter().filter(|taken| **taken).count() as u64),
            ),
            field("endpoints waiting", Value::Count(self.waiting.len() as u64)),
        ]
    }

    fn header(&self) -> Header {
        Header {
            vendor: VENDOR,
            device: DEVICE,
            class: CLASS,
            subsystem_vendor: VENDOR,
            subsystem: DEVICE,
            bars: [
                Bar::Memory {
                    size: WINDOW,
                    prefetchable: false,
                    wide: false,
                },
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            // One wire, for a machine with nowhere to send a message, and one vector for
            // one interrupter on a machine that has.
            pin: 1,
            msix: Some(MsiX {
                vectors: INTERRUPTERS,
                table: (0, MSIX_TABLE),
                pending: (0, MSIX_PENDING),
            }),
            express: true,
        }
    }

    fn load(&mut self, bar: usize, offset: u64, size: u64) -> Result<u64, Exception> {
        if bar != 0 {
            return Err(Exception::LoadAccessFault(offset));
        }
        // Every register here is thirty-two bits, and a narrower access reads the part
        // of one its address selects. A driver reads them at their width; anything
        // else is a program looking.
        let word = self.register(offset & !3);
        let shift = (offset & 3) * 8;
        Ok(((word as u64) >> shift) & (u64::MAX >> (64 - size.min(32))))
    }

    fn store(&mut self, bar: usize, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if bar != 0 {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        // A write narrower than a register keeps the rest of it, which every one of
        // these is defined as: there is no register here a byte is the whole of.
        let register = offset & !3;
        let shift = (offset & 3) * 8;
        let mask = ((u64::MAX >> (64 - size.min(32))) << shift) as u32;
        let word = (self.register(register) & !mask) | ((value << shift) as u32 & mask);
        self.write(register, word);
        Ok(())
    }

    fn poll(&mut self) {
        // Everything that was waiting on a device having something to say, asked again.
        // A transfer that still cannot finish stays on the list.
        let waiting: Vec<(usize, usize)> = self.waiting.iter().copied().collect();
        for (slot, endpoint) in waiting {
            if self.transfers(slot, endpoint) {
                self.waiting.remove(&(slot, endpoint));
            }
        }
    }

    fn asserted(&mut self, messaging: bool) -> Asserted {
        let mut taken = self.asserted;
        self.asserted.messages = 0;
        if messaging {
            // Sending the message is what clears the pending bit. Software does not,
            // and cannot: it never sees the message go, so it would be clearing a bit
            // it has no way to know was set. A wire is the other way round, and stays
            // up until software says it has dealt with what raised it.
            // xHCI 1.2, 4.17.5.
            if taken.messages != 0 {
                self.interrupters[0].iman &= !IMAN_PENDING;
                self.signal();
                self.asserted.messages = 0;
            }
            // And a function sending messages does not pull its pin.
            taken.pin = false;
        }
        taken
    }
}

impl Xhci {
    /// What one of the registers in the window reads as.
    fn register(&self, offset: u64) -> u32 {
        match offset {
            _ if offset < CAPABILITY => self.capability(offset),
            _ if (PORTSC..PORTSC + (PORTS as u64) * 0x10).contains(&offset) => {
                let (port, register) = Self::port(offset - PORTSC);
                self.read_port(port, register)
            }
            _ if (OPERATIONAL..OPERATIONAL + 0x400).contains(&offset) => {
                self.operational(offset - OPERATIONAL)
            }
            _ if (EXTENDED..RUNTIME).contains(&offset) => self.extended(offset - EXTENDED),
            _ if (RUNTIME..DOORBELLS).contains(&offset) => self.runtime(offset - RUNTIME),
            // A doorbell is not storage: what it does is done when it is written, and
            // reading one back says nothing was left waiting.
            _ if (DOORBELLS..DOORBELLS + 0x400).contains(&offset) => 0,
            _ => 0,
        }
    }

    /// And what writing one does.
    fn write(&mut self, offset: u64, value: u32) {
        match offset {
            _ if (PORTSC..PORTSC + (PORTS as u64) * 0x10).contains(&offset) => {
                let (port, register) = Self::port(offset - PORTSC);
                self.write_port(port, register, value);
            }
            _ if (OPERATIONAL..OPERATIONAL + 0x400).contains(&offset) => {
                self.write_operational(offset - OPERATIONAL, value);
            }
            _ if (RUNTIME..DOORBELLS).contains(&offset) => {
                self.write_runtime(offset - RUNTIME, value);
            }
            _ if (DOORBELLS..DOORBELLS + 0x400).contains(&offset) => {
                self.ring(((offset - DOORBELLS) / 4) as usize, value);
            }
            // The capability registers and the extended capabilities describe what the
            // controller is, which is not software's to say.
            _ => {}
        }
    }
}
