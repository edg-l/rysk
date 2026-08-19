//! Control and status registers: the numbers rysk knows, and the layout of the
//! fields the trap machinery reads.
//!
//! The RISC-V Instruction Set Manual Volume II, chapter 2 and 3.1.

pub const MISA: usize = 0x301;
/// `misa` reports the width of the machine in its top two bits and the extensions it
/// implements in the low twenty-six, one bit per letter of the alphabet.
/// The RISC-V Instruction Set Manual Volume II, 3.1.1.
pub const MISA_MXL_64: u64 = 2 << 62;
pub const fn misa_extension(letter: u8) -> u64 {
    1 << (letter - b'a')
}

pub const MSTATUS: usize = 0x300;
/// Bit positions in `mstatus`: the machine interrupt-enable bit, the value it had
/// before the current trap, and the two-bit field holding the mode the trap came from.
/// The RISC-V Instruction Set Manual Volume II, 3.1.6.
pub const MSTATUS_MIE: u64 = 3;
pub const MSTATUS_MPIE: u64 = 7;
pub const MSTATUS_MPP_SHIFT: u64 = 11;
pub const MSTATUS_MPP: u64 = 0b11 << MSTATUS_MPP_SHIFT;
/// Bit positions in `mstatus` that belong to the supervisor: its own interrupt-enable
/// stack, the mode it trapped from, and the two controls over how it may reach user
/// memory. The RISC-V Instruction Set Manual Volume II, 12.1.1.
pub const MSTATUS_SIE: u64 = 1;
pub const MSTATUS_SPIE: u64 = 5;
pub const MSTATUS_SPP: u64 = 8;
pub const MSTATUS_SUM: u64 = 18;
pub const MSTATUS_MXR: u64 = 19;
/// When set, a load or store is translated and checked as if the hart were in the mode
/// `MPP` names. An instruction fetch is not affected.
/// The RISC-V Instruction Set Manual Volume II, 3.1.6.3.
pub const MSTATUS_MPRV: u64 = 17;
/// The three bits that take a supervisor's privileges away one at a time, so that a
/// machine or a hypervisor underneath it can see what it was about to do: `TVM`
/// intercepts the page table, `TW` the idle loop, and `TSR` the return from a trap.
/// The RISC-V Instruction Set Manual Volume II, 3.1.6.5.
pub const MSTATUS_TVM: u64 = 20;
pub const MSTATUS_TW: u64 = 21;
pub const MSTATUS_TSR: u64 = 22;
/// The fields of `mstatus` that `sstatus` exposes. `FS`, `VS`, `XS` and the `SD` that
/// summarises them are absent because rysk has no floating-point or vector state, so
/// they are read-only zero, and `UXL` because a machine that implements a single width
/// reports it rather than taking a write.
pub const SSTATUS_MASK: u64 = (1 << MSTATUS_SIE)
    | (1 << MSTATUS_SPIE)
    | (1 << MSTATUS_SPP)
    | (1 << MSTATUS_SUM)
    | (1 << MSTATUS_MXR);
/// The width of the register file a supervisor and a user program see. Both are WARL
/// over the widths the machine supports, and rysk supports one, so both read as 64 and
/// ignore a write. The RISC-V Instruction Set Manual Volume II, 3.1.6.3.
pub const MSTATUS_UXL: u64 = 0b11 << 32;
pub const MSTATUS_SXL: u64 = 0b11 << 34;
pub const MSTATUS_XL_64: u64 = (2 << 32) | (2 << 34);
pub const MTVEC: usize = 0x305;
pub const MEPC: usize = 0x341;
pub const MCAUSE: usize = 0x342;
pub const MTVAL: usize = 0x343;
pub const MIP: usize = 0x344;
pub const MIE: usize = 0x304;
pub const SSTATUS: usize = 0x100;
pub const SIE: usize = 0x104;
pub const STVEC: usize = 0x105;
pub const SSCRATCH: usize = 0x140;
pub const SEPC: usize = 0x141;
pub const SCAUSE: usize = 0x142;
pub const STVAL: usize = 0x143;
pub const SIP: usize = 0x144;
pub const SATP: usize = 0x180;
pub const MEDELEG: usize = 0x302;
pub const MIDELEG: usize = 0x303;
/// The interrupt bits of `mip` and `mie`, which share their layout with `mcause`'s
/// interrupt causes. The RISC-V Instruction Set Manual Volume II, 3.1.9, figure 15.
pub const SSIP: u64 = 1 << 1;
pub const MSIP: u64 = 1 << 3;
pub const STIP: u64 = 1 << 5;
pub const MTIP: u64 = 1 << 7;
pub const SEIP: u64 = 1 << 9;
pub const MEIP: u64 = 1 << 11;
/// The enable bits of `mie` share the pending bits' layout, so they are the same
/// numbers under names that read correctly at a call site.
pub const SSIE: u64 = SSIP;
pub const MSIE: u64 = MSIP;
pub const STIE: u64 = STIP;
pub const MTIE: u64 = MTIP;
pub const SEIE: u64 = SEIP;
pub const MEIE: u64 = MEIP;
/// The interrupts a supervisor can be given: software, timer and external at its own
/// level. The machine-level bits are never delegatable, and the counter-overflow one
/// belongs to `Sscofpmf`, which rysk does not implement.
/// The RISC-V Instruction Set Manual Volume II, 12.1.3, figure 55.
pub const S_INTERRUPTS: u64 = SSIP | STIP | SEIP;
/// The pending bits a device drives rather than software. They are read-only in `mip`
/// and follow whatever the device is doing, so clearing one means acting on the
/// device: moving a deadline, or completing a claim.
/// The RISC-V Instruction Set Manual Volume II, 3.1.9.
pub const MIP_DEVICE: u64 = MSIP | MTIP | SEIP | MEIP;
pub const RDCYCLE: usize = 0xC00;
pub const RDTIME: usize = 0xC01;
pub const INSTRET: usize = 0xC02;

/// The supervisor CSRs that are a view of a machine one, as the register that backs
/// them, the bits a write may change, and the further bits a read may see. Reading or
/// writing one of these reads or writes the homonymous field of the machine register;
/// none of them is storage of its own.
/// The RISC-V Instruction Set Manual Volume II, 12.1.1 and 12.1.3.
pub fn alias(addr: usize, mideleg: u64) -> Option<(usize, u64, u64)> {
    match addr {
        SSTATUS => Some((MSTATUS, SSTATUS_MASK, MSTATUS_UXL)),
        SIE => Some((MIE, mideleg, 0)),
        SIP => Some((MIP, mideleg, 0)),
        _ => None,
    }
}

/// `mstatus` has fields a write does not get to choose: `MPP` is WARL over the modes
/// that exist, and the two width fields report the single width this machine has.
/// The RISC-V Instruction Set Manual Volume II, 3.1.6.
pub fn warl_mstatus(old: u64, new: u64) -> u64 {
    let mpp = if (new & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT == 2 {
        old & MSTATUS_MPP
    } else {
        new & MSTATUS_MPP
    };
    let widths = MSTATUS_UXL | MSTATUS_SXL;
    (new & !MSTATUS_MPP & !widths) | mpp | (old & widths)
}

/// The privilege the hart runs at, in the encoding the `MPP` and `SPP` fields use.
/// The RISC-V Instruction Set Manual Volume II, table 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    User = 0,
    Supervisor = 1,
    Machine = 3,
}

impl Mode {
    /// The mode an `xPP` field names. Two is reserved, and the field is WARL, so a
    /// write that names it never reaches storage.
    pub fn from_bits(bits: u64) -> Self {
        match bits {
            0 => Self::User,
            1 => Self::Supervisor,
            3 => Self::Machine,
            _ => unreachable!("privilege mode {bits} does not exist"),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User => "u-mode",
            Self::Supervisor => "s-mode",
            Self::Machine => "m-mode",
        })
    }
}
