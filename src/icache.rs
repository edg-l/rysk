//! The instructions already decoded, kept by the address they were fetched from.
//!
//! Decoding the same word over and over is most of what an interpreter does inside a
//! loop, and the answer cannot change while the bytes do not. RISC-V does not
//! guarantee that a store to instruction memory is visible to instruction fetch until
//! the hart executes a `fence.i`, so that instruction, and only that instruction,
//! empties this.
//!
//! The RISC-V Instruction Set Manual Volume I, 5.

use crate::inst::Inst;

/// What a fetch produces: what the instruction does, the bits it was, and how many
/// bytes of them. The encoding is kept because a trap that rejects an instruction owes
/// `mtval` the bits it was given, and the length because the fetch already knows it and
/// the next `pc` is otherwise derived from the encoding all over again.
#[derive(Debug, Clone, Copy)]
pub struct Decoded {
    pub inst: Inst,
    pub encoding: u32,
    pub length: u8,
    /// Whether this instruction names the retired-instruction counter, and so says
    /// what it holds rather than counting itself. It is a property of the encoding, so
    /// it is answered once here rather than asked of every instruction that retires.
    /// The RISC-V Instruction Set Manual Volume II, 3.3.1.
    pub writes_instret: bool,
}

/// One of them, and the physical address whose bytes it came from.
#[derive(Debug, Clone, Copy)]
struct Entry {
    pa: u64,
    decoded: Decoded,
}

/// Enough entries to hold a page of compressed instructions, and small enough that the
/// whole table stays in the cache the host has for it.
const SIZE: usize = 2048;

/// A direct-mapped cache of them.
///
/// Keyed by physical address rather than virtual, which is what lets it survive a
/// change of address space: the fetch translates first either way, so permissions,
/// `satp` and the mode it is running in are all still checked on every instruction,
/// and `sfence.vma` has nothing to say about what is in here.
#[derive(Debug)]
pub struct Icache {
    /// An array rather than a slice, so that masking the index to its length is a
    /// proof the index is in range: against a slice the length is a value to be loaded
    /// and compared against on every fetch.
    entries: Box<[Option<Entry>; SIZE]>,
}

impl Default for Icache {
    fn default() -> Self {
        Self {
            entries: vec![None; SIZE]
                .into_boxed_slice()
                .try_into()
                .expect("SIZE entries"),
        }
    }
}

impl Icache {
    /// Every instruction is two-byte aligned, so the bit below that carries nothing.
    #[inline]
    const fn slot(pa: u64) -> usize {
        (pa >> 1) as usize & (SIZE - 1)
    }

    #[inline]
    pub fn get(&self, pa: u64) -> Option<Decoded> {
        self.entries[Self::slot(pa)]
            .filter(|entry| entry.pa == pa)
            .map(|entry| entry.decoded)
    }

    #[inline]
    pub fn insert(&mut self, pa: u64, decoded: Decoded) {
        self.entries[Self::slot(pa)] = Some(Entry { pa, decoded });
    }

    /// Forget everything, which is what `fence.i` means.
    pub fn flush(&mut self) {
        self.entries.fill(None);
    }
}
