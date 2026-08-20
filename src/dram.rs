use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering::SeqCst};

use crate::{bus::DRAM_BASE, shared::Bytes};

/// How much memory a machine has when nothing says otherwise. Enough for a kernel and
/// the room it wants after itself, and small enough to allocate without thinking.
pub const DRAM_SIZE: u64 = 1024 * 1024 * 128;

/// The memory every hart shares: a `shared::Bytes` placed at `DRAM_BASE`, plus the
/// host atomics an atomic instruction needs.
///
/// The argument for reaching those bytes from several threads without a lock is in
/// `shared`, and it is the same argument here. What this type adds is where memory
/// begins, so that a guest address is checked once and turned into an index once, and
/// the four widths of indivisible access an atomic instruction can ask for.
#[derive(Debug)]
pub struct Dram {
    bytes: Bytes,
}

impl Dram {
    pub fn new(code: Vec<u8>) -> Dram {
        Self::with_size(code, DRAM_SIZE)
    }

    /// A machine with `size` bytes of memory. How much there is is a property of the
    /// machine rather than of the emulator, so it is asked for rather than assumed:
    /// a kernel image is tens of megabytes before it has run an instruction.
    pub fn with_size(code: Vec<u8>, size: u64) -> Dram {
        Self {
            bytes: Bytes::new(code, size),
        }
    }

    /// How much memory there is, in bytes.
    #[inline]
    pub fn size(&self) -> u64 {
        self.bytes.len()
    }

    /// Whether all `size` bits at `addr` are memory. This is what the bus asks to decide
    /// whether an access is memory at all, and what an access asserts before it reaches
    /// the bytes, and it is one function so that the two are the same comparison: the
    /// second is then a repeat the compiler can see is answered and drop, which is what
    /// keeps a checked access costing what an unchecked one does.
    #[inline]
    pub fn contains(&self, addr: u64, size: u64) -> bool {
        match addr.checked_add(size / 8) {
            Some(end) => DRAM_BASE <= addr && end <= DRAM_BASE + self.size(),
            None => false,
        }
    }

    /// Where `size` bits at `addr` start, once all of them are known to be inside
    /// memory. Every access comes through here, so nothing below it repeats the check.
    #[inline]
    fn offset(&self, addr: u64, size: u64) -> usize {
        assert!(
            self.contains(addr, size),
            "{addr:#x} is not {size} bits of memory"
        );
        (addr - DRAM_BASE) as usize
    }

    /// Every byte of memory, for a caller that needs them contiguous: a device
    /// transferring to or from a window of guest memory.
    ///
    /// # Safety
    ///
    /// The bytes are shared with every running hart, so what the reference says is only
    /// meaningful to a caller that knows what else may be writing them.
    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { self.bytes.as_slice() }
    }

    /// Place `bytes` at `addr` and zero `zeroes` bytes after them, as loading an image
    /// does. Returns whether it fit.
    pub fn write(&mut self, addr: u64, bytes: &[u8], zeroes: u64) -> bool {
        let Some(start) = addr.checked_sub(DRAM_BASE) else {
            return false;
        };
        let Some(end) = (start as usize).checked_add(bytes.len() + zeroes as usize) else {
            return false;
        };
        if end as u64 > self.size() {
            return false;
        }
        let memory = self.bytes.as_mut_slice();
        let start = start as usize;
        memory[start..start + bytes.len()].copy_from_slice(bytes);
        memory[start + bytes.len()..end].fill(0);
        true
    }

    /// Read `size` bits at `addr`.
    ///
    /// Deliberately not volatile. Volatile would stop the compiler moving one guest
    /// access across another, but RVWMO already permits exactly that for two ordinary
    /// accesses with no `fence` between them, so it buys ordering the guest never asked
    /// for; what the guest does ask for arrives as a `fence` or an atomic, and both of
    /// those are compiler barriers in their own right. It is not free either: volatile
    /// forbids merging the element accesses, so a volatile read of eight bytes is eight
    /// loads, seven shifts and seven ors instead of one move, and 3.2% of the host
    /// instructions `bench/paging.bin` retires.
    ///
    /// What it leaves is the progress axiom (Volume I, 17.1): a hart spinning on a
    /// plain load has to see a remote store eventually, which a compiler that hoisted
    /// the load out of the interpreter's own loop would break. Nothing can hoist it
    /// today, since the address is worked out afresh from guest registers through calls
    /// that clobber memory, and `a_hart_spinning_on_a_plain_load_sees_another_hart_write`
    /// is what stops passing, by hanging, if a later compiler ever manages it.
    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> u64 {
        self.bytes.load(self.offset(addr, size), size)
    }

    /// Write the low `size` bits of `value` at `addr`.
    #[inline]
    pub fn store(&self, addr: u64, size: u64, value: u64) {
        self.bytes.store(self.offset(addr, size), size, value);
    }
}

/// The same operation at each of the four widths an atomic instruction has, because the
/// standard library gives its four atomic integers no trait in common to be generic over.
///
/// Every one of them is `SeqCst`. An instruction says what it needs with its `aq` and
/// `rl` bits and none of them asks for more than this, so ordering every one the
/// strongest way is correct for all of them and is one thing to reason about rather than
/// four. Volume I, 14.4.
macro_rules! at_width {
    ($size:expr, $at:expr, |$atom:ident| $body:expr) => {
        match $size {
            8 => {
                let $atom = unsafe { AtomicU8::from_ptr($at.cast()) };
                ($body) as u64
            }
            16 => {
                let $atom = unsafe { AtomicU16::from_ptr($at.cast()) };
                ($body) as u64
            }
            32 => {
                let $atom = unsafe { AtomicU32::from_ptr($at.cast()) };
                ($body) as u64
            }
            64 => {
                let $atom = unsafe { AtomicU64::from_ptr($at.cast()) };
                ($body) as u64
            }
            _ => unreachable!("an atomic of {} bits", $size),
        }
    };
}

impl Dram {
    /// Where `size` bits at `addr` start, for an instruction that needs them indivisible.
    /// Naturally aligned or the instruction would have trapped before reaching here,
    /// which is what lets the host's own atomic answer for it.
    #[inline]
    fn atomically(&self, addr: u64, size: u64) -> *mut u8 {
        let index = self.offset(addr, size);
        debug_assert!(
            addr.is_multiple_of(size / 8),
            "an atomic of {size} bits at {addr:#x} is not aligned"
        );
        unsafe { self.bytes.at(index) }
    }

    /// Replace `size` bits at `addr` with what `change` makes of them, as one operation
    /// no other hart can land inside, and answer what was there.
    // The widening is what the other three widths need; at sixty-four bits it is the
    // identity, and one instantiation of a macro cannot be written differently from the
    // rest of it.
    #[allow(clippy::useless_conversion)]
    #[inline]
    pub fn modify(&self, addr: u64, size: u64, mut change: impl FnMut(u64) -> u64) -> u64 {
        let at = self.atomically(addr, size);
        at_width!(size, at, |atom| atom
            .fetch_update(SeqCst, SeqCst, |old| Some(change(u64::from(old)) as _))
            .expect("a change that always answers"))
    }

    /// Put `new` in place of `size` bits at `addr` if they are `expected`, and answer
    /// what was there either way.
    #[inline]
    pub fn compare_swap(&self, addr: u64, size: u64, expected: u64, new: u64) -> u64 {
        let at = self.atomically(addr, size);
        at_width!(size, at, |atom| match atom.compare_exchange(
            expected as _,
            new as _,
            SeqCst,
            SeqCst
        ) {
            Ok(was) | Err(was) => was,
        })
    }
}

impl Clone for Dram {
    fn clone(&self) -> Self {
        // A snapshot, which only means anything to a caller holding the memory still.
        let mut copy = Self::with_size(Vec::new(), self.size());
        copy.write(DRAM_BASE, unsafe { self.as_slice() }, 0);
        copy
    }
}
