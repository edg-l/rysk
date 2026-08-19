use std::{cmp::Ordering, ops::Range};

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    device::Device,
    dram::{DRAM_SIZE, Dram},
    exception::Exception,
};

/// The address which dram starts, same as QEMU virt machine.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// Address decode: which device owns an address, and dram.
///
/// Dram is not one of the devices. It is the overwhelming majority of accesses and the
/// only one on the fetch path, so it is answered before the search begins and pays
/// nothing for the devices existing.
#[derive(Debug)]
pub struct Bus {
    pub dram: Dram,
    /// Every device, sorted by base address and never overlapping, so an address
    /// decodes by binary search.
    devices: Vec<(Range<u64>, Box<dyn Device>)>,
    /// The bytes reserved by the last load-reserved, invalidated by any store that
    /// overlaps them. One hart, so at most one reservation.
    pub reservation: Option<Range<u64>>,
}

impl Bus {
    pub fn new(dram: Dram) -> Self {
        Self {
            dram,
            devices: Vec::new(),
            reservation: None,
        }
    }

    /// Place `device` so it answers for `size` bytes from `base`. Two devices may not
    /// claim the same address, and a machine that says they do is built wrong.
    pub fn attach(&mut self, base: u64, size: u64, device: Box<dyn Device>) {
        let range = base..base + size;
        let at = self
            .devices
            .partition_point(|(existing, _)| existing.start < range.start);
        let clashes = |other: &(Range<u64>, Box<dyn Device>)| {
            range.start < other.0.end && other.0.start < range.end
        };
        assert!(
            !self.devices.get(at).is_some_and(clashes)
                && !at
                    .checked_sub(1)
                    .and_then(|i| self.devices.get(i))
                    .is_some_and(clashes),
            "a device already answers for {range:#x?}"
        );
        self.devices.insert(at, (range, device));
    }

    /// The device that owns all `size` bits at `addr`, if one does. An access that
    /// runs off the end of a device is not that device's to answer.
    fn device(&mut self, addr: u64, size: u64) -> Option<&mut (Range<u64>, Box<dyn Device>)> {
        let end = addr.checked_add(size / 8)?;
        let found = self.devices.binary_search_by(|(range, _)| {
            if range.end <= addr {
                Ordering::Less
            } else if range.start > addr {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        let at = found.ok()?;
        (end <= self.devices[at].0.end).then(|| &mut self.devices[at])
    }

    /// Whether any device is asserting its interrupt line.
    pub fn pending(&self) -> bool {
        self.devices.iter().any(|(_, device)| device.pending())
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    pub fn load(&mut self, addr: u64, size: u64) -> Result<u64, Exception> {
        trace_mem!("load");
        if self.in_dram(addr, size) {
            return Ok(self.dram.load(addr, size));
        }
        match self.device(addr, size) {
            Some((range, device)) => {
                let offset = addr - range.start;
                device.load(offset, size)
            }
            None => Err(Exception::LoadAccessFault(addr)),
        }
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), Exception> {
        trace_mem!("store");
        if !self.in_dram(addr, size) {
            return match self.device(addr, size) {
                Some((range, device)) => {
                    let offset = addr - range.start;
                    device.store(offset, size, value)
                }
                None => Err(Exception::StoreAmoAccessFault(addr)),
            };
        }
        if let Some(reserved) = &self.reservation
            && addr < reserved.end
            && reserved.start < addr + size / 8
        {
            self.reservation = None;
        }
        self.dram.store(addr, size, value);
        Ok(())
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
    fn in_dram(&self, addr: u64, size: u64) -> bool {
        match addr.checked_add(size / 8) {
            Some(end) => DRAM_BASE <= addr && end <= DRAM_BASE + DRAM_SIZE,
            None => false,
        }
    }
}
