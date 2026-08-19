use crate::common::*;
use rysk::{device::Device, trap::Exception};

// ---------------------------------------------------------- address decode

const BASE: u64 = 0x1000_0000;

/// The offset of the register that hands back whatever was last written, so a test can
/// see that a store arrived rather than assuming it.
const WITNESS: i32 = 0x7f0;

/// A stand-in for a real device. Every register reports the offset and width the bus
/// handed it, which is what a test of address decode wants to see, except `WITNESS`.
#[derive(Debug, Default)]
struct Fake {
    written: u64,
}

impl Device for Fake {
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        Ok(match offset {
            _ if offset == WITNESS as u64 => self.written,
            _ => offset << 8 | size,
        })
    }

    fn store(&mut self, _offset: u64, _size: u64, value: u64) -> Result<(), Exception> {
        self.written = value;
        Ok(())
    }
}

fn fake() -> Box<dyn Device> {
    Box::new(Fake::default())
}

#[test]
fn a_device_is_read_at_an_offset_from_its_own_base() {
    let cpu = prog(&[lw(A0, T0, 0), lw(A1, T0, 8)])
        .reg(T0, BASE + 0x40)
        .device(BASE, 0x1000, fake())
        .run();
    assert_eq!(cpu.reg(A0), 0x40 << 8 | 32, "offset 0x40, 32 bits");
    assert_eq!(cpu.reg(A1), 0x48 << 8 | 32, "and the next word along");
}

#[test]
fn a_store_reaches_the_device() {
    let cpu = prog(&[sw(T1, T0, 4), lw(A0, T0, WITNESS)])
        .reg(T0, BASE)
        .reg(T1, 0xabcd)
        .device(BASE, 0x1000, fake())
        .run();
    assert_eq!(
        cpu.reg(A0),
        0xabcd,
        "the device kept what was written to it"
    );
}

#[test]
fn an_address_no_device_claims_still_faults() {
    prog(&[lw(A0, T0, 0)])
        .reg(T0, BASE + 0x2000)
        .device(BASE, 0x1000, fake())
        .expect(Exception::LoadAccessFault(BASE + 0x2000));
    prog(&[sw(T1, T0, 0)])
        .reg(T0, BASE - 8)
        .device(BASE, 0x1000, fake())
        .expect(Exception::StoreAmoAccessFault(BASE - 8));
}

#[test]
fn an_access_running_off_the_end_of_a_device_is_not_its_to_answer() {
    // The last four bytes are inside; a doubleword starting there is not.
    prog(&[ld(A0, T0, 0)])
        .reg(T0, BASE + 0x1000 - 4)
        .device(BASE, 0x1000, fake())
        .expect(Exception::LoadAccessFault(BASE + 0x1000 - 4));
    let cpu = prog(&[lw(A0, T0, 0)])
        .reg(T0, BASE + 0x1000 - 4)
        .device(BASE, 0x1000, fake())
        .run();
    assert_eq!(cpu.reg(A0), 0xffc << 8 | 32, "but a word is");
}

#[test]
fn several_devices_decode_to_the_right_one() {
    let cpu = prog(&[lw(A0, T0, 0), lw(A1, T1, 0), lw(A2, T2, 0)])
        .reg(T0, 0x0200_0000)
        .reg(T1, 0x0c00_0000)
        .reg(T2, BASE)
        .device(BASE, 0x100, fake())
        .device(0x0200_0000, 0x10000, fake())
        .device(0x0c00_0000, 0x400000, fake())
        .run();
    for value in [cpu.reg(A0), cpu.reg(A1), cpu.reg(A2)] {
        assert_eq!(value, 32, "each decoded to offset zero of its own device");
    }
}

#[test]
#[should_panic(expected = "a device already answers")]
fn two_devices_may_not_claim_the_same_address() {
    prog(&[nop()])
        .device(BASE, 0x1000, fake())
        .device(BASE + 0x800, 0x1000, fake())
        .run();
}
