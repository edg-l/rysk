use std::{cell::UnsafeCell, fmt, slice};

use crate::bus::DRAM_BASE;

/// How much memory a machine has when nothing says otherwise. Enough for a kernel and
/// the room it wants after itself, and small enough to allocate without thinking.
pub const DRAM_SIZE: u64 = 1024 * 1024 * 128;

/// The memory every hart shares.
///
/// The bytes are `UnsafeCell` and are reached through raw pointers, so that harts on
/// different host threads can load and store at the same time. This is the one place in
/// rysk that is unsound by Rust's rules and deliberate about it: two threads accessing
/// the same byte without synchronisation is a data race, and a data race is undefined
/// behaviour whatever the hardware underneath would have done. There is no sound
/// alternative that keeps the machine's shape. A byte-wise atomic backing store turns an
/// eight-byte load into eight loads and shifts, and a word-wise one makes every sub-word
/// store a read-modify-write and takes away the contiguous `&[u8]` that a display's
/// framebuffer and a disk's transfer both want. Every emulator that runs guests on host
/// threads makes this same trade.
///
/// What keeps it honest is that guest memory is only ever reached through `load` and
/// `store` here, both of which check their bounds, and that the guest's own memory model
/// is what says when a racing access means anything: RVWMO orders accesses with `fence`
/// and the atomics, and a guest that races without them may read whatever it likes, which
/// is what a real machine gives it too.
pub struct Dram {
    bytes: Box<[UnsafeCell<u8>]>,
}

/// Sound only under the contract above: the bytes are shared, the guest's own barriers
/// are what order them, and nothing hands out a reference that outlives an access.
/// `Send` needs no such claim, since a `u8` behind an `UnsafeCell` already is one.
unsafe impl Sync for Dram {}

impl Dram {
    pub fn new(code: Vec<u8>) -> Dram {
        Self::with_size(code, DRAM_SIZE)
    }

    /// A machine with `size` bytes of memory. How much there is is a property of the
    /// machine rather than of the emulator, so it is asked for rather than assumed:
    /// a kernel image is tens of megabytes before it has run an instruction.
    pub fn with_size(code: Vec<u8>, size: u64) -> Dram {
        let mut bytes = vec![0u8; size as usize];
        bytes.splice(..code.len(), code);
        // `UnsafeCell<u8>` is `repr(transparent)` over `u8`, so this is the same
        // allocation seen as the same bytes, and the zeroed pages the allocator handed
        // over stay untouched rather than being copied into a second buffer.
        let bytes = Box::into_raw(bytes.into_boxed_slice()) as *mut [UnsafeCell<u8>];

        Self {
            bytes: unsafe { Box::from_raw(bytes) },
        }
    }

    /// How much memory there is, in bytes.
    #[inline]
    pub fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// The first byte, as a pointer. Every access starts here.
    ///
    /// Writing through it is what `UnsafeCell` is for: a shared reference to one is
    /// permission to mutate what is inside.
    #[inline]
    fn base(&self) -> *mut u8 {
        self.bytes.as_ptr().cast::<u8>().cast_mut()
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
        unsafe { slice::from_raw_parts(self.base(), self.bytes.len()) }
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
        if end > self.bytes.len() {
            return false;
        }
        // `&mut self` is exclusive access to the whole of memory, so this is the one
        // path that may hold a slice of it: no hart can be running to race with it.
        let memory = unsafe { slice::from_raw_parts_mut(self.base(), self.bytes.len()) };
        let start = start as usize;
        memory[start..start + bytes.len()].copy_from_slice(bytes);
        memory[start + bytes.len()..end].fill(0);
        true
    }

    /// Read `size` bits at `addr`.
    ///
    /// The width is a constant at almost every call site, so each arm folds to the one
    /// load it describes: an array of bytes is aligned to one, so the read answers for
    /// an address the guest did not align and still becomes a single move.
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
    /// today, since the address is recomputed from guest registers through calls that
    /// clobber memory, and a hart spinning on what another one writes is what stops
    /// being able to see it if a later compiler ever manages it.
    #[inline]
    pub fn load(&self, addr: u64, size: u64) -> u64 {
        let index = self.offset(addr, size);
        match size {
            8 => u8::from_le_bytes(self.bytes(index)) as u64,
            16 => u16::from_le_bytes(self.bytes(index)) as u64,
            32 => u32::from_le_bytes(self.bytes(index)) as u64,
            64 => u64::from_le_bytes(self.bytes(index)),
            _ => unreachable!("load of {size} bits"),
        }
    }

    /// Write the low `size` bits of `value` at `addr`.
    #[inline]
    pub fn store(&self, addr: u64, size: u64, value: u64) {
        let index = self.offset(addr, size);
        match size {
            8 => self.put(index, (value as u8).to_le_bytes()),
            16 => self.put(index, (value as u16).to_le_bytes()),
            32 => self.put(index, (value as u32).to_le_bytes()),
            64 => self.put(index, value.to_le_bytes()),
            _ => unreachable!("store of {size} bits"),
        }
    }

    /// The `N` bytes at `index`, which `offset` has already found to be inside memory.
    #[inline]
    fn bytes<const N: usize>(&self, index: usize) -> [u8; N] {
        unsafe { self.base().add(index).cast::<[u8; N]>().read_unaligned() }
    }

    /// Put `bytes` at `index`, which `offset` has already found to be inside memory.
    #[inline]
    fn put<const N: usize>(&self, index: usize, bytes: [u8; N]) {
        unsafe {
            self.base()
                .add(index)
                .cast::<[u8; N]>()
                .write_unaligned(bytes)
        }
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

impl fmt::Debug for Dram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dram").field("size", &self.size()).finish()
    }
}
