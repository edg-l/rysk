//! Bytes more than one thread reaches at once, and the argument for reaching them
//! without a lock.
//!
//! Two things in this machine are a flat array of bytes that several threads touch
//! while a guest is running: the memory the harts share, and the video memory a hart
//! draws into while the host reads it out. Both want the same thing, which is a byte
//! array that any number of threads may load from and store to at the same time, and
//! that a caller can also see contiguously as a `&[u8]`. This is that array, once.
//!
//! The bytes are `UnsafeCell` and are reached through raw pointers. This is the one
//! place in rysk that is unsound by Rust's rules and deliberate about it: two threads
//! accessing the same byte without synchronisation is a data race, and a data race is
//! undefined behaviour whatever the hardware underneath would have done. There is no
//! sound alternative that keeps the machine's shape. A byte-wise atomic backing store
//! turns an eight-byte load into eight loads and shifts, and a word-wise one makes
//! every sub-word store a read-modify-write and takes away the contiguous `&[u8]` that
//! a display's framebuffer and a disk's transfer both want. Every emulator that runs
//! guests on host threads makes this same trade.
//!
//! What keeps it honest is that the bytes are only ever reached through the checked
//! accessors of whatever owns them, and that what a racing access means is the
//! guest's own business: RVWMO orders guest accesses with `fence` and the atomics, and
//! a guest that races without them may read whatever it likes, which is what a real
//! machine gives it too. A host frontend reading video memory while a hart writes it
//! is the same bargain a real card makes with a real scanout engine: it sees a torn
//! frame, and the next frame fixes it.

use std::{cell::UnsafeCell, fmt, slice};

/// A flat array of bytes, shared.
pub struct Bytes {
    bytes: Box<[UnsafeCell<u8>]>,
}

/// Sound only under the contract above: the bytes are shared, whatever orders them is
/// outside this type, and nothing hands out a reference that outlives an access.
/// `Send` needs no such claim, since a `u8` behind an `UnsafeCell` already is one.
unsafe impl Sync for Bytes {}

impl Bytes {
    /// `size` bytes, zeroed, with `head` placed at the front of them.
    pub fn new(head: Vec<u8>, size: u64) -> Self {
        let mut bytes = vec![0u8; size as usize];
        bytes.splice(..head.len(), head);
        // `UnsafeCell<u8>` is `repr(transparent)` over `u8`, so this is the same
        // allocation seen as the same bytes, and the zeroed pages the allocator handed
        // over stay untouched rather than being copied into a second buffer.
        let bytes = Box::into_raw(bytes.into_boxed_slice()) as *mut [UnsafeCell<u8>];

        Self {
            bytes: unsafe { Box::from_raw(bytes) },
        }
    }

    /// How many bytes there are.
    #[inline]
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The first byte, as a pointer. Every access starts here.
    ///
    /// Writing through it is what `UnsafeCell` is for: a shared reference to one is
    /// permission to mutate what is inside.
    #[inline]
    fn base(&self) -> *mut u8 {
        self.bytes.as_ptr().cast::<u8>().cast_mut()
    }

    /// Every byte, for a caller that needs them contiguous: a device transferring to
    /// or from a window of guest memory, or a frontend presenting a framebuffer.
    ///
    /// # Safety
    ///
    /// The bytes are shared with whatever else is running, so what the reference says
    /// is only meaningful to a caller that knows what else may be writing them.
    #[inline]
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.base(), self.bytes.len()) }
    }

    /// Every byte, exclusively. `&mut self` is exclusive access to the whole array, so
    /// this is the one path that may hold a slice of it: nothing can be racing with a
    /// caller that has one.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.base(), self.bytes.len()) }
    }

    /// The `N` bytes at `index`, which the caller has already found to be inside.
    #[inline]
    pub fn read<const N: usize>(&self, index: usize) -> [u8; N] {
        debug_assert!(index + N <= self.bytes.len(), "a read past the end");
        unsafe { self.base().add(index).cast::<[u8; N]>().read_unaligned() }
    }

    /// Put `bytes` at `index`, which the caller has already found to be inside.
    #[inline]
    pub fn write<const N: usize>(&self, index: usize, bytes: [u8; N]) {
        debug_assert!(index + N <= self.bytes.len(), "a write past the end");
        unsafe {
            self.base()
                .add(index)
                .cast::<[u8; N]>()
                .write_unaligned(bytes)
        }
    }

    /// Where `index` is, for a caller that needs the host's own atomics to reach it.
    ///
    /// # Safety
    ///
    /// The caller has already found `index` to be inside, and owes whatever alignment
    /// the access it is about to make needs.
    #[inline]
    pub unsafe fn at(&self, index: usize) -> *mut u8 {
        debug_assert!(index < self.bytes.len(), "an access past the end");
        unsafe { self.base().add(index) }
    }

    /// Read `size` bits at `index`.
    ///
    /// The width is a constant at almost every call site, so each arm folds to the one
    /// load it describes: an array of bytes is aligned to one, so the read answers for
    /// an address the guest did not align and still becomes a single move.
    #[inline]
    pub fn load(&self, index: usize, size: u64) -> u64 {
        match size {
            8 => u8::from_le_bytes(self.read(index)) as u64,
            16 => u16::from_le_bytes(self.read(index)) as u64,
            32 => u32::from_le_bytes(self.read(index)) as u64,
            64 => u64::from_le_bytes(self.read(index)),
            _ => unreachable!("load of {size} bits"),
        }
    }

    /// Write the low `size` bits of `value` at `index`.
    #[inline]
    pub fn store(&self, index: usize, size: u64, value: u64) {
        match size {
            8 => self.write(index, (value as u8).to_le_bytes()),
            16 => self.write(index, (value as u16).to_le_bytes()),
            32 => self.write(index, (value as u32).to_le_bytes()),
            64 => self.write(index, value.to_le_bytes()),
            _ => unreachable!("store of {size} bits"),
        }
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bytes").field("len", &self.len()).finish()
    }
}
