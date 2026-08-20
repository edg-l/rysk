use std::{
    cmp::Ordering,
    ops::Range,
    sync::{
        Mutex,
        atomic::{
            AtomicU64, AtomicUsize,
            Ordering::{Relaxed, SeqCst},
        },
    },
};

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    device::{Device, Pending},
    dram::Dram,
    trap::Exception,
};

/// The address which dram starts, same as QEMU virt machine.
pub const DRAM_BASE: u64 = 0x8000_0000;

/// A device and the addresses it answers for. The lock is what supplies the `&mut self`
/// an access is given, since several harts hold the bus at once and only one of them may
/// be inside a device at a time.
type Attached = (Range<u64>, Mutex<Box<dyn Device>>);

/// A word no reservation can be, so a hart holding none needs no second field to say so.
const UNRESERVED: u64 = u64::MAX;

/// What one hart has reserved, in a word: the address, with the low bit saying whether
/// it took eight bytes or four. A load-reserved is four or eight bytes and traps unless
/// it is naturally aligned, so the low bit is never the address's to use.
/// The RISC-V Instruction Set Manual Volume I, 14.2.
#[inline]
fn reserved_word(addr: u64, size: u64) -> u64 {
    addr | (size == 64) as u64
}

/// The bytes that word stands for.
#[inline]
fn reserved_bytes(word: u64) -> Range<u64> {
    let base = word & !1;
    base..base + if word & 1 == 1 { 8 } else { 4 }
}

/// Address decode: which device owns an address, and dram.
///
/// Dram is not one of the devices. It is the overwhelming majority of accesses and the
/// only one on the fetch path, so it is answered before the search begins and pays
/// nothing for the devices existing.
///
/// Nothing here needs `&mut`, so several harts on several host threads can reach the
/// address space at once. Dram is shared without a lock, since the guest's own barriers
/// are what order it; a device is not, because `Device` answers an access with `&mut
/// self` for good reason and the lock is what supplies it. That lock is only ever taken
/// for an access that was not memory, which is rare by construction.
#[derive(Debug)]
pub struct Bus {
    pub dram: Dram,
    /// Every device, sorted by base address and never overlapping, so an address
    /// decodes by binary search.
    devices: Vec<Attached>,
    /// The word the interrupt controllers drive `mip` through, one per hart. What a
    /// hart asks before every instruction is one load from it, however many controllers
    /// the machine has and however many devices are on the bus.
    pending: Pending,
    /// Where in `devices` the interrupt controllers are. A wire moves when the device
    /// driving it is accessed, which is an access the controller on the other end
    /// never sees, so the controllers are asked to look again after every access that
    /// was not dram.
    controllers: Vec<usize>,
    /// The bytes each hart reserved with its last load-reserved, invalidated by any
    /// store that overlaps them. A hart has at most one reservation, and the set of
    /// them belongs to the memory system rather than to any hart: a store has to
    /// break every reservation it overlaps, whichever hart made it, and only the thing
    /// the stores go through can see them all.
    ///
    /// The RISC-V Instruction Set Manual Volume I, 14.2.
    /// One word per hart rather than one lock over all of them, because the harts that
    /// contend for these are exactly the ones contending for a lock the guest built out
    /// of them, and making them queue behind each other here would be inventing the
    /// contention the guest is trying to resolve.
    reservations: Box<[AtomicU64]>,
    /// How many of them are live, so that a store which cannot break one does not look
    /// at them at all. Almost no store can: only the window between a load-reserved
    /// and its store-conditional has anything reserved.
    reserved: AtomicUsize,
    /// Held across a quadword compare-and-swap, which is the one atomic instruction no
    /// host primitive answers for: the standard library has no 128-bit atomic. It makes
    /// two of them indivisible against each other and against nothing else, so a plain
    /// store landing between the halves is still a store landing between the halves.
    /// Naming it is the point; a guest using `amocas.q` against ordinary stores to the
    /// same octoword has no machine to run on anyway.
    wide: Mutex<()>,
}

impl Bus {
    /// A bus with memory and room for `harts` reservations.
    pub fn new(dram: Dram, harts: usize) -> Self {
        Self {
            dram,
            devices: Vec::new(),
            pending: Pending::new(harts),
            controllers: Vec::new(),
            reservations: (0..harts).map(|_| AtomicU64::new(UNRESERVED)).collect(),
            reserved: AtomicUsize::new(0),
            wide: Mutex::new(()),
        }
    }

    /// Place `device` so it answers for `size` bytes from `base`. Two devices may not
    /// claim the same address, and a machine that says they do is built wrong.
    pub fn attach(&mut self, base: u64, size: u64, mut device: Box<dyn Device>) {
        let range = base..base + size;
        // Dram is answered before the devices are searched, so a device underneath it
        // would never be reached. That is a machine built wrong rather than a device
        // that quietly stops answering, and it only becomes reachable once a machine
        // has a window above dram and enough memory to grow into it.
        assert!(
            range.start >= DRAM_BASE + self.dram.size() || range.end <= DRAM_BASE,
            "dram already answers for part of {range:#x?}"
        );
        let at = self
            .devices
            .partition_point(|(existing, _)| existing.start < range.start);
        let clashes = |other: &Attached| range.start < other.0.end && other.0.start < range.end;
        assert!(
            !self.devices.get(at).is_some_and(clashes)
                && !at
                    .checked_sub(1)
                    .and_then(|i| self.devices.get(i))
                    .is_some_and(clashes),
            "a device already answers for {range:#x?}"
        );
        // Inserting ahead of a controller moves it along one.
        for controller in &mut self.controllers {
            if *controller >= at {
                *controller += 1;
            }
        }
        // A controller takes its handle on the word it drives `mip` through once, here.
        // One seen at two addresses takes one per address and is looked at through
        // both, since either can be the half with something to say.
        if device.wire(&self.pending) {
            self.controllers.push(at);
        }
        self.devices.insert(at, (range, Mutex::new(device)));
    }

    /// Let the controllers see what an access did to the wires. A device raises its
    /// line while it is being read or written, and the controller it runs to is not
    /// part of that access, so this is the only place the two meet.
    #[inline(never)]
    fn resample(&self) {
        for at in &self.controllers {
            self.held(*at).poll();
        }
    }

    /// The device at `at`, held for as long as the access lasts. Nothing a device does
    /// while it is being accessed comes back through the bus: a line is an atomic and a
    /// message goes to an interrupt file directly, both of which say so where they are
    /// defined, so no access is ever waiting on the one it is inside.
    #[inline]
    fn held(&self, at: usize) -> impl std::ops::DerefMut<Target = Box<dyn Device>> {
        self.devices[at]
            .1
            .lock()
            .unwrap_or_else(|held| held.into_inner())
    }

    /// The device that owns all `size` bits at `addr`, if one does. An access that
    /// runs off the end of a device is not that device's to answer.
    fn device(&self, addr: u64, size: u64) -> Option<usize> {
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
        (end <= self.devices[at].0.end).then_some(at)
    }

    /// Let every device notice whatever arrived without an access to notice it at.
    ///
    /// The controllers go last, whatever address they were given. A byte typed at a
    /// serial port is a line raised inside that port's own poll, and a controller that
    /// had already looked would not see it until the round after.
    pub fn poll(&self) {
        for at in 0..self.devices.len() {
            if !self.controllers.contains(&at) {
                self.held(at).poll();
            }
        }
        self.resample();
    }

    /// The bits the controllers are asserting in `hart`'s `mip`, together.
    #[inline]
    pub fn interrupts(&self, hart: usize) -> u64 {
        self.pending.get(hart)
    }

    /// Read `size` bits at `addr`.
    ///
    /// Dram is inline and the devices are not. Every fetch comes through here and
    /// almost every access is dram, so the answer for dram is a bounds check and a
    /// move at the call site, with the width already folded to a constant; a device
    /// costs a call, which it was going to cost anyway.
    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> Result<u64, Exception> {
        trace_mem!("load");
        if self.in_dram(addr, size) {
            return Ok(self.dram.load(addr, size));
        }
        self.device_load(addr, size)
    }

    #[inline(never)]
    fn device_load(&self, addr: u64, size: u64) -> Result<u64, Exception> {
        let answer = match self.device(addr, size) {
            Some(at) => {
                let offset = addr - self.devices[at].0.start;
                self.held(at).load(offset, size).map_err(|e| e.at(addr))
            }
            None => Err(Exception::LoadAccessFault(addr)),
        };
        self.resample();
        answer
    }

    /// Write the low `size` bits of `value` at `addr`, with the same split.
    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    #[inline]
    pub fn store(&self, addr: u64, size: u64, value: u64) -> Result<(), Exception> {
        trace_mem!("store");
        if !self.in_dram(addr, size) {
            return self.device_store(addr, size, value);
        }
        self.dram.store(addr, size, value);
        // After the write, not before it, and that order is the whole of what makes a
        // store-conditional safe against another hart. Breaking first leaves a window
        // where this hart sees no reservation, another hart takes one and reads the old
        // value, this store lands, and its store-conditional then succeeds against
        // memory that moved under it: two harts leaving the same lock believing they
        // hold it. Breaking afterwards closes it the other way round, since a hart that
        // reserved early enough to miss this scan also reserved early enough that its
        // own read came before this write.
        //
        // What is left is the width of a store buffer. A hart that reserves while this
        // store is still draining is neither seen by the scan nor sees the write, and
        // only a barrier between the two would order them, which is what a real
        // machine's cache coherence is and is not something a store can afford per
        // store. A store-conditional is allowed to fail when it need not; this is the
        // far rarer converse, and it is the bargain every emulator running harts on
        // host threads makes.
        if self.reserved.load(SeqCst) != 0 {
            self.break_reservations(addr, size);
        }
        Ok(())
    }

    /// Replace `size` bits at `addr` with what `change` makes of them, as one operation
    /// no other hart can land inside, and answer what was there. This is every atomic
    /// memory operation: the read, the arithmetic and the write are one thing, which on
    /// memory is the host's own atomic and on a device is the device held throughout.
    pub fn modify(
        &self,
        addr: u64,
        size: u64,
        change: impl FnMut(u64) -> u64,
    ) -> Result<u64, Exception> {
        if !self.in_dram(addr, size) {
            return self.device_modify(addr, size, change);
        }
        let was = self.dram.modify(addr, size, change);
        if self.reserved.load(SeqCst) != 0 {
            self.break_reservations(addr, size);
        }
        Ok(was)
    }

    /// Put `new` in place of `size` bits at `addr` if they are `expected`, and answer
    /// what was there either way.
    pub fn compare_swap(
        &self,
        addr: u64,
        size: u64,
        expected: u64,
        new: u64,
    ) -> Result<u64, Exception> {
        if !self.in_dram(addr, size) {
            return self.device_modify(addr, size, |was| if was == expected { new } else { was });
        }
        let was = self.dram.compare_swap(addr, size, expected, new);
        if was == expected && self.reserved.load(SeqCst) != 0 {
            self.break_reservations(addr, size);
        }
        Ok(was)
    }

    /// The same for the two doublewords at `addr`, which no host atomic answers for.
    pub fn compare_swap_wide(
        &self,
        addr: u64,
        expected: (u64, u64),
        new: (u64, u64),
    ) -> Result<(u64, u64), Exception> {
        let _held = self.wide.lock().unwrap_or_else(|held| held.into_inner());
        let was = (self.load(addr, 64)?, self.load(addr + 8, 64)?);
        if was == expected {
            self.store(addr, 64, new.0)?;
            self.store(addr + 8, 64, new.1)?;
        }
        Ok(was)
    }

    /// A device read and the write that answers it, with the device held across both so
    /// no other hart is inside it in between.
    #[inline(never)]
    fn device_modify(
        &self,
        addr: u64,
        size: u64,
        mut change: impl FnMut(u64) -> u64,
    ) -> Result<u64, Exception> {
        let answer = match self.device(addr, size) {
            Some(at) => {
                let offset = addr - self.devices[at].0.start;
                let mut device = self.held(at);
                match device.load(offset, size) {
                    Ok(was) => device
                        .store(offset, size, change(was))
                        .map(|()| was)
                        .map_err(|e| e.at(addr)),
                    Err(e) => Err(e.at(addr)),
                }
            }
            None => Err(Exception::StoreAmoAccessFault(addr)),
        };
        self.resample();
        answer
    }

    #[inline(never)]
    fn device_store(&self, addr: u64, size: u64, value: u64) -> Result<(), Exception> {
        let answer = match self.device(addr, size) {
            Some(at) => {
                let offset = addr - self.devices[at].0.start;
                self.held(at)
                    .store(offset, size, value)
                    .map_err(|e| e.at(addr))
            }
            None => Err(Exception::StoreAmoAccessFault(addr)),
        };
        self.resample();
        answer
    }

    /// Release every reservation the bytes written by a store overlap, whichever hart
    /// holds it, which is what makes a store-conditional fail after another hart wrote
    /// what was reserved.
    #[inline(never)]
    fn break_reservations(&self, addr: u64, size: u64) {
        let written = addr..addr + size / 8;
        for reservation in &self.reservations {
            let word = reservation.load(SeqCst);
            if word == UNRESERVED {
                continue;
            }
            let held = reserved_bytes(word);
            // Only the hart that takes the word away counts it away, so two stores
            // breaking one reservation at once still only decrement once.
            if written.start < held.end
                && held.start < written.end
                && reservation
                    .compare_exchange(word, UNRESERVED, SeqCst, Relaxed)
                    .is_ok()
            {
                self.reserved.fetch_sub(1, SeqCst);
            }
        }
    }

    /// Reserve, for `hart`, the bytes a load-reserved of `size` bits at `addr` reads.
    ///
    /// Published before the load it belongs to is answered, so a store about to break it
    /// either sees it here or has already written what that load is about to read.
    pub fn reserve(&self, hart: usize, addr: u64, size: u64) {
        debug_assert!(
            (size == 32 || size == 64) && addr.is_multiple_of(size / 8),
            "a load-reserved of {size} bits at {addr:#x}"
        );
        if self.reservations[hart].swap(reserved_word(addr, size), SeqCst) == UNRESERVED {
            self.reserved.fetch_add(1, SeqCst);
        }
    }

    /// Whether a store-conditional by `hart` of `size` bits at `addr` may write. That
    /// hart's reservation is released either way.
    pub fn take_reservation(&self, hart: usize, addr: u64, size: u64) -> bool {
        let written = addr..addr + size / 8;
        let word = self.reservations[hart].swap(UNRESERVED, SeqCst);
        if word == UNRESERVED {
            return false;
        }
        let reserved = reserved_bytes(word);
        self.reserved.fetch_sub(1, SeqCst);
        reserved.start <= written.start && written.end <= reserved.end
    }

    /// Whether `size` bits at `addr` fall inside dram.
    #[inline]
    pub fn in_dram(&self, addr: u64, size: u64) -> bool {
        self.dram.contains(addr, size)
    }
}
