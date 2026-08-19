//! The core-local interruptor: a monotonic counter, the deadline it is compared
//! against, and a bit software sets to interrupt itself. It is the first thing on the
//! machine that can raise an interrupt without an instruction asking for one.
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

const MSIP0: u64 = 0x0000;
const MTIMECMP0: u64 = 0x4000;
const MTIME: u64 = 0xbff8;

#[derive(Debug)]
pub struct Clint {
    /// `mtime` counts from here rather than being stepped, so it follows the wall
    /// clock a guest's timeouts are really measured against, not how fast rysk gets
    /// through instructions.
    start: Instant,
    mtimecmp: u64,
    msip: bool,
}

impl Default for Clint {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            // No deadline. The reset value is not specified, and zero would mean the
            // timer has already expired before firmware has had a chance to arm it.
            mtimecmp: u64::MAX,
            msip: false,
        }
    }
}

impl Clint {
    pub fn mtime(&self) -> u64 {
        let elapsed = self.start.elapsed().as_nanos();
        (elapsed * FREQUENCY as u128 / 1_000_000_000) as u64
    }
}

impl Device for Clint {
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        Ok(match (offset, size) {
            (MSIP0, 32) => self.msip as u64,
            (MTIMECMP0, 64) => self.mtimecmp,
            (MTIME, 64) => self.mtime(),
            _ => return Err(Exception::LoadAccessFault(offset)),
        })
    }

    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        match (offset, size) {
            // Only the low bit of an msip register exists; the rest reads as zero.
            (MSIP0, 32) => self.msip = value & 1 == 1,
            (MTIMECMP0, 64) => self.mtimecmp = value,
            // mtime is writable, which is how a debugger or a boot rom sets the clock.
            (MTIME, 64) => {}
            _ => return Err(Exception::StoreAmoAccessFault(offset)),
        }
        Ok(())
    }

    /// The timer interrupt is a comparison, not an event: it is asserted for exactly
    /// as long as `mtime` is at or past `mtimecmp`, and the only way to clear it is to
    /// move the deadline.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9.
    fn interrupts(&self) -> u64 {
        let mut bits = 0;
        if self.msip {
            bits |= MSIP;
        }
        if self.mtime() >= self.mtimecmp {
            bits |= MTIP;
        }
        bits
    }
}
