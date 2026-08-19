use std::{cmp::Ordering, ops::Range};

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{device::Device, dram::Dram, trap::Exception};

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
    /// The bytes each hart reserved with its last load-reserved, invalidated by any
    /// store that overlaps them. A hart has at most one reservation, and the set of
    /// them belongs to the memory system rather than to any hart: a store has to
    /// break every reservation it overlaps, whichever hart made it, and only the thing
    /// the stores go through can see them all.
    ///
    /// The RISC-V Instruction Set Manual Volume I, 14.2.
    reservations: Vec<Option<Range<u64>>>,
    /// How many of them are live, so that a store which cannot break one does not look
    /// at them at all. Almost no store can: only the window between a load-reserved
    /// and its store-conditional has anything reserved.
    reserved: usize,
}

impl Bus {
    /// A bus with memory and room for `harts` reservations.
    pub fn new(dram: Dram, harts: usize) -> Self {
        Self {
            dram,
            devices: Vec::new(),
            reservations: vec![None; harts],
            reserved: 0,
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

    /// Let every device notice whatever arrived without an access to notice it at.
    pub fn poll(&mut self) {
        for (_, device) in &mut self.devices {
            device.poll();
        }
    }

    /// The bits the devices are asserting in `hart`'s `mip`, together.
    pub fn interrupts(&self, hart: usize) -> u64 {
        self.devices
            .iter()
            .fold(0, |bits, (_, device)| bits | device.interrupts(hart))
    }

    /// Read `size` bits at `addr`.
    ///
    /// Dram is inline and the devices are not. Every fetch comes through here and
    /// almost every access is dram, so the answer for dram is a bounds check and a
    /// move at the call site, with the width already folded to a constant; a device
    /// costs a call, which it was going to cost anyway.
    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    #[inline]
    pub fn load(&mut self, addr: u64, size: u64) -> Result<u64, Exception> {
        trace_mem!("load");
        if self.in_dram(addr, size) {
            return Ok(self.dram.load(addr, size));
        }
        self.device_load(addr, size)
    }

    #[inline(never)]
    fn device_load(&mut self, addr: u64, size: u64) -> Result<u64, Exception> {
        match self.device(addr, size) {
            Some((range, device)) => {
                let offset = addr - range.start;
                device.load(offset, size).map_err(|e| e.at(addr))
            }
            None => Err(Exception::LoadAccessFault(addr)),
        }
    }

    /// Write the low `size` bits of `value` at `addr`, with the same split.
    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    #[inline]
    pub fn store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), Exception> {
        trace_mem!("store");
        if !self.in_dram(addr, size) {
            return self.device_store(addr, size, value);
        }
        if self.reserved != 0 {
            self.break_reservations(addr, size);
        }
        self.dram.store(addr, size, value);
        Ok(())
    }

    #[inline(never)]
    fn device_store(&mut self, addr: u64, size: u64, value: u64) -> Result<(), Exception> {
        match self.device(addr, size) {
            Some((range, device)) => {
                let offset = addr - range.start;
                device.store(offset, size, value).map_err(|e| e.at(addr))
            }
            None => Err(Exception::StoreAmoAccessFault(addr)),
        }
    }

    /// Release every reservation the bytes written by a store overlap, whichever hart
    /// holds it, which is what makes a store-conditional fail after another hart wrote
    /// what was reserved.
    #[inline(never)]
    fn break_reservations(&mut self, addr: u64, size: u64) {
        let written = addr..addr + size / 8;
        for reservation in &mut self.reservations {
            if reservation
                .as_ref()
                .is_some_and(|r| written.start < r.end && r.start < written.end)
            {
                *reservation = None;
                self.reserved -= 1;
            }
        }
    }

    /// Reserve, for `hart`, the bytes a load-reserved of `size` bits at `addr` reads.
    pub fn reserve(&mut self, hart: usize, addr: u64, size: u64) {
        if self.reservations[hart]
            .replace(addr..addr + size / 8)
            .is_none()
        {
            self.reserved += 1;
        }
    }

    /// Whether a store-conditional by `hart` of `size` bits at `addr` may write. That
    /// hart's reservation is released either way.
    pub fn take_reservation(&mut self, hart: usize, addr: u64, size: u64) -> bool {
        let written = addr..addr + size / 8;
        let Some(reserved) = self.reservations[hart].take() else {
            return false;
        };
        self.reserved -= 1;
        reserved.start <= written.start && written.end <= reserved.end
    }

    /// Whether `size` bits at `addr` fall inside dram.
    #[inline]
    pub fn in_dram(&self, addr: u64, size: u64) -> bool {
        match addr.checked_add(size / 8) {
            Some(end) => DRAM_BASE <= addr && end <= DRAM_BASE + self.dram.size(),
            None => false,
        }
    }
}
