//! The instructions already decoded, kept by the address they were fetched from.
//!
//! Decoding the same word over and over is most of what an interpreter does inside a
//! loop, and the answer cannot change while the bytes do not. RISC-V does not
//! guarantee that a store to instruction memory is visible to instruction fetch until
//! the hart executes a `fence.i`, so that instruction, and only that instruction,
//! empties this.
//!
//! The RISC-V Instruction Set Manual Volume I, 5.

use crate::inst::{Inst, Op};

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
///
/// There is no `Option` around this. Every instruction is two-byte aligned, so an odd
/// address is one no fetch can ask for, and an entry holding one is an entry holding
/// nothing. That keeps a probe to a load and a compare, where an `Option` made it a
/// tag to test and a copy of the whole entry through a `filter` and a `map`.
#[derive(Debug, Clone, Copy)]
struct Entry {
    pa: u64,
    decoded: Decoded,
}

/// The address of an entry that holds nothing, which is one no instruction can be at.
const EMPTY: u64 = u64::MAX;

/// How many instructions are kept. Each entry answers for one address, so the table
/// covers `SIZE * 2` bytes of code at a time.
///
/// Chosen by measuring a Linux boot, which is the workload with a code footprint worth
/// speaking of. Two thousand entries cover a single page and cost 29.9 seconds; the
/// time falls to about 27.5 by sixteen thousand and does not fall again, and a hundred
/// and thirty thousand is slower than either. Sixteen and thirty-two thousand cannot be
/// told apart, so this is the smaller of the two: half a mebibyte a hart rather than a
/// whole one, and half as much for `fence.i` to clear.
///
/// The number that used to be here was two thousand, on the reasoning that the table
/// should fit the cache the host has for it. The measurement says otherwise, and the
/// reason is that most of a big table is cold: what it buys is decodes not repeated,
/// and what it costs is misses on lines that were not going to be hit anyway.
const SIZE: usize = 16384;

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
    entries: Box<[Entry; SIZE]>,
}

impl Entry {
    /// An entry holding nothing. What makes it empty is its address, which no fetch
    /// can ask for; the instruction beside it is never read and is a `nop` so that it
    /// is something rather than a hole to reason about.
    const NONE: Self = Self {
        pa: EMPTY,
        decoded: Decoded {
            inst: Inst {
                op: Op::Addi,
                rd: 0,
                rs1: 0,
                rs2: 0,
                imm: 0,
            },
            encoding: 0,
            length: 0,
            writes_instret: false,
        },
    };
}

impl Default for Icache {
    fn default() -> Self {
        Self {
            entries: vec![Entry::NONE; SIZE]
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
        let entry = self.entries[Self::slot(pa)];
        (entry.pa == pa).then_some(entry.decoded)
    }

    #[inline]
    pub fn insert(&mut self, pa: u64, decoded: Decoded) {
        self.entries[Self::slot(pa)] = Entry { pa, decoded };
    }

    /// Forget everything, which is what `fence.i` means.
    pub fn flush(&mut self) {
        self.entries.fill(Entry::NONE);
    }
}
