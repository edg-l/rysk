use tracing::{instrument, trace};

use crate::dram::Dram;

/// The address which dram starts, same as QEMU virt machine.
pub const DRAM_BASE: u64 = 0x8000_0000;

#[derive(Debug, Clone)]
pub struct Bus {
    pub dram: Dram,
}

impl Bus {
    #[instrument(skip(self))]
    pub fn load(&self, addr: u64, size: u64) -> Result<u64, ()> {
        trace!("load");
        if DRAM_BASE <= addr {
            return self.dram.load(addr, size);
        }
        Err(())
    }

    #[instrument(skip(self))]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), ()> {
        trace!("store");
        if DRAM_BASE <= addr {
            return self.dram.store(addr, size, value);
        }
        Err(())
    }
}
