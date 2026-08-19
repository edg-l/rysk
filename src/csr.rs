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
pub const MSTATUS_MPP: u64 = 0b11 << 11;
/// Machine mode in the two-bit `MPP` encoding.
pub const MSTATUS_MPP_M: u64 = 0b11 << 11;
/// Bit positions in `mstatus` that belong to the supervisor: its own interrupt-enable
/// stack, the mode it trapped from, and the two controls over how it may reach user
/// memory. The RISC-V Instruction Set Manual Volume II, 12.1.1.
pub const MSTATUS_SIE: u64 = 1;
pub const MSTATUS_SPIE: u64 = 5;
pub const MSTATUS_SPP: u64 = 8;
pub const MSTATUS_SUM: u64 = 18;
pub const MSTATUS_MXR: u64 = 19;
/// The fields of `mstatus` that `sstatus` exposes. `FS`, `VS`, `XS` and the `SD` that
/// summarises them are absent because rysk has no floating-point or vector state, so
/// they are read-only zero, and `UXL` because a machine that implements a single width
/// reports it rather than taking a write.
pub const SSTATUS_MASK: u64 = (1 << MSTATUS_SIE)
    | (1 << MSTATUS_SPIE)
    | (1 << MSTATUS_SPP)
    | (1 << MSTATUS_SUM)
    | (1 << MSTATUS_MXR);
pub const MTVEC: usize = 0x305;
pub const MEPC: usize = 0x341;
pub const MCAUSE: usize = 0x342;
pub const MTVAL: usize = 0x343;
pub const MIP: usize = 0x344;
pub const MIE: usize = 0x304;
pub const SSTATUS: usize = 0x100;
pub const SIP: usize = 0x144;
pub const SIE: usize = 0x104;
pub const MEDELEG: usize = 0x302;
pub const MIDELEG: usize = 0x303;
/// The interrupts a supervisor can be given: software, timer and external at its own
/// level. The machine-level bits are never delegatable, and the counter-overflow one
/// belongs to `Sscofpmf`, which rysk does not implement.
/// The RISC-V Instruction Set Manual Volume II, 12.1.3, figure 55.
pub const S_INTERRUPTS: u64 = (1 << 1) | (1 << 5) | (1 << 9);
pub const RDCYCLE: usize = 0xC00;
pub const RDTIME: usize = 0xC01;
pub const INSTRET: usize = 0xC02;

/// The supervisor CSRs that are a view of a machine one, as the register that backs
/// them and the bits they expose. Reading or writing one of these reads or writes the
/// homonymous field of the machine register; none of them is storage of its own.
/// The RISC-V Instruction Set Manual Volume II, 12.1.1 and 12.1.3.
pub fn alias(addr: usize, mideleg: u64) -> Option<(usize, u64)> {
    match addr {
        SSTATUS => Some((MSTATUS, SSTATUS_MASK)),
        SIE => Some((MIE, mideleg)),
        SIP => Some((MIP, mideleg)),
        _ => None,
    }
}
