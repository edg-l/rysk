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
    /// What this instruction owes `mcycle`, which is one for each instruction it is:
    /// two where it is a fused pair, one otherwise.
    pub cycles: u8,
    /// And what it owes `minstret`, which is the same, or nothing at all where the
    /// instruction names that counter and so says what it holds rather than counting
    /// itself. Two numbers rather than a count and a flag, because both are properties
    /// of the encoding and the flag would be one more thing the run loop has to keep
    /// hold of across the instruction it is executing.
    /// The RISC-V Instruction Set Manual Volume II, 3.3.1.
    pub instret: u8,
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
        cycles: 1,
        instret: 1,
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

/// The one instruction a pair of them does the work of, where there is one.
///
/// Fusion is worth what a whole turn of the run loop costs: a dispatch, the counters,
/// the interrupt check and the write of `pc`. It happens once, where a block is built,
/// rather than every time the pair runs.
///
/// The pairs here are the ones a compiler emits to make a constant, an address or a
/// call, and every one of them collapses into an instruction this machine already has,
/// so none of them adds an arm to `execute`. What makes that possible is that the value
/// in between is one nothing can read: the second instruction writes over the register
/// the first wrote, and nothing can look at a hart between the two, since a fused pair
/// is one instruction to the run loop and an interrupt is offered before it or after it.
///
/// The RISC-V Instruction Set Manual Volume I, 2.4 and 2.5, which name these as the
/// sequences an implementation is expected to fuse.
pub fn fuse(first: Decoded, second: Decoded) -> Option<Decoded> {
    let (a, b) = (first.inst, second.inst);
    // `x0` is never written, so a pair that names it as the register in between is two
    // instructions reading zero rather than one passing a value along.
    if a.rd == 0 || b.rd != a.rd || b.rs1 != a.rd || first.cycles != 1 {
        return None;
    }
    // A pair of compressed instructions is not taken, so that a fused pair is always
    // longer than four bytes. Nothing needs that today; it is what keeps the door open
    // to deriving how much a pair retires from how long it is.
    if first.length + second.length <= 4 {
        return None;
    }
    let op = match (a.op, b.op) {
        // A constant, in the two halves the encoding makes of it.
        (Op::Lui, Op::Addi) => Op::Addi,
        // The same, narrowed to what fits in thirty-two bits and sign-extended back.
        (Op::Lui, Op::Addiw) => Op::Addi,
        // An address relative to this instruction.
        (Op::Auipc, Op::Addi) => Op::Auipc,
        // A call to one: the link the `jalr` writes is over the address the `auipc`
        // made, and `jal` is the same instruction with the offset already in it.
        (Op::Auipc, Op::Jalr) => Op::Jal,
        _ => return None,
    };
    let sum = a.imm.wrapping_add(b.imm);
    let imm = match (a.op, b.op) {
        (Op::Lui, Op::Addiw) => sum as i32 as i64 as u64,
        // `jalr` clears the low bit of the address it computes, and `jal` does not. The
        // two are the same for an even `pc`, which is the only kind there is, so the
        // bit comes off the offset here instead.
        (Op::Auipc, Op::Jalr) => sum & !1,
        _ => sum,
    };
    Some(Decoded {
        inst: Inst {
            op,
            rd: a.rd,
            rs1: 0,
            rs2: 0,
            imm,
        },
        // The pair raises nothing that owes `mtval` an encoding: the sum of two
        // immediates cannot be misaligned, and none of the three can fault.
        encoding: first.encoding,
        length: first.length + second.length,
        cycles: 2,
        instret: 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inst::decode;

    /// What a run holds for one encoding, as the builder would decode it.
    fn one(encoding: u32) -> Decoded {
        Decoded {
            inst: decode(encoding).expect("a valid encoding"),
            encoding,
            length: 4,
            cycles: 1,
            instret: 1,
        }
    }

    /// `lui t0, 0x12345` then `addi t0, t0, 0x678`.
    const LUI: u32 = 0x1234_52b7;
    const ADDI: u32 = 0x6782_8293;
    /// `auipc t0, 0x12345` and `jalr t0, t0, 0x679`, over the same register. The odd
    /// offset is what `jalr` clears the low bit of.
    const AUIPC: u32 = 0x1234_5297;
    const JALR: u32 = 0x6792_82e7;

    #[test]
    fn a_pair_becomes_the_one_instruction_it_adds_up_to() {
        let fused = fuse(one(LUI), one(ADDI)).expect("lui and addi fuse");
        assert_eq!(fused.inst.op, Op::Addi);
        assert_eq!(fused.inst.rs1, 0, "the constant comes from nowhere else");
        assert_eq!(fused.inst.imm, 0x1234_5678);
        assert_eq!(fused.length, 8, "and it is as long as the two of them");
        assert_eq!(fused.instret, 2, "and counts as the two of them");

        let fused = fuse(one(AUIPC), one(JALR)).expect("auipc and jalr fuse");
        assert_eq!(fused.inst.op, Op::Jal);
        assert_eq!(fused.inst.imm, 0x1234_5678, "with the low bit taken off");
    }

    #[test]
    fn a_pair_that_is_not_one_instruction_is_left_alone() {
        // The second writes somewhere else, so what the first wrote is still wanted.
        let elsewhere = ADDI | (6 << 7);
        assert!(fuse(one(LUI), one(elsewhere)).is_none());
        // The second reads somewhere else, so it is not the pair's own value.
        let other_source = (ADDI & !(0x1f << 15)) | (6 << 15);
        assert!(fuse(one(LUI), one(other_source)).is_none());
        // Nothing passes through `x0`.
        let through_zero = (LUI & !(0x1f << 7), ADDI & !(0x1f << 7) & !(0x1f << 15));
        assert!(fuse(one(through_zero.0), one(through_zero.1)).is_none());
        // And a fused pair does not take a third instruction into itself.
        let already = fuse(one(LUI), one(ADDI)).expect("lui and addi fuse");
        assert!(fuse(already, one(ADDI)).is_none());
    }
}
