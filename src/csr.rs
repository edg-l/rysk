//! Control and status registers: the numbers rysk knows, and the layout of the
//! fields the trap machinery reads.
//!
//! The RISC-V Instruction Set Manual Volume II, chapter 2 and 3.1.

pub const MSTATUS: usize = 0x300;
/// Bit positions in `mstatus`: the machine interrupt-enable bit, the value it had
/// before the current trap, and the two-bit field holding the mode the trap came from.
/// The RISC-V Instruction Set Manual Volume II, 3.1.6.
pub const MSTATUS_MIE: u64 = 3;
pub const MSTATUS_MPIE: u64 = 7;
pub const MSTATUS_MPP: u64 = 0b11 << 11;
/// Machine mode in the two-bit `MPP` encoding.
pub const MSTATUS_MPP_M: u64 = 0b11 << 11;
pub const MTVEC: usize = 0x305;
pub const MEPC: usize = 0x341;
pub const MCAUSE: usize = 0x342;
pub const MTVAL: usize = 0x343;
pub const MIP: usize = 0x344;
pub const MIE: usize = 0x304;
pub const SIP: usize = 0x144;
pub const SIE: usize = 0x104;
pub const MEDELEG: usize = 0x302;
pub const MIDELEG: usize = 0x303;
pub const RDCYCLE: usize = 0xC00;
pub const RDTIME: usize = 0xC01;
pub const INSTRET: usize = 0xC02;
