use std::ops::Range;

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::dram::{DRAM_SIZE, Dram};

/// The address which dram starts, same as QEMU virt machine.
pub const DRAM_BASE: u64 = 0x8000_0000;

#[derive(Debug, Clone)]
pub struct Bus {
    pub dram: Dram,
    /// The bytes reserved by the last load-reserved, invalidated by any store that
    /// overlaps them. One hart, so at most one reservation.
    pub reservation: Option<Range<u64>>,
}

impl Bus {
    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    pub fn load(&self, addr: u64, size: u64) -> Result<u64, ()> {
        trace_mem!("load");
        self.check(addr, size)?;
        self.dram.load(addr, size)
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), ()> {
        trace_mem!("store");
        self.check(addr, size)?;
        if let Some(reserved) = &self.reservation
            && addr < reserved.end
            && reserved.start < addr + size / 8
        {
            self.reservation = None;
        }
        self.dram.store(addr, size, value)
    }

    /// Reserve the bytes a load-reserved of `size` bits at `addr` reads.
    pub fn reserve(&mut self, addr: u64, size: u64) {
        self.reservation = Some(addr..addr + size / 8);
    }

    /// Whether a store-conditional of `size` bits at `addr` may write. The
    /// reservation is released either way.
    pub fn take_reservation(&mut self, addr: u64, size: u64) -> bool {
        let written = addr..addr + size / 8;
        self.reservation
            .take()
            .is_some_and(|r| r.start <= written.start && written.end <= r.end)
    }

    /// Whether `size` bits at `addr` fall inside dram.
    fn check(&self, addr: u64, size: u64) -> Result<(), ()> {
        let end = addr.checked_add(size / 8).ok_or(())?;
        if DRAM_BASE <= addr && end <= DRAM_BASE + DRAM_SIZE {
            Ok(())
        } else {
            Err(())
        }
    }
}
