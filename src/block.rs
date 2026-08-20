//! The instructions already decoded, kept in the runs they were decoded as.
//!
//! Decoding the same word over and over is most of what an interpreter does inside a
//! loop, and the answer cannot change while the bytes do not. What is kept is not one
//! instruction but the whole straight-line run starting at an address a jump can land
//! on, so that translating, finding the run and deciding where it ends all happen once
//! for as many instructions as the run holds rather than once each.
//!
//! RISC-V does not guarantee that a store to instruction memory is visible to
//! instruction fetch until the hart executes a `fence.i`, so that instruction, and only
//! that instruction, empties this. Emptying is an increment of `epoch`: a run whose
//! epoch is not the current one is not there, whatever address it says it starts at.
//! An epoch has to be a field of its own rather than spare bits of the address, since a
//! physical address here can be thirty-four page numbers wide and leaves none.
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

impl Decoded {
    /// A slot of a run that holds nothing. Never executed: a run says how many of its
    /// instructions are its own, and the rest are only there to be written over.
    pub const NONE: Self = Self {
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
    };
}

/// The most instructions one run can hold.
///
/// A run ends where control might not continue into the next instruction, which for
/// compiled code is every handful of them, so a bound this size truncates few runs and
/// the ones it does truncate cost only a lookup where the next one starts. It is also
/// what each slot reserves room for, so it is what decides how much of the table is
/// occupied by runs shorter than it.
pub const LENGTH: usize = 16;

/// How many runs are kept.
///
/// Chosen by measuring a Linux boot, the workload with a code footprint worth speaking
/// of. Two thousand runs cost 26.0 seconds; four thousand reach 24.7 and eight thousand
/// 24.5, which a boot varying by 4% cannot tell apart, so this is the smaller of the
/// two. A run of thirty-two instead of sixteen instructions is likewise within the
/// noise, and costs twice the memory to say it.
const RUNS: usize = 4096;

/// A run of decoded instructions: where its first one is, and how many follow it.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    pub start: usize,
    pub len: usize,
}

/// What a slot holds: the physical address the run in it starts at, which generation of
/// the table it belongs to, and how many of the slot's instructions are its own.
#[derive(Debug, Clone, Copy)]
struct Header {
    pa: u64,
    epoch: u32,
    len: u32,
}

impl Header {
    /// A slot holding nothing, which is what an epoch of zero means: the table itself
    /// never runs at that epoch.
    const NONE: Self = Self {
        pa: 0,
        epoch: 0,
        len: 0,
    };
}

/// A direct-mapped cache of runs.
///
/// Keyed by physical address rather than virtual, which is what lets it survive a
/// change of address space: the fetch translates the first instruction of a run either
/// way, so permissions, `satp` and the mode it is running in are all still checked, and
/// `sfence.vma` has nothing to say about what is in here. A run never crosses the page
/// it starts in, so one translation answers for all of it.
#[derive(Debug)]
pub struct Blocks {
    /// What is in each slot, apart from the instructions themselves. Kept away from
    /// them so that a lookup that misses reads one dense array rather than striding
    /// through the instructions it did not want.
    headers: Box<[Header; RUNS]>,
    /// Every slot's instructions, each slot's `LENGTH` of them together and in order,
    /// so that running one is a walk along an array.
    ///
    /// Arrays rather than slices, so that masking an index to the length is a proof it
    /// is in range: against a slice the length is a value to be loaded and compared
    /// against on every instruction.
    insts: Box<[Decoded; RUNS * LENGTH]>,
    /// Which generation of the table is current. A `fence.i` moves it on, which is what
    /// makes emptying the table an increment rather than half a mebibyte of writes.
    epoch: u32,
}

impl Default for Blocks {
    fn default() -> Self {
        Self {
            headers: vec![Header::NONE; RUNS]
                .into_boxed_slice()
                .try_into()
                .expect("RUNS headers"),
            insts: vec![Decoded::NONE; RUNS * LENGTH]
                .into_boxed_slice()
                .try_into()
                .expect("RUNS runs of LENGTH"),
            epoch: 1,
        }
    }
}

impl Blocks {
    /// Every instruction is two-byte aligned, so the bit below that carries nothing.
    #[inline]
    const fn slot(pa: u64) -> usize {
        (pa >> 1) as usize & (RUNS - 1)
    }

    /// The run starting at `pa`, if this table holds it.
    #[inline]
    pub fn get(&self, pa: u64) -> Option<Block> {
        let slot = Self::slot(pa);
        let header = self.headers[slot];
        if header.pa != pa || header.epoch != self.epoch {
            return None;
        }
        Some(Block {
            start: slot * LENGTH,
            len: header.len as usize,
        })
    }

    /// Keep `insts` as the run starting at `pa`, over whatever was in its slot.
    pub fn insert(&mut self, pa: u64, insts: &[Decoded]) -> Block {
        let slot = Self::slot(pa);
        let start = slot * LENGTH;
        self.insts[start..start + insts.len()].copy_from_slice(insts);
        self.headers[slot] = Header {
            pa,
            epoch: self.epoch,
            len: insts.len() as u32,
        };
        Block {
            start,
            len: insts.len(),
        }
    }

    /// The instruction at an index of a run.
    #[inline]
    pub fn at(&self, index: usize) -> Decoded {
        self.insts[index & (RUNS * LENGTH - 1)]
    }

    /// Forget everything, which is what `fence.i` means.
    ///
    /// The generation moves on and the table is left alone. Only a wrap has to touch
    /// it, so that an entry left over from four billion flushes ago cannot be mistaken
    /// for one of this generation's.
    pub fn flush(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.headers.fill(Header::NONE);
            self.epoch = 1;
        }
    }
}

/// Whether a run ends after this instruction: one that may transfer control somewhere
/// other than the instruction after it, or one after which what the instructions of a
/// run were decoded under may no longer hold.
///
/// A branch ends a run whether or not it is taken, since the run is decided once and
/// the branch is not. A CSR access ends one because `satp` decides what the addresses
/// of a run mean and `mcountinhibit` and the counters decide what its bookkeeping is
/// worth; a fence because it is what says the bytes behind a run may have changed; and
/// `wfi` because a hart that parks resumes through whatever ran while it was parked.
#[inline]
pub const fn ends(op: Op) -> bool {
    matches!(
        op,
        Op::Jal
            | Op::Jalr
            | Op::Branch { .. }
            | Op::Csrrw { .. }
            | Op::Csrrs { .. }
            | Op::Csrrc { .. }
            | Op::Ecall
            | Op::Ebreak
            | Op::Mret
            | Op::Sret
            | Op::SfenceVma
            | Op::FenceI
            | Op::Wfi
    )
}
