//! The root complex: what enumeration finds, how a window is sized and placed, and
//! where an access in one lands.

use crate::common::*;
use rysk::{
    device::Line,
    pci::{self, Bar, Function, Header, HostBridge, Root},
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
    let root = Root::new(std::array::from_fn(|_| Line::default()));
    root.plug(0, Box::new(HostBridge));
    root.plug(1, Box::new(Probe::default()));
    prog(code)
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
        .reg(T3, u64::MAX)
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
