//! The xHCI controller: what enumeration finds, what its registers answer, and what
//! happens on the rings a driver hands it work on.
//!
//! These drive the controller the way a driver does rather than the way a guest
//! instruction does, because that is where the substance is: the registers are a dozen
//! words and everything else is rings and contexts in memory. The path from a guest
//! store to a function's window is the PCI chapter's to prove, and the one test here
//! that goes through it only checks that this function is on the bus at all.

use std::sync::Arc;

use rysk::{
    bus::DRAM_BASE,
    device::{Device, Dma, Line, Msi},
    dram::Dram,
    hid::{Hid, Keys, Pointer},
    pci::{self, Asserted, Function, Root},
    usb,
    xhci::Xhci,
};

/// Where in guest memory this test puts each of the things a driver has to build.
const DCBAA: u64 = DRAM_BASE + 0x1_0000;
const COMMANDS: u64 = DRAM_BASE + 0x2_0000;
const ERST: u64 = DRAM_BASE + 0x3_0000;
const EVENTS: u64 = DRAM_BASE + 0x4_0000;
const INPUT: u64 = DRAM_BASE + 0x5_0000;
const CONTEXTS: u64 = DRAM_BASE + 0x6_0000;
const TRANSFERS: u64 = DRAM_BASE + 0x7_0000;
const BUFFER: u64 = DRAM_BASE + 0x8_0000;

/// How many blocks the event ring and each transfer ring hold here. Small enough that a
/// test can fill one and watch it wrap.
const EVENT_RING: u64 = 8;

/// One transfer request block, as the four words it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn kind(self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    fn code(self) -> u32 {
        self.status >> 24
    }

    fn left(self) -> u32 {
        self.status & 0xff_ffff
    }

    fn slot(self) -> u32 {
        self.control >> 24
    }
}

/// A block of `kind`, with the flags a caller wants and no cycle bit: the harness puts
/// that in, since whose turn a block is depends on where in the ring it went.
fn trb(kind: u32, parameter: u64, status: u32, flags: u32) -> Trb {
    Trb {
        parameter,
        status,
        control: (kind << 10) | flags,
    }
}

// The block types and flags, from the specification rather than from the model.
const NORMAL: u32 = 1;
const SETUP: u32 = 2;
const DATA: u32 = 3;
const STATUS: u32 = 4;
const LINK: u32 = 6;
const ENABLE_SLOT: u32 = 9;
const DISABLE_SLOT: u32 = 10;
const ADDRESS_DEVICE: u32 = 11;
const CONFIGURE_ENDPOINT: u32 = 12;
const EVALUATE_CONTEXT: u32 = 13;
const RESET_ENDPOINT: u32 = 14;
const STOP_ENDPOINT: u32 = 15;
const SET_DEQUEUE: u32 = 16;
const NOOP_COMMAND: u32 = 23;
const TRANSFER_EVENT: u32 = 32;
const COMMAND_EVENT: u32 = 33;
const PORT_EVENT: u32 = 34;

const TOGGLE: u32 = 1 << 1;
const SHORT: u32 = 1 << 2;
const CHAIN: u32 = 1 << 4;
const IOC: u32 = 1 << 5;
const IMMEDIATE: u32 = 1 << 6;
const BLOCK_ADDRESS: u32 = 1 << 9;
const DECONFIGURE: u32 = 1 << 9;
const TO_HOST: u32 = 1 << 16;

/// Completion codes.
const SUCCESS: u32 = 1;
const STALL: u32 = 6;
const NO_SLOTS: u32 = 9;
const SHORT_PACKET: u32 = 13;

/// Bits of the registers the tests below write.
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTE: u32 = 1 << 2;
const STS_HALTED: u32 = 1 << 0;
const STS_EINT: u32 = 1 << 3;
const STS_PORT_CHANGE: u32 = 1 << 4;
const IMAN_PENDING: u32 = 1 << 0;
const IMAN_ENABLE: u32 = 1 << 1;
const ERDP_BUSY: u64 = 1 << 3;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_CONNECT_CHANGE: u32 = 1 << 17;
const PORT_RESET_CHANGE: u32 = 1 << 21;

/// The two endpoint identifiers a test uses: the control endpoint, and the first one
/// that carries reports in.
const CONTROL: usize = 1;
const REPORTS: usize = 3;

/// A driver, and the controller it drives.
struct Host {
    memory: Arc<Dram>,
    xhci: Xhci,
    keys: Keys,
    pointer: Pointer,
    /// Where the four regions turned out to be, read out of the capability registers
    /// rather than assumed.
    operational: u64,
    runtime: u64,
    doorbells: u64,
    ports: u64,
    /// Where this driver has got to on each ring, and whose turn the entry there is.
    command: u64,
    command_cycle: bool,
    event: u64,
    event_cycle: bool,
    transfer: u64,
    transfer_cycle: bool,
}

/// A controller with a keyboard on port one and a mouse on port two, and the memory
/// they share, before anything has been written to it.
fn host() -> Host {
    let memory = Arc::new(Dram::with_size(Vec::new(), 1024 * 1024));
    let (keyboard, keys) = Hid::keyboard();
    let (mouse, pointer) = Hid::mouse();
    let devices: Vec<Box<dyn usb::Device>> = vec![Box::new(keyboard), Box::new(mouse)];
    let xhci = Xhci::new(Dma::new(memory.clone()), devices);

    let mut host = Host {
        memory,
        xhci,
        keys,
        pointer,
        operational: 0,
        runtime: 0,
        doorbells: 0,
        ports: 0,
        command: COMMANDS,
        command_cycle: true,
        event: EVENTS,
        event_cycle: true,
        transfer: TRANSFERS,
        transfer_cycle: true,
    };
    host.operational = (host.read(0) & 0xff) as u64;
    host.ports = host.operational + 0x400;
    host.runtime = host.read(0x18) as u64;
    host.doorbells = host.read(0x14) as u64;
    host
}

impl Host {
    fn read(&mut self, register: u64) -> u32 {
        self.xhci.load(0, register, 32).expect("a register") as u32
    }

    fn write(&mut self, register: u64, value: u32) {
        self.xhci
            .store(0, register, 32, value as u64)
            .expect("a register");
    }

    fn write64(&mut self, register: u64, value: u64) {
        self.write(register, value as u32);
        self.write(register + 4, (value >> 32) as u32);
    }

    /// The operational registers, which are wherever the capability length said.
    fn op(&mut self, register: u64) -> u32 {
        let at = self.operational + register;
        self.read(at)
    }

    fn write_op(&mut self, register: u64, value: u32) {
        let at = self.operational + register;
        self.write(at, value);
    }

    /// One port's status register, counting the ports from one the way the hardware
    /// numbers them.
    fn port(&mut self, port: u64) -> u32 {
        let at = self.ports + (port - 1) * 0x10;
        self.read(at)
    }

    fn write_port(&mut self, port: u64, value: u32) {
        let at = self.ports + (port - 1) * 0x10;
        self.write(at, value);
    }

    /// The one interrupter's registers.
    fn interrupter(&mut self, register: u64) -> u32 {
        let at = self.runtime + 0x20 + register;
        self.read(at)
    }

    fn write_interrupter(&mut self, register: u64, value: u32) {
        let at = self.runtime + 0x20 + register;
        self.write(at, value);
    }

    fn doorbell(&mut self, slot: usize, endpoint: usize) {
        let at = self.doorbells + slot as u64 * 4;
        self.write(at, endpoint as u32);
    }

    fn poke(&self, at: u64, bytes: &[u8]) {
        assert!(self.memory.write(at, bytes, 0), "{at:#x} is not memory");
    }

    fn peek(&self, at: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0u8; len];
        assert!(
            Dma::new(self.memory.clone()).read(at, &mut bytes),
            "{at:#x} is not memory"
        );
        bytes
    }

    fn put_trb(&self, at: u64, trb: Trb) {
        self.poke(at, &trb.parameter.to_le_bytes());
        self.poke(at + 8, &trb.status.to_le_bytes());
        self.poke(at + 12, &trb.control.to_le_bytes());
    }

    fn get_trb(&self, at: u64) -> Trb {
        let bytes = self.peek(at, 16);
        Trb {
            parameter: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            status: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            control: u32::from_le_bytes(bytes[12..].try_into().unwrap()),
        }
    }

    /// Everything the controller has put on the event ring since this was last asked,
    /// telling it afterwards how far this driver got.
    fn events(&mut self) -> Vec<Trb> {
        let mut events = Vec::new();
        loop {
            let trb = self.get_trb(self.event);
            if (trb.control & 1 != 0) != self.event_cycle {
                break;
            }
            events.push(trb);
            self.event += 16;
            if self.event >= EVENTS + EVENT_RING * 16 {
                self.event = EVENTS;
                self.event_cycle = !self.event_cycle;
            }
        }
        let erdp = self.event | ERDP_BUSY;
        let at = self.runtime + 0x20 + 0x18;
        self.write64(at, erdp);
        events
    }

    /// A driver's whole setup: the tables, the two rings and the interrupter, then run.
    fn start(&mut self) {
        self.write_op(0x00, CMD_RESET);
        // Thirty-two slots, and the array their contexts are named by.
        self.write_op(0x38, 32);
        self.write64(self.operational + 0x30, DCBAA);
        // The command ring, in the cycle state this driver starts in.
        self.write64(self.operational + 0x18, COMMANDS | 1);
        // One segment of event ring, and where this driver has read up to in it.
        self.poke(ERST, &EVENTS.to_le_bytes());
        self.poke(ERST + 8, &(EVENT_RING as u32).to_le_bytes());
        self.write_interrupter(0x08, 1);
        let (runtime, erst) = (self.runtime, ERST);
        self.write64(runtime + 0x20 + 0x10, erst);
        self.write64(runtime + 0x20 + 0x18, EVENTS);
        self.write_interrupter(0x00, IMAN_ENABLE);
        self.write_op(0x00, CMD_RUN | CMD_INTE);
    }

    /// Put one command on the command ring and ring for it, and answer the events it
    /// produced.
    fn command(&mut self, mut command: Trb) -> Vec<Trb> {
        command.control |= self.command_cycle as u32;
        self.put_trb(self.command, command);
        self.command += 16;
        self.doorbell(0, 0);
        self.events()
    }

    /// The one event a command produced, which is what every command here produces.
    fn one(&mut self, command: Trb) -> Trb {
        let events = self.command(command);
        assert_eq!(events.len(), 1, "one command, one answer: {events:?}");
        events[0]
    }

    /// Put a transfer on an endpoint's ring and ring for it.
    fn transfer(&mut self, slot: usize, endpoint: usize, trbs: &[Trb]) -> Vec<Trb> {
        for mut trb in trbs.iter().copied() {
            trb.control |= self.transfer_cycle as u32;
            self.put_trb(self.transfer, trb);
            self.transfer += 16;
        }
        self.doorbell(slot, endpoint);
        self.events()
    }

    /// Reset a port and take the slot the driver would: enable one, describe the device
    /// in an input context, and address it.
    fn attach(&mut self, port: u64) -> usize {
        self.write_port(port, PORT_RESET);
        self.events();

        let slot = self.one(trb(ENABLE_SLOT, 0, 0, 0)).slot() as usize;
        let context = CONTEXTS + slot as u64 * 0x400;
        self.poke(DCBAA + slot as u64 * 8, &context.to_le_bytes());

        // What the driver says about the device: which port it is on, how fast it runs,
        // and where its control endpoint's ring is.
        let speed = (self.port(port) >> 10) & 0xf;
        self.poke(INPUT, &0u32.to_le_bytes());
        self.poke(INPUT + 4, &0b11u32.to_le_bytes());
        self.poke(INPUT + 32, &((speed << 20) | (1 << 27)).to_le_bytes());
        self.poke(INPUT + 36, &((port as u32) << 16).to_le_bytes());
        // A control endpoint, eight bytes at a time, and the ring it reads.
        self.poke(INPUT + 64 + 4, &((4u32 << 3) | (8 << 16)).to_le_bytes());
        self.poke(INPUT + 64 + 8, &(TRANSFERS | 1).to_le_bytes());

        let answer = self.one(trb(ADDRESS_DEVICE, INPUT, 0, (slot as u32) << 24));
        assert_eq!(answer.code(), SUCCESS, "addressing the device");
        slot
    }

    /// A control transfer that reads bytes out of the device, as the three stages it is.
    ///
    /// Nothing chains them, which is what the driver does: the specification makes each
    /// stage a transfer descriptor of its own and the chain bit says something else.
    fn get(&mut self, slot: usize, setup: [u8; 8], length: u32) -> (Vec<Trb>, Vec<u8>) {
        let events = self.transfer(
            slot,
            CONTROL,
            &[
                trb(SETUP, u64::from_le_bytes(setup), 8, IMMEDIATE),
                trb(DATA, BUFFER, length, TO_HOST | SHORT),
                trb(STATUS, 0, 0, IOC),
            ],
        );
        let bytes = self.peek(BUFFER, length as usize);
        (events, bytes)
    }
}

/// The eight bytes of a request for a descriptor.
fn descriptor(kind: u8, index: u8, length: u16) -> [u8; 8] {
    let value = ((kind as u16) << 8) | index as u16;
    let mut setup = [0x80, 6, 0, 0, 0, 0, 0, 0];
    setup[2..4].copy_from_slice(&value.to_le_bytes());
    setup[6..8].copy_from_slice(&length.to_le_bytes());
    setup
}

// ------------------------------------------------------------------ what is on the bus

#[test]
fn enumeration_finds_a_usb_controller_speaking_xhci() {
    let root = Root::new(std::array::from_fn(|_| Line::default()), Msi::default());
    let memory = Arc::new(Dram::with_size(Vec::new(), 1024 * 1024));
    root.plug(2, Box::new(Xhci::new(Dma::new(memory), Vec::new())));
    let mut config = root.config();

    let class = config.load((2 << 15) + 0x08, 32).expect("config space");
    let capabilities = config.load((2 << 15) + 0x34, 32).expect("config space");
    let interrupt = config.load((2 << 15) + 0x3c, 32).expect("config space");

    assert_eq!(
        class, 0x0c03_3001,
        "serial bus, USB, xHCI programming interface, revision one"
    );
    assert_ne!(capabilities, 0, "and it has capabilities to declare");
    assert_eq!((interrupt >> 8) & 0xff, 1, "on the first of the four wires");
}

// ------------------------------------------------------------- the capability registers

#[test]
fn the_capability_registers_say_where_the_other_regions_are() {
    let mut host = host();

    assert_eq!(host.read(0) & 0xff, 0x20, "how long this region is");
    assert_eq!(host.read(0) >> 16, 0x0120, "version 1.2 of the interface");
    assert_eq!(host.read(0x04) & 0xff, 32, "thirty-two slots");
    assert_eq!((host.read(0x04) >> 8) & 0x7ff, 1, "one interrupter");
    assert_eq!(host.read(0x04) >> 24, 4, "four ports");
    assert_ne!(host.read(0x14), 0, "the doorbells are somewhere");
    assert_ne!(host.read(0x18), 0, "and so are the runtime registers");
}

/// The parameters a driver reads before it allocates anything: how it may address
/// memory, how long a context is, and whether it has to lend the controller any.
#[test]
fn the_controller_takes_sixty_four_bit_addresses_and_short_contexts() {
    let mut host = host();
    let params = host.read(0x10);

    assert_eq!(params & 1, 1, "sixty-four bit addressing");
    assert_eq!(params & 4, 0, "contexts of thirty-two bytes");
    assert_eq!(
        params & 8,
        0,
        "no port power control, so a port is always on"
    );
    assert_ne!(
        params >> 16,
        0,
        "and the extended capabilities are somewhere"
    );
    assert_eq!(
        host.read(0x08) >> 21,
        0,
        "and it borrows no memory of the guest's for scratch"
    );
}

#[test]
fn the_extended_capabilities_say_which_ports_speak_which_revision() {
    let mut host = host();
    let at = (host.read(0x10) >> 16) as u64 * 4;

    assert_eq!(host.read(at) & 0xff, 2, "a supported protocol capability");
    assert_eq!(host.read(at) >> 24, 2, "revision two of the bus");
    assert_eq!(host.read(at + 4), u32::from_le_bytes(*b"USB "), "of USB");
    assert_eq!(host.read(at + 8) & 0xff, 1, "starting at the first port");
    assert_eq!((host.read(at + 8) >> 8) & 0xff, 2, "and two of them");

    let next = at + ((host.read(at) >> 8) & 0xff) as u64 * 4;
    assert_eq!(host.read(next) >> 24, 3, "then revision three");
    assert_eq!(host.read(next + 8) & 0xff, 3, "starting at the third port");
    assert_eq!((host.read(next + 8) >> 8) & 0xff, 2, "and two of them");
    assert_eq!(
        (host.read(next) >> 8) & 0xff,
        0,
        "and nothing after it, which is what ends the list"
    );
}

/// A driver may not write what the controller is.
#[test]
fn the_capability_registers_are_the_controllers_to_say() {
    let mut host = host();
    host.write(0x04, 0);
    host.write(0x10, 0);

    assert_eq!(host.read(0x04) & 0xff, 32);
    assert_eq!(host.read(0x10) & 1, 1);
}

// ----------------------------------------------------------------------- running it

#[test]
fn a_controller_that_has_not_been_started_is_halted() {
    let mut host = host();
    assert_eq!(host.op(0x04) & STS_HALTED, STS_HALTED);
    assert_eq!(host.op(0x00) & CMD_RUN, 0);
}

#[test]
fn running_it_and_stopping_it_move_the_halted_bit() {
    let mut host = host();
    host.write_op(0x00, CMD_RUN);
    assert_eq!(host.op(0x04) & STS_HALTED, 0, "running");

    host.write_op(0x00, 0);
    assert_eq!(host.op(0x04) & STS_HALTED, STS_HALTED, "and halted again");
}

#[test]
fn resetting_it_forgets_everything_software_had_written() {
    let mut host = host();
    host.start();
    assert_ne!(host.op(0x38), 0, "the slots software asked for");

    host.write_op(0x00, CMD_RESET);

    assert_eq!(host.op(0x38), 0, "which the reset took back");
    assert_eq!(host.op(0x04) & STS_HALTED, STS_HALTED);
    assert_eq!(host.op(0x00), 0, "and the reset bit does not stay set");
    assert_eq!(
        host.op(0x30),
        0,
        "and the table of device contexts is forgotten"
    );
}

/// The page sizes the controller can be given memory in, which is one.
#[test]
fn the_controller_wants_four_kibibyte_pages() {
    let mut host = host();
    assert_eq!(host.op(0x08), 1);
}

/// The command ring pointer is not something software may read back: what it may learn
/// is whether the ring is running.
#[test]
fn the_command_ring_says_only_whether_it_is_running() {
    let mut host = host();
    host.write64(host.operational + 0x18, COMMANDS | 1);

    assert_eq!(host.op(0x18), 1 << 3, "running, and nothing about where");
    assert_eq!(host.op(0x1c), 0);
}

// --------------------------------------------------------------------------- the ports

#[test]
fn a_port_with_something_in_it_says_so_before_anything_has_looked() {
    let mut host = host();

    for port in 1..=2 {
        let status = host.port(port);
        assert_eq!(status & PORT_CONNECTED, PORT_CONNECTED, "port {port}");
        assert_eq!(status & PORT_CONNECT_CHANGE, PORT_CONNECT_CHANGE);
        assert_eq!(status & PORT_ENABLED, 0, "and is not enabled yet");
        assert_eq!(status & PORT_POWER, PORT_POWER, "and is powered");
    }
    for port in 3..=4 {
        let status = host.port(port);
        assert_eq!(status & PORT_CONNECTED, 0, "port {port} has nothing in it");
        assert_eq!(status & PORT_CONNECT_CHANGE, 0);
    }
}

/// What a reset finds out, which is the only way the speed of what is attached is
/// learned: the field says nothing until one has happened.
#[test]
fn resetting_a_port_enables_it_and_finds_out_how_fast_it_runs() {
    let mut host = host();
    assert_eq!((host.port(1) >> 10) & 0xf, 0, "no speed before a reset");

    host.write_port(1, PORT_RESET);
    let status = host.port(1);

    assert_eq!(status & PORT_ENABLED, PORT_ENABLED);
    assert_eq!(status & PORT_RESET, 0, "and the reset is over");
    assert_eq!(status & PORT_RESET_CHANGE, PORT_RESET_CHANGE);
    assert_eq!(
        (status >> 10) & 0xf,
        1,
        "full speed, which is what a hid is"
    );
    assert_eq!((status >> 5) & 0xf, 0, "and the link is up");
}

#[test]
fn resetting_an_empty_port_enables_nothing() {
    let mut host = host();
    host.write_port(3, PORT_RESET);
    let status = host.port(3);

    assert_eq!(status & PORT_ENABLED, 0);
    assert_eq!(
        status & PORT_RESET_CHANGE,
        PORT_RESET_CHANGE,
        "but it is over"
    );
}

#[test]
fn a_change_bit_is_cleared_by_writing_a_one_over_it() {
    let mut host = host();
    assert_ne!(host.port(1) & PORT_CONNECT_CHANGE, 0);

    host.write_port(1, PORT_CONNECT_CHANGE);
    assert_eq!(host.port(1) & PORT_CONNECT_CHANGE, 0);
    assert_ne!(host.port(1) & PORT_CONNECTED, 0, "and what is there stays");
}

#[test]
fn a_port_that_changed_is_an_event_and_a_bit_of_the_status() {
    let mut host = host();
    host.start();
    host.events();
    host.write_op(0x04, STS_PORT_CHANGE);

    host.write_port(1, PORT_RESET);
    let events = host.events();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), PORT_EVENT);
    assert_eq!(events[0].parameter >> 24, 1, "which port it was");
    assert_eq!(events[0].code(), SUCCESS);
    assert_ne!(host.op(0x04) & STS_PORT_CHANGE, 0);
}

// ------------------------------------------------------------------------ the commands

#[test]
fn a_command_is_answered_on_the_event_ring() {
    let mut host = host();
    host.start();
    host.events();

    let answer = host.one(trb(NOOP_COMMAND, 0, 0, 0));

    assert_eq!(answer.kind(), COMMAND_EVENT);
    assert_eq!(answer.code(), SUCCESS);
    assert_eq!(
        answer.parameter, COMMANDS,
        "and it names the command it answers"
    );
}

#[test]
fn a_command_the_controller_does_not_have_is_refused_rather_than_ignored() {
    let mut host = host();
    host.start();
    host.events();

    // Get Port Bandwidth, which this controller has no bandwidth to report.
    let answer = host.one(trb(21, 0, 0, 0));

    assert_eq!(answer.kind(), COMMAND_EVENT);
    assert_eq!(answer.code(), 5, "a block it could not carry out");
}

#[test]
fn a_command_ring_that_links_back_to_its_start_is_followed() {
    let mut host = host();
    host.start();
    host.events();

    // Two commands, then a link home, then a third in the cycle state the link says.
    host.put_trb(COMMANDS + 16, trb(LINK, COMMANDS, 0, TOGGLE | 1));
    host.put_trb(COMMANDS, trb(NOOP_COMMAND, 0, 0, 1));
    host.doorbell(0, 0);
    let first = host.events();
    // The ring turned over, so the next command is written with the other cycle bit.
    host.put_trb(COMMANDS, trb(NOOP_COMMAND, 0, 0, 0));
    host.doorbell(0, 0);
    let second = host.events();

    assert_eq!(first.len(), 1, "the one command before the link");
    assert_eq!(second.len(), 1, "and the one after it, back at the start");
    assert_eq!(second[0].parameter, COMMANDS);
}

#[test]
fn enabling_a_slot_hands_one_out_and_disabling_it_gives_it_back() {
    let mut host = host();
    host.start();
    host.events();

    let first = host.one(trb(ENABLE_SLOT, 0, 0, 0));
    assert_eq!(first.code(), SUCCESS);
    assert_eq!(first.slot(), 1, "the first slot there is");

    let second = host.one(trb(ENABLE_SLOT, 0, 0, 0));
    assert_eq!(second.slot(), 2, "and then the next");

    let freed = host.one(trb(DISABLE_SLOT, 0, 0, 1 << 24));
    assert_eq!(freed.code(), SUCCESS);

    let again = host.one(trb(ENABLE_SLOT, 0, 0, 0));
    assert_eq!(again.slot(), 1, "which is handed out again");
}

#[test]
fn a_slot_that_was_never_enabled_cannot_be_disabled() {
    let mut host = host();
    host.start();
    host.events();

    let answer = host.one(trb(DISABLE_SLOT, 0, 0, 7 << 24));
    assert_eq!(answer.code(), 11, "there is no such slot");
}

/// Only as many slots as software said it would use, which is what the configure
/// register is for.
#[test]
fn slots_run_out_at_the_number_software_asked_for() {
    let mut host = host();
    host.start();
    host.write_op(0x38, 2);
    host.events();

    assert_eq!(host.one(trb(ENABLE_SLOT, 0, 0, 0)).code(), SUCCESS);
    assert_eq!(host.one(trb(ENABLE_SLOT, 0, 0, 0)).code(), SUCCESS);
    assert_eq!(host.one(trb(ENABLE_SLOT, 0, 0, 0)).code(), NO_SLOTS);
}

#[test]
fn addressing_a_device_copies_what_the_driver_said_into_the_context_it_reads() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    let context = CONTEXTS + slot as u64 * 0x400;

    let state = u32::from_le_bytes(host.peek(context + 12, 4).try_into().unwrap());
    let port = u32::from_le_bytes(host.peek(context + 4, 4).try_into().unwrap()) >> 16;
    let endpoint = u32::from_le_bytes(host.peek(context + 32, 4).try_into().unwrap());

    assert_eq!(state & 0xff, slot as u32, "the address it was given");
    assert_eq!(state >> 27, 2, "addressed");
    assert_eq!(port & 0xff, 1, "on the port the driver said");
    assert_eq!(endpoint & 7, 1, "and its control endpoint is running");
}

/// A driver that only wants somewhere to send a control transfer asks for the address
/// not to be assigned, and the device is left in the state a reset left it in.
#[test]
fn addressing_with_the_block_bit_set_assigns_no_address() {
    let mut host = host();
    host.start();
    host.events();
    host.write_port(1, PORT_RESET);
    host.events();
    let slot = host.one(trb(ENABLE_SLOT, 0, 0, 0)).slot() as usize;
    let context = CONTEXTS + slot as u64 * 0x400;
    host.poke(DCBAA + slot as u64 * 8, &context.to_le_bytes());
    host.poke(INPUT + 4, &0b11u32.to_le_bytes());
    host.poke(INPUT + 32, &((1u32 << 20) | (1 << 27)).to_le_bytes());
    host.poke(INPUT + 36, &(1u32 << 16).to_le_bytes());
    host.poke(INPUT + 64 + 8, &(TRANSFERS | 1).to_le_bytes());

    let answer = host.one(trb(
        ADDRESS_DEVICE,
        INPUT,
        0,
        BLOCK_ADDRESS | ((slot as u32) << 24),
    ));
    let state = u32::from_le_bytes(host.peek(context + 12, 4).try_into().unwrap());

    assert_eq!(answer.code(), SUCCESS);
    assert_eq!(state & 0xff, 0, "no address");
    assert_eq!(
        state >> 27,
        1,
        "and still in the state a reset leaves it in"
    );
}

#[test]
fn evaluating_a_context_changes_the_largest_packet_the_control_endpoint_takes() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    let context = CONTEXTS + slot as u64 * 0x400;

    host.poke(INPUT + 4, &0b10u32.to_le_bytes());
    host.poke(INPUT + 64 + 4, &((4u32 << 3) | (64 << 16)).to_le_bytes());
    let answer = host.one(trb(EVALUATE_CONTEXT, INPUT, 0, (slot as u32) << 24));
    let endpoint = u32::from_le_bytes(host.peek(context + 32 + 4, 4).try_into().unwrap());

    assert_eq!(answer.code(), SUCCESS);
    assert_eq!(endpoint >> 16, 64, "the packet size the driver found out");
    assert_eq!((endpoint >> 3) & 7, 4, "and it is still a control endpoint");
}

#[test]
fn configuring_an_endpoint_starts_it_and_deconfiguring_takes_it_away() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    let context = CONTEXTS + slot as u64 * 0x400;
    host.poke(INPUT, &0u32.to_le_bytes());
    host.poke(INPUT + 4, &(1u32 | (1 << REPORTS)).to_le_bytes());
    // An interrupt IN endpoint, eight bytes at a time, and where its ring is.
    host.poke(
        INPUT + (REPORTS as u64 + 1) * 32 + 4,
        &((7u32 << 3) | (8 << 16)).to_le_bytes(),
    );
    host.poke(
        INPUT + (REPORTS as u64 + 1) * 32 + 8,
        &((TRANSFERS + 0x1000) | 1).to_le_bytes(),
    );
    let added = host.one(trb(CONFIGURE_ENDPOINT, INPUT, 0, (slot as u32) << 24));
    let running = u32::from_le_bytes(
        host.peek(context + REPORTS as u64 * 32, 4)
            .try_into()
            .unwrap(),
    );

    let gone = host.one(trb(
        CONFIGURE_ENDPOINT,
        INPUT,
        0,
        DECONFIGURE | ((slot as u32) << 24),
    ));
    let stopped = u32::from_le_bytes(
        host.peek(context + REPORTS as u64 * 32, 4)
            .try_into()
            .unwrap(),
    );

    assert_eq!(added.code(), SUCCESS);
    assert_eq!(running & 7, 1, "running");
    assert_eq!(gone.code(), SUCCESS);
    assert_eq!(stopped & 7, 0, "and then disabled");
}

/// Where a stopped endpoint's ring is is software's to set; where a running one's is is
/// the controller's, and moving it under one would lose whatever it was reading.
#[test]
fn a_running_endpoints_dequeue_pointer_is_not_softwares_to_move() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    let refused = host.one(trb(
        SET_DEQUEUE,
        TRANSFERS | 1,
        0,
        ((CONTROL as u32) << 16) | ((slot as u32) << 24),
    ));
    assert_eq!(refused.code(), 19, "the endpoint is in the wrong state");

    host.one(trb(
        STOP_ENDPOINT,
        0,
        0,
        ((CONTROL as u32) << 16) | ((slot as u32) << 24),
    ));
    let allowed = host.one(trb(
        SET_DEQUEUE,
        (TRANSFERS + 0x40) | 1,
        0,
        ((CONTROL as u32) << 16) | ((slot as u32) << 24),
    ));
    let context = CONTEXTS + slot as u64 * 0x400;
    let deq = u64::from_le_bytes(host.peek(context + 32 + 8, 8).try_into().unwrap());

    assert_eq!(allowed.code(), SUCCESS);
    assert_eq!(deq, (TRANSFERS + 0x40) | 1, "where software put it");
}

// ------------------------------------------------------------------------- the transfers

#[test]
fn a_control_transfer_reads_a_descriptor_out_of_the_device() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    let (events, bytes) = host.get(slot, descriptor(1, 0, 18), 18);

    assert_eq!(events.len(), 1, "one event, on the stage with the flag set");
    assert_eq!(events[0].kind(), TRANSFER_EVENT);
    assert_eq!(events[0].code(), SUCCESS);
    assert_eq!(events[0].slot(), slot as u32);
    assert_eq!(bytes[0], 18, "eighteen bytes of device descriptor");
    assert_eq!(bytes[1], 1, "which is what it says it is");
    assert_eq!(bytes[7], 8, "eight bytes at a time on endpoint zero");
}

/// A transfer that asked for more than the device had says how much did not arrive,
/// which is the only way a driver learns how long an answer was.
#[test]
fn a_transfer_that_came_up_short_says_how_much_did_not_arrive() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    let (events, bytes) = host.get(slot, descriptor(1, 0, 255), 255);

    assert_eq!(
        events.len(),
        2,
        "the stage that came up short, and the last"
    );
    assert_eq!(events[0].code(), SHORT_PACKET);
    assert_eq!(events[0].left(), 255 - 18, "what did not arrive");
    assert_eq!(events[1].code(), SUCCESS, "and the transfer still finished");
    assert_eq!(bytes[0], 18);
}

#[test]
fn a_request_the_device_refuses_halts_the_endpoint() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    // A device qualifier, which a device with only one speed does not have.
    let (events, _) = host.get(slot, descriptor(6, 0, 10), 10);
    let context = CONTEXTS + slot as u64 * 0x400;
    let endpoint = u32::from_le_bytes(host.peek(context + 32, 4).try_into().unwrap());

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].code(), STALL);
    assert_eq!(endpoint & 7, 2, "and the endpoint is halted");
}

/// A transfer whose last stage has not been written yet is one to carry out when it
/// has, rather than a setup stage carried out on its own.
#[test]
fn a_control_transfer_missing_its_status_stage_waits_for_it() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    let waiting = host.transfer(
        slot,
        CONTROL,
        &[
            trb(
                SETUP,
                u64::from_le_bytes(descriptor(1, 0, 18)),
                8,
                IMMEDIATE,
            ),
            trb(DATA, BUFFER, 18, TO_HOST | SHORT),
        ],
    );
    assert!(waiting.is_empty(), "nothing has finished");

    let finished = host.transfer(slot, CONTROL, &[trb(STATUS, 0, 0, IOC)]);

    assert_eq!(finished.len(), 1, "and now the whole of it runs");
    assert_eq!(finished[0].code(), SUCCESS);
    assert_eq!(host.peek(BUFFER, 1)[0], 18, "with the descriptor read");
}

/// What a driver does after a stall, which is what every enumeration does at least
/// once: a device qualifier a device with one speed does not have is refused, and the
/// endpoint has to come back from it.
#[test]
fn an_endpoint_comes_back_from_a_stall_when_it_is_reset_and_rung_again() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    let endpoint = ((CONTROL as u32) << 16) | ((slot as u32) << 24);

    let (stalled, _) = host.get(slot, descriptor(6, 0, 10), 10);
    assert_eq!(stalled[0].code(), STALL);

    // Reset it, put its ring where the driver wants to start again, and ring.
    assert_eq!(
        host.one(trb(RESET_ENDPOINT, 0, 0, endpoint)).code(),
        SUCCESS
    );
    let deq = host.transfer | 1;
    assert_eq!(host.one(trb(SET_DEQUEUE, deq, 0, endpoint)).code(), SUCCESS);

    let (events, bytes) = host.get(slot, descriptor(1, 0, 18), 18);

    assert_eq!(events.len(), 1, "and the next transfer runs");
    assert_eq!(events[0].code(), SUCCESS);
    assert_eq!(bytes[0], 18);
}

/// A transfer going the other way, which is how a driver chooses a configuration and
/// how it lights a keyboard's lamps.
#[test]
fn a_control_transfer_carries_bytes_out_to_the_device() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);

    // Set Configuration, which has no data stage at all.
    let chosen = host.transfer(
        slot,
        CONTROL,
        &[
            trb(
                SETUP,
                u64::from_le_bytes([0, 9, 1, 0, 0, 0, 0, 0]),
                8,
                IMMEDIATE,
            ),
            trb(STATUS, 0, 0, TO_HOST | IOC),
        ],
    );
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].code(), SUCCESS);

    // And then read it back, which is what says the device took it.
    let (_, bytes) = host.get(slot, [0x80, 8, 0, 0, 0, 0, 1, 0], 1);
    assert_eq!(bytes[0], 1, "the configuration it was put in");
}

// ------------------------------------------------------- the endpoint that has to wait

/// An interrupt endpoint with nothing to report is the ordinary case, and the transfer
/// asking for a report has to stay outstanding rather than finish empty.
#[test]
fn an_endpoint_with_nothing_to_report_leaves_the_transfer_outstanding() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    host.configure_reports(slot);

    let events = host.transfer(slot, REPORTS, &[trb(NORMAL, BUFFER, 8, IOC | SHORT)]);

    assert!(events.is_empty(), "nothing has been typed");
}

/// And when something is typed, the transfer that was waiting finishes, out of a poll
/// rather than out of an access.
#[test]
fn a_key_typed_after_the_transfer_was_queued_finishes_it() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    host.configure_reports(slot);
    host.transfer(slot, REPORTS, &[trb(NORMAL, BUFFER, 8, IOC | SHORT)]);

    // The letter a, held with no modifier, and then released.
    host.keys.typed(0, 0x04);
    host.xhci.poll();
    let events = host.events();
    let report = host.peek(BUFFER, 8);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind(), TRANSFER_EVENT);
    assert_eq!(events[0].code(), SUCCESS);
    assert_eq!(events[0].control >> 16 & 0x1f, REPORTS as u32);
    assert_eq!(
        report,
        [0, 0, 0x04, 0, 0, 0, 0, 0],
        "the key that went down"
    );
}

/// A poll with nothing typed changes nothing, which is what stops a transfer finishing
/// with a report of nothing every time round.
#[test]
fn polling_with_nothing_typed_finishes_nothing() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    host.configure_reports(slot);
    host.transfer(slot, REPORTS, &[trb(NORMAL, BUFFER, 8, IOC | SHORT)]);

    for _ in 0..8 {
        host.xhci.poll();
    }

    assert!(host.events().is_empty());
}

/// A transfer whose buffer is not one run of memory is as many blocks as it has runs,
/// chained together, and what arrives is spread over them in order.
#[test]
fn a_report_is_spread_over_the_blocks_a_transfer_was_chained_from() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(1);
    host.configure_reports(slot);
    host.transfer(
        slot,
        REPORTS,
        &[
            trb(NORMAL, BUFFER, 3, CHAIN),
            trb(NORMAL, BUFFER + 0x100, 5, IOC | SHORT),
        ],
    );

    host.keys.holding(2, &[4, 5, 6, 7, 8, 9]);
    host.xhci.poll();
    let events = host.events();

    assert_eq!(events.len(), 1, "one transfer, however many blocks");
    assert_eq!(events[0].code(), SUCCESS);
    assert_eq!(host.peek(BUFFER, 3), [2, 0, 4], "the first three bytes");
    assert_eq!(
        host.peek(BUFFER + 0x100, 5),
        [5, 6, 7, 8, 9],
        "and the rest"
    );
}

/// The mouse reports four bytes and the transfer asks for eight, so every report it
/// sends is a short one.
#[test]
fn a_report_shorter_than_the_buffer_says_how_much_of_it_was_filled() {
    let mut host = host();
    host.start();
    host.events();
    let slot = host.attach(2);
    host.configure_reports(slot);
    host.transfer(slot, REPORTS, &[trb(NORMAL, BUFFER, 8, IOC | SHORT)]);

    host.pointer.moved(1, 5, -5, 0);
    host.xhci.poll();
    let events = host.events();
    let report = host.peek(BUFFER, 4);

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].code(), SHORT_PACKET);
    assert_eq!(events[0].left(), 4, "four of the eight bytes went unused");
    assert_eq!(report, [1, 5, 0xfb, 0], "a button, and how far it moved");
}

// ------------------------------------------------------------------------ the interrupt

/// A wire is a level: it goes up when there is an interrupt and stays up until software
/// says it has dealt with what raised it.
#[test]
fn an_event_raises_the_wire_and_taking_the_interrupt_lowers_it() {
    let mut host = host();
    host.start();
    assert!(!host.xhci.asserted(false).pin, "nothing has happened yet");

    host.command(trb(NOOP_COMMAND, 0, 0, 0));
    assert!(host.xhci.asserted(false).pin, "a command answered");

    // Reading the events is not what lowers it: the two bits software writes are.
    host.write_op(0x04, STS_EINT);
    assert!(
        host.xhci.asserted(false).pin,
        "the interrupter is still pending"
    );
    host.write_interrupter(0x00, IMAN_ENABLE | IMAN_PENDING);
    assert!(!host.xhci.asserted(false).pin, "and now it is not");
}

/// A message is not: nothing software does clears the pending bit, because software
/// never sees the message go, so the controller clears it as it sends.
#[test]
fn a_message_is_what_clears_the_pending_bit_when_one_carries_the_interrupt() {
    let mut host = host();
    host.start();
    host.xhci.asserted(true);

    host.command(trb(NOOP_COMMAND, 0, 0, 0));
    assert_ne!(
        host.interrupter(0x00) & IMAN_PENDING,
        0,
        "before it is sent"
    );
    let sent = host.xhci.asserted(true);

    assert_eq!(sent.messages, 1);
    assert!(
        !sent.pin,
        "and a function sending messages does not pull its pin"
    );
    assert_eq!(
        host.interrupter(0x00) & IMAN_PENDING,
        0,
        "which is what leaves the next event able to raise one"
    );

    host.events();
    host.command(trb(NOOP_COMMAND, 0, 0, 0));
    assert_eq!(host.xhci.asserted(true).messages, 1, "and it does");
}

/// A message is an edge where a wire is a level: one goes out each time the interrupt
/// starts, and not again until it has stopped.
#[test]
fn a_message_goes_out_once_per_interrupt_rather_than_once_per_event() {
    let mut host = host();
    host.start();
    host.xhci.asserted(false);
    host.events();

    host.command(trb(NOOP_COMMAND, 0, 0, 0));
    let first = host.xhci.asserted(false);
    host.command(trb(NOOP_COMMAND, 0, 0, 0));
    let second = host.xhci.asserted(false);

    assert_eq!(first.messages, 1, "the interrupt started");
    assert_eq!(second.messages, 0, "and has not stopped since");
}

/// An interrupter that has not been enabled reports its events and raises nothing,
/// which is what lets a driver set the rings up before it is ready for interrupts.
#[test]
fn an_interrupter_that_is_not_enabled_raises_nothing() {
    let mut host = host();
    host.start();
    host.write_interrupter(0x00, IMAN_PENDING);
    host.xhci.asserted(false);
    host.events();

    let answer = host.one(trb(NOOP_COMMAND, 0, 0, 0));

    assert_eq!(answer.code(), SUCCESS, "the command still ran");
    assert_eq!(
        host.xhci.asserted(false),
        Asserted::default(),
        "and nothing was raised about it"
    );
}

/// And neither does one whose controller has interrupts turned off altogether.
#[test]
fn a_controller_with_interrupts_disabled_raises_nothing() {
    let mut host = host();
    host.start();
    host.write_op(0x00, CMD_RUN);
    host.xhci.asserted(false);
    host.events();

    host.one(trb(NOOP_COMMAND, 0, 0, 0));

    assert_eq!(host.xhci.asserted(false), Asserted::default());
}

// ------------------------------------------------------------------------ the event ring

/// The cycle bit is the whole of how a driver tells an event it has not seen from one
/// it has: the ring wraps and the bit turns over.
#[test]
fn the_event_ring_wraps_and_turns_its_cycle_bit_over() {
    let mut host = host();
    host.start();
    host.events();

    let cycle = host.event_cycle;
    for _ in 0..EVENT_RING + 2 {
        assert_eq!(host.one(trb(NOOP_COMMAND, 0, 0, 0)).code(), SUCCESS);
    }

    assert_ne!(host.event_cycle, cycle, "it went round");
    assert!(host.event < EVENTS + EVENT_RING * 16, "and stayed inside");
}

/// A ring with nowhere left to put an event is a host controller error rather than an
/// event quietly dropped, which would leave a driver waiting for a transfer forever.
#[test]
fn an_event_ring_nobody_is_reading_reports_an_error() {
    let mut host = host();
    host.start();
    host.events();

    // Never read them, so the dequeue pointer stays where the driver left it.
    for _ in 0..EVENT_RING + 4 {
        host.put_trb(host.command, trb(NOOP_COMMAND, 0, 0, 1));
        host.command += 16;
        host.doorbell(0, 0);
    }

    assert_ne!(host.op(0x04) & (1 << 12), 0, "a host controller error");
}

// ------------------------------------------------------------------------------ the doorbell

/// A doorbell is not storage: what it does is done when it is written.
#[test]
fn a_doorbell_reads_back_as_nothing() {
    let mut host = host();
    host.start();
    host.doorbell(0, 0);
    let at = host.doorbells;

    assert_eq!(host.read(at), 0);
}

/// And ringing one before the controller is running does nothing at all, which is what
/// stops a driver's leftovers running when it starts.
#[test]
fn a_doorbell_rung_before_the_controller_runs_does_nothing() {
    let mut host = host();
    host.write_op(0x38, 32);
    host.write64(host.operational + 0x30, DCBAA);
    host.write64(host.operational + 0x18, COMMANDS | 1);
    host.put_trb(COMMANDS, trb(NOOP_COMMAND, 0, 0, 1));

    host.doorbell(0, 0);
    host.start();
    host.events();

    // The command is still there to run, which is what says nothing ran it.
    assert_eq!(host.op(0x04) & STS_HALTED, 0);
}

impl Host {
    /// Add the endpoint the reports come in on, which a driver does when it chooses a
    /// configuration.
    fn configure_reports(&mut self, slot: usize) {
        self.poke(INPUT, &0u32.to_le_bytes());
        self.poke(INPUT + 4, &(1u32 | (1 << REPORTS)).to_le_bytes());
        self.poke(
            INPUT + (REPORTS as u64 + 1) * 32 + 4,
            &((7u32 << 3) | (8 << 16)).to_le_bytes(),
        );
        self.poke(
            INPUT + (REPORTS as u64 + 1) * 32 + 8,
            &((TRANSFERS + 0x1000) | 1).to_le_bytes(),
        );
        let answer = self.one(trb(CONFIGURE_ENDPOINT, INPUT, 0, (slot as u32) << 24));
        assert_eq!(answer.code(), SUCCESS, "configuring the endpoint");
        self.transfer = TRANSFERS + 0x1000;
    }
}

/// The controller is on the bus and so is what is plugged into it, and neither is
/// anything a `Device` on the bus reaches: this is what says the whole thing still
/// crosses a thread boundary, which is what the window in the phase after this needs.
#[test]
fn a_controller_and_its_devices_can_be_handed_to_another_thread() {
    fn assert_send<T: Send>() {}
    assert_send::<Xhci>();
    assert_send::<Box<dyn usb::Device>>();
    assert_send::<Keys>();
    assert_send::<Pointer>();
    let _ = pci::PINS;
}
