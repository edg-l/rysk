//! The root complex: what enumeration finds, how a window is sized and placed, and
//! where an access in one lands.

use crate::common::*;
use rysk::{
    device::{Device, Line, Msi},
    pci::{self, Bar, Function, Header, HostBridge, MsiX, Root},
    trap::Exception,
};

/// Config space of one device, and the register offsets a type 0 header defines.
fn config(device: usize) -> u64 {
    pci::ECAM + ((device as u64) << 15)
}

const COMMAND: i32 = 0x04;
const BAR0: i32 = 0x10;
const BAR1: i32 = 0x14;
const BAR2: i32 = 0x18;
/// `command`'s memory space enable.
const MEMORY: u64 = 1 << 1;

/// A function that answers with where it was reached rather than with anything: the
/// register it landed in and the offset into it, which is what a routing test wants to
/// know. The first doubleword of the first window is storage instead, so a test can
/// also see a write arrive.
#[derive(Debug, Default)]
struct Probe {
    cell: u64,
}

const PROBE_BAR0: u64 = 0x1000;
const PROBE_BAR1: u64 = 0x2_0000;

impl Function for Probe {
    fn header(&self) -> Header {
        Header {
            vendor: 0xabcd,
            device: 0x1234,
            // A device with no class anyone drives, revision one.
            class: 0x00ff_0001,
            bars: [
                Bar::Memory {
                    size: PROBE_BAR0,
                    prefetchable: false,
                    wide: false,
                },
                Bar::Memory {
                    size: PROBE_BAR1,
                    prefetchable: true,
                    wide: true,
                },
                Bar::High,
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            pin: 1,
            ..Header::default()
        }
    }

    fn load(&mut self, bar: usize, offset: u64, _size: u64) -> Result<u64, Exception> {
        Ok(match (bar, offset) {
            (0, 0) => self.cell,
            _ => ((bar as u64) << 32) | offset,
        })
    }

    fn store(&mut self, bar: usize, offset: u64, _size: u64, value: u64) -> Result<(), Exception> {
        if (bar, offset) == (0, 0) {
            self.cell = value;
        }
        Ok(())
    }
}

/// A machine with a root complex holding the host bridge at device zero and a probe at
/// device one. `t0` is device zero's config space, `t1` device one's, `t2` the 32-bit
/// window and `t3` all ones.
fn pcie(code: &[u32]) -> Program {
    posted(code, Msi::default()).0
}

/// The same, with somewhere for a message to go and the root complex handed back so a
/// test can make a function raise one.
fn posted(code: &[u32], msi: Msi) -> (Program, Root) {
    let root = Root::new(std::array::from_fn(|_| Line::default()), msi);
    root.plug(0, Box::new(HostBridge));
    root.plug(1, Box::new(Probe::default()));
    root.plug(MESSENGER, Box::new(Messenger));
    let program = prog(code)
        .device(pci::ECAM, pci::ECAM_SIZE, Box::new(root.config()))
        .device(pci::MMIO, pci::MMIO_SIZE, Box::new(root.window(pci::MMIO)))
        .device(
            pci::MMIO64,
            pci::MMIO64_SIZE,
            Box::new(root.window(pci::MMIO64)),
        )
        .reg(T0, config(0))
        .reg(T1, config(1))
        .reg(T2, pci::MMIO)
        .reg(T3, u64::MAX);
    (program, root)
}

/// A function that interrupts by message rather than by wire, with room in its one
/// window for a table of two vectors and the array that goes with them.
const MESSENGER: usize = 3;
const VECTORS: usize = 2;
const TABLE: u64 = 0;
const PENDING: u64 = 0x800;

#[derive(Debug)]
struct Messenger;

impl Function for Messenger {
    fn header(&self) -> Header {
        Header {
            vendor: 0x1af4,
            device: 0x1052,
            class: 0x00ff_0000,
            bars: [
                Bar::Memory {
                    size: 0x1000,
                    prefetchable: false,
                    wide: false,
                },
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            msix: Some(MsiX {
                vectors: VECTORS,
                table: (0, TABLE),
                pending: (0, PENDING),
            }),
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

#[test]
fn enumeration_finds_what_is_plugged_in_and_nothing_where_nothing_is() {
    let machine = pcie(&[
        lwu(A0, T0, 0),
        lwu(A1, T1, 0),
        // device two, which is empty, and function one of device zero, which does not
        // exist because nothing here is multi-function
        lwu(A2, T4, 0),
        lwu(A3, T5, 0),
    ])
    .reg(T4, config(2))
    .reg(T5, config(0) + (1 << 12))
    .run();

    assert_eq!(
        machine.reg(A0),
        0x0008_1b36,
        "the host bridge is at device zero, where enumeration looks first"
    );
    assert_eq!(machine.reg(A1), 0x1234_abcd, "and the probe at device one");
    assert_eq!(
        machine.reg(A2),
        0xffff_ffff,
        "an empty slot reads as a vendor no device may have, which is how the scan ends"
    );
    assert_eq!(
        machine.reg(A3),
        0xffff_ffff,
        "and so does a function a single-function device does not have"
    );
}

#[test]
fn a_window_reports_its_size_through_the_bits_a_base_cannot_set() {
    let machine = pcie(&[
        // write all ones and read back: what stayed zero is the size
        sw(T3, T1, BAR0),
        lwu(A0, T1, BAR0),
        sw(T3, T1, BAR1),
        lwu(A1, T1, BAR1),
        sw(T3, T1, BAR2),
        lwu(A2, T1, BAR2),
        // a register the function does not have stays zero however it is written
        lwu(A3, T1, 0x1c),
    ])
    .run();

    assert_eq!(
        machine.reg(A0),
        !(PROBE_BAR0 - 1) as u32 as u64,
        "four kibibytes, in memory space, neither wide nor prefetchable"
    );
    assert_eq!(
        machine.reg(A1),
        (!(PROBE_BAR1 - 1) as u32 as u64) | 0b1100,
        "a hundred and twenty-eight kibibytes, wide and prefetchable"
    );
    assert_eq!(
        machine.reg(A2),
        0xffff_ffff,
        "and the register after a wide one is its high half"
    );
    assert_eq!(
        machine.reg(A3),
        0,
        "a register that is not there reads zero"
    );
}

#[test]
fn a_window_answers_only_where_it_was_put_and_only_once_it_is_turned_on() {
    let machine = pcie(&[
        // place the first window at the bottom of the 32-bit range
        sw(T2, T1, BAR0),
        // reading it before the command register says so reaches nothing
        lwu(A0, T2, 0x40),
        addi(T4, ZERO, MEMORY as i32),
        sw(T4, T1, COMMAND),
        lwu(A1, T2, 0x40),
        // and the base reads back where it was put
        lwu(A2, T1, BAR0),
    ])
    .run();

    assert_eq!(
        machine.reg(A0),
        0xffff_ffff,
        "a window that is not enabled answers as nothing does"
    );
    assert_eq!(
        machine.reg(A1),
        0x40,
        "and once it is, the function is told which of its windows and where in it"
    );
    assert_eq!(machine.reg(A2), pci::MMIO, "the base reads back");
}

#[test]
fn a_wide_window_takes_two_registers_and_can_be_put_above_four_gibibytes() {
    let machine = pcie(&[
        // the low half of the address goes in the register and the high half in the
        // one after it
        sw(T4, T1, BAR1),
        sw(T5, T1, BAR2),
        addi(T6, ZERO, MEMORY as i32),
        sw(T6, T1, COMMAND),
        ld(A0, T4, 0x10),
        lwu(A1, T1, BAR1),
        lwu(A2, T1, BAR2),
    ])
    .reg(T4, pci::MMIO64)
    .reg(T5, pci::MMIO64 >> 32)
    .run();

    assert_eq!(
        machine.reg(A0),
        (1 << 32) | 0x10,
        "an access sixteen bytes into the second window reached it as its own"
    );
    assert_eq!(
        machine.reg(A1),
        (pci::MMIO64 as u32 as u64) | 0b1100,
        "the low half of the address reads back, wide and prefetchable"
    );
    assert_eq!(
        machine.reg(A2),
        pci::MMIO64 >> 32,
        "and the high half is the register after it"
    );
}

#[test]
fn a_byte_of_config_space_is_the_byte_of_the_word_its_address_names() {
    let machine = pcie(&[
        lbu(A0, T1, 0),
        lbu(A1, T1, 1),
        lhu(A2, T1, 2),
        // the class register, whose top byte is the class and whose bottom is the
        // revision
        lbu(A3, T1, 0xb),
        lbu(A4, T1, 0x8),
    ])
    .run();

    assert_eq!(machine.reg(A0), 0xcd, "the low byte of the vendor");
    assert_eq!(machine.reg(A1), 0xab, "and its high byte");
    assert_eq!(
        machine.reg(A2),
        0x1234,
        "the device id is the halfword above"
    );
    assert_eq!(machine.reg(A3), 0x00, "the class");
    assert_eq!(machine.reg(A4), 0x01, "and the revision at the bottom");
}

#[test]
fn a_write_through_a_window_reaches_the_function() {
    let machine = pcie(&[
        sw(T2, T1, BAR0),
        addi(T4, ZERO, MEMORY as i32),
        sw(T4, T1, COMMAND),
        sd(T5, T2, 0),
        ld(A0, T2, 0),
    ])
    .reg(T5, 0x0123_4567_89ab_cdef)
    .run();

    assert_eq!(
        machine.reg(A0),
        0x0123_4567_89ab_cdef,
        "it round-tripped through the window the base address register named"
    );
}

#[test]
fn a_pin_is_swizzled_so_that_four_devices_do_not_share_one_wire() {
    // Every device's first pin lands on a different wire, and the four pins of one
    // device land on all four. The device tree publishes exactly this mapping, and the
    // test that reads the tree holds it against this function.
    let first: Vec<usize> = (0..4).map(|device| pci::swizzle(device, 1)).collect();
    assert_eq!(
        first,
        [0, 1, 2, 3],
        "one pin each, spread over the four wires"
    );
    let pins: Vec<usize> = (1..=4).map(|pin| pci::swizzle(1, pin as u8)).collect();
    assert_eq!(
        pins,
        [1, 2, 3, 0],
        "and one device's four pins over all of them"
    );
}

// ---------------------------------------------------------------- messages

/// The capability's registers, from the specification rather than from the model.
/// PCI Local Bus Specification 3.0, 6.8.2.
const STATUS: i32 = 0x06;
const CAPABILITIES: i32 = 0x34;
const MSIX_CONTROL: i32 = 0x40;
const MSIX_TABLE: i32 = 0x44;
const MSIX_PBA: i32 = 0x48;
const MSIX_ENABLE: u64 = 1 << 31;
const MSIX_FUNCTION_MASK: u64 = 1 << 30;
/// How big one entry is, which is four words.
const VECTOR_SIZE: u64 = 16;

/// Where a test points a vector, and what it has it say when it gets there.
const SINK: u64 = 0x1234_5000;
const IDENTITY: u32 = 42;

/// Every message that reached the sink, as the address it was posted to and what was
/// posted there.
type Posted = std::sync::Arc<std::sync::Mutex<Vec<(u64, u32)>>>;

/// A place to post to, and the messages that arrived there.
fn recorder() -> (Msi, Posted) {
    let posted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = posted.clone();
    (
        Msi::new(move |addr, identity| kept.lock().unwrap().push((addr, identity))),
        posted,
    )
}

#[test]
fn a_function_that_sends_messages_says_so_in_its_capability_list() {
    let machine = pcie(&[
        lhu(A0, T4, STATUS),
        lbu(A1, T4, CAPABILITIES),
        lwu(A2, T4, MSIX_CONTROL),
        lwu(A3, T4, MSIX_TABLE),
        lwu(A4, T4, MSIX_PBA),
        lhu(A5, T1, STATUS),
    ])
    .reg(T4, config(MESSENGER))
    .run();
    assert_eq!(machine.reg(A0) & (1 << 4), 1 << 4, "there is a list");
    assert_eq!(
        machine.reg(A1),
        MSIX_CONTROL as u64,
        "and the pointer names where it starts"
    );
    assert_eq!(
        machine.reg(A2),
        0x11 | ((VECTORS as u64 - 1) << 16),
        "the capability is msi-x, ends the list, and has two vectors"
    );
    assert_eq!(machine.reg(A3), TABLE, "the table is in window zero");
    assert_eq!(machine.reg(A4), PENDING, "and so is the array");
    assert_eq!(
        machine.reg(A5) & (1 << 4),
        0,
        "a function with no capabilities says it has none"
    );
}

/// Place the messenger's window at `WINDOW`, turn its memory space on, and point
/// vector zero at the sink. `t5` is the window and `t6` the messenger's config space.
const WINDOW: u64 = pci::MMIO + 0x8000;

fn armed() -> Vec<u32> {
    vec![
        sw(S0, T6, BAR0),
        sw(S1, T6, COMMAND),
        // Where to write, and what to write there.
        sw(A5, T5, TABLE as i32),
        sw(ZERO, T5, TABLE as i32 + 4),
        sw(A4, T5, TABLE as i32 + 8),
    ]
}

fn messenger(code: &[u32], msi: Msi) -> (Program, Root) {
    let (program, root) = posted(code, msi);
    let program = program
        .reg(T5, WINDOW)
        .reg(T6, config(MESSENGER))
        .reg(S0, WINDOW)
        .reg(S1, MEMORY)
        .reg(A4, IDENTITY as u64)
        .reg(A5, SINK);
    (program, root)
}

#[test]
fn a_vector_is_posted_where_its_entry_points() {
    let (msi, posted) = recorder();
    let mut code = armed();
    code.extend([
        // Unmask the vector, then the function.
        sw(ZERO, T5, TABLE as i32 + 12),
        sw(A3, T6, MSIX_CONTROL),
    ]);
    let (program, root) = messenger(&code, msi);
    let machine = program.reg(A3, MSIX_ENABLE).run();
    let _ = machine;

    root.message(MESSENGER, 0);
    assert_eq!(
        *posted.lock().unwrap(),
        [(SINK, IDENTITY)],
        "the address and the data the entry was given"
    );
}

#[test]
fn a_function_that_has_not_been_turned_on_sends_nothing() {
    let (msi, posted) = recorder();
    let mut code = armed();
    code.push(sw(ZERO, T5, TABLE as i32 + 12));
    let (program, root) = messenger(&code, msi);
    program.run();

    root.message(MESSENGER, 0);
    assert!(
        posted.lock().unwrap().is_empty(),
        "a message needs the capability enabled, whatever the entry says"
    );
}

#[test]
fn a_masked_vector_waits_rather_than_being_dropped() {
    let (msi, posted) = recorder();
    let mut code = armed();
    // Enabled, but with every vector masked at the function.
    code.push(sw(A3, T6, MSIX_CONTROL));
    code.push(sw(ZERO, T5, TABLE as i32 + 12));
    let (program, root) = messenger(&code, msi);
    let machine = program.reg(A3, MSIX_ENABLE | MSIX_FUNCTION_MASK).run();
    let _ = machine;

    root.message(MESSENGER, 0);
    assert!(posted.lock().unwrap().is_empty(), "masked, so nothing goes");
}

#[test]
fn a_vector_raised_while_masked_waits_and_goes_when_it_is_unmasked() {
    let (msi, posted) = recorder();
    let root = Root::new(std::array::from_fn(|_| Line::default()), msi);
    root.plug(MESSENGER, Box::new(Messenger));
    let mut config = root.config();
    let mut window = root.window(pci::MMIO);
    // Config space and the window are reached where the bus reaches them, which is at
    // an offset from each one's own base.
    let register = |reg: i32| ((MESSENGER as u64) << 15) + reg as u64;
    let entry = WINDOW - pci::MMIO + TABLE + VECTOR_SIZE;
    let array = WINDOW - pci::MMIO + PENDING;

    // Place the window, turn its memory space on, point vector one at the sink, and
    // enable the capability while leaving the vector itself masked.
    config.store(register(BAR0), 32, WINDOW).unwrap();
    config.store(register(COMMAND), 32, MEMORY).unwrap();
    window.store(entry, 32, SINK).unwrap();
    window.store(entry + 8, 32, IDENTITY as u64).unwrap();
    config
        .store(register(MSIX_CONTROL), 32, MSIX_ENABLE)
        .unwrap();

    root.message(MESSENGER, 1);
    assert!(
        posted.lock().unwrap().is_empty(),
        "a vector comes out of reset masked"
    );
    assert_eq!(
        window.load(array, 32).unwrap(),
        1 << 1,
        "and the array says which one is waiting"
    );

    window.store(entry + 12, 32, 0).unwrap();
    assert_eq!(
        *posted.lock().unwrap(),
        [(SINK, IDENTITY)],
        "unmasking sends what was waiting"
    );
    assert_eq!(
        window.load(array, 32).unwrap(),
        0,
        "and it is not waiting now"
    );
}

/// A function with both capabilities, so that a test can walk from one to the next.
/// Nothing else about it matters: it has one window because a vector table has to live
/// somewhere, and it never answers an access to it.
const INTEGRATED: usize = 4;

#[derive(Debug)]
struct Integrated;

impl Function for Integrated {
    fn header(&self) -> Header {
        Header {
            vendor: 0x1af4,
            device: 0x1053,
            class: 0x00ff_0000,
            bars: [
                Bar::Memory {
                    size: 0x1000,
                    prefetchable: false,
                    wide: false,
                },
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
                Bar::None,
            ],
            msix: Some(MsiX {
                vectors: VECTORS,
                table: (0, TABLE),
                pending: (0, PENDING),
            }),
            express: true,
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

/// Config space of a function with both capabilities. `t0` is where the list starts.
fn integrated(code: &[u32]) -> Program {
    let root = Root::new(std::array::from_fn(|_| Line::default()), Msi::default());
    root.plug(0, Box::new(HostBridge));
    root.plug(1, Box::new(Probe::default()));
    root.plug(INTEGRATED, Box::new(Integrated));
    prog(code)
        .device(pci::ECAM, pci::ECAM_SIZE, Box::new(root.config()))
        .reg(T0, config(INTEGRATED))
        .reg(T1, config(1))
}

/// Where the capability list puts each of them: message-signalled interrupts first,
/// twelve bytes of it, then the Express capability.
const FIRST: i32 = MSIX_CONTROL;
const SECOND: i32 = FIRST + 12;
const EXPRESS_FLAGS: i32 = SECOND;
const EXPRESS_DEVCAP: i32 = SECOND + 0x04;
const EXPRESS_DEVCTL: i32 = SECOND + 0x08;
const EXPRESS_LNKCAP: i32 = SECOND + 0x0c;
const EXPRESS_DEVCTL2: i32 = SECOND + 0x28;

#[test]
fn the_capability_list_chains_one_capability_to_the_next() {
    let machine = integrated(&[
        lwu(A0, T0, CAPABILITIES),
        lwu(A1, T0, FIRST),
        lwu(A2, T0, SECOND),
        // And a function with no capabilities at all points at nothing.
        lwu(A3, T1, CAPABILITIES),
    ])
    .run();

    assert_eq!(
        machine.reg(A0),
        FIRST as u64,
        "the list starts after the header"
    );
    assert_eq!(
        machine.reg(A1) & 0xffff,
        0x11 | ((SECOND as u64) << 8),
        "message-signalled interrupts, and the Express capability after it"
    );
    assert_eq!(
        machine.reg(A2) & 0xffff,
        0x10,
        "Express, and nothing after it"
    );
    assert_eq!(machine.reg(A3), 0, "a function with no capabilities");
}

#[test]
fn status_says_there_is_a_list_only_when_there_is_one() {
    let machine = integrated(&[lwu(A0, T0, COMMAND), lwu(A1, T1, COMMAND)]).run();

    assert_ne!(machine.reg(A0) >> 16 & (1 << 4), 0);
    assert_eq!(
        machine.reg(A1) >> 16 & (1 << 4),
        0,
        "or software follows a pointer into nothing"
    );
}

#[test]
fn an_express_function_says_which_kind_of_express_device_it_is() {
    let machine = integrated(&[lwu(A0, T0, EXPRESS_FLAGS)]).run();

    assert_eq!(
        machine.reg(A0) >> 16,
        0x0092,
        "version two, and a root complex integrated endpoint"
    );
}

#[test]
fn an_express_function_offers_no_reset_and_no_link() {
    let machine = integrated(&[lwu(A0, T0, EXPRESS_DEVCAP), lwu(A1, T0, EXPRESS_LNKCAP)]).run();

    assert_eq!(
        machine.reg(A0),
        0,
        "128-byte payloads, and no function level reset, which is not implemented"
    );
    assert_eq!(
        machine.reg(A1),
        0,
        "an endpoint on the root complex's own bus has no link below it"
    );
}

#[test]
fn device_control_is_storage_but_for_the_reset_that_is_not_offered() {
    let machine = integrated(&[
        lwu(A0, T0, EXPRESS_DEVCTL),
        sw(T4, T0, EXPRESS_DEVCTL),
        lwu(A1, T0, EXPRESS_DEVCTL),
        sw(T4, T0, EXPRESS_DEVCTL2),
        lwu(A2, T0, EXPRESS_DEVCTL2),
        lwu(A3, T0, EXPRESS_DEVCTL),
    ])
    .reg(T4, u64::MAX)
    .run();

    assert_eq!(
        machine.reg(A0),
        0x2000,
        "reads of up to 512 bytes, which is what it comes out of reset asking for"
    );
    assert_eq!(
        machine.reg(A1) & 0xffff,
        0x7fff,
        "every bit but the one that would start a reset"
    );
    assert_eq!(machine.reg(A1) >> 16, 0, "device status detects nothing");
    assert_eq!(
        machine.reg(A2) & 0xffff,
        0xffff,
        "the second one is storage"
    );
    assert_eq!(
        machine.reg(A3) & 0xffff,
        0x7fff,
        "and writing it leaves the first alone"
    );
}
