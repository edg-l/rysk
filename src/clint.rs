//! The core-local interruptor: a monotonic counter, a deadline per hart compared
//! against it, and a bit per hart that software sets to interrupt one. It is the first
//! thing on the machine that can raise an interrupt without an instruction asking for
//! one, and the only way one hart interrupts another.
//!
//! The offsets are the SiFive CLINT's, which the ACLINT specification keeps for
//! compatibility: a software-interrupt device at zero and a timer at `0x4000`.
//!
//! RISC-V Advanced Core Local Interruptor Specification, 1.1 and chapters 2 and 3.

use std::time::Instant;

use crate::{
    csr::{MSIP, MTIP},
    device::Device,
    trap::Exception,
};

pub const BASE: u64 = 0x0200_0000;
pub const SIZE: u64 = 0x1_0000;

/// Ticks of `mtime` per second, the `timebase-frequency` a device tree would
/// advertise. Ten megahertz is what the QEMU `virt` machine reports, so a guest built
/// for that machine keeps its arithmetic.
pub const FREQUENCY: u64 = 10_000_000;

/// A software-interrupt bit per hart, one word each, and a deadline per hart, one
/// doubleword each. A machine with more harts has more of both, at the same stride.
/// RISC-V Advanced Core Local Interruptor Specification, 2.3 and 3.2.
const MSIP0: u64 = 0x0000;
const MSIP_END: u64 = 0x4000;
const MTIMECMP0: u64 = 0x4000;
/// Where the counter is, which the `time` CSR has to agree with: they are the same
/// counter seen two ways. It is also the end of the deadlines, since they run up to it.
pub const MTIME: u64 = 0xbff8;

/// Which register an access names. Each has one width as well as one address, so an
/// access of the wrong width at the right address is not that register, and there is
/// no other it could be.
#[derive(Debug, Clone, Copy)]
enum Register {
    Msip(usize),
    Mtimecmp(usize),
    Mtime,
}

#[derive(Debug)]
pub struct Clint {
    /// `mtime` counts from here rather than being stepped, so it follows the wall
    /// clock a guest's timeouts are really measured against, not how fast rysk gets
    /// through instructions. One counter, which every deadline is compared against.
    start: Instant,
    mtimecmp: Vec<u64>,
    msip: Vec<bool>,
}

impl Default for Clint {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Clint {
    /// A timer and a software interrupt for each of `harts` harts.
    pub fn new(harts: usize) -> Self {
        Self {
            start: Instant::now(),
            // No deadline. The reset value is not specified, and zero would mean every
            // timer has already expired before firmware has had a chance to arm one.
            mtimecmp: vec![u64::MAX; harts],
            msip: vec![false; harts],
        }
    }

    pub fn mtime(&self) -> u64 {
        let elapsed = self.start.elapsed().as_nanos();
        (elapsed * FREQUENCY as u128 / 1_000_000_000) as u64
    }

    /// The register `size` bits at `offset` names, if it names one this clint has.
    fn decode(&self, offset: u64, size: u64) -> Option<Register> {
        match (offset, size) {
            (MTIME, 64) => Some(Register::Mtime),
            (MSIP0..MSIP_END, 32) => {
                let hart = (offset / 4) as usize;
                (offset.is_multiple_of(4) && hart < self.msip.len()).then_some(Register::Msip(hart))
            }
            (MTIMECMP0..MTIME, 64) => {
                let at = offset - MTIMECMP0;
                let hart = (at / 8) as usize;
                (at.is_multiple_of(8) && hart < self.mtimecmp.len())
                    .then_some(Register::Mtimecmp(hart))
            }
            _ => None,
        }
    }
}

impl Device for Clint {
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        Ok(match self.decode(offset, size) {
            Some(Register::Msip(hart)) => self.msip[hart] as u64,
            Some(Register::Mtimecmp(hart)) => self.mtimecmp[hart],
            Some(Register::Mtime) => self.mtime(),
            None => return Err(Exception::LoadAccessFault(offset)),
        })
    }

    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        match self.decode(offset, size) {
            // Only the low bit of an msip register exists; the rest reads as zero.
            Some(Register::Msip(hart)) => self.msip[hart] = value & 1 == 1,
            Some(Register::Mtimecmp(hart)) => self.mtimecmp[hart] = value,
            // mtime is writable, which is how a debugger or a boot rom sets the clock.
            Some(Register::Mtime) => {}
            None => return Err(Exception::StoreAmoAccessFault(offset)),
        }
        Ok(())
    }

    /// The timer interrupt is a comparison, not an event: it is asserted for exactly
    /// as long as `mtime` is at or past that hart's `mtimecmp`, and the only way to
    /// clear it is to move that deadline.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9.
    fn interrupts(&self, hart: usize) -> u64 {
        let mut bits = 0;
        if self.msip[hart] {
            bits |= MSIP;
        }
        if self.mtime() >= self.mtimecmp[hart] {
            bits |= MTIP;
        }
        bits
    }
}
