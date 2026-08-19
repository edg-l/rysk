//! Decoding: turning a 32-bit word into something `Cpu::execute` can dispatch on
//! without re-reading bit ranges, and print when a program goes somewhere unexpected.

use std::fmt;

use crate::trap::Exception;

/// The width of a memory access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Byte,
    Half,
    Word,
    Double,
}

impl Width {
    pub const fn bits(self) -> u64 {
        match self {
            Self::Byte => 8,
            Self::Half => 16,
            Self::Word => 32,
            Self::Double => 64,
        }
    }

    const fn suffix(self) -> &'static str {
        match self {
            Self::Byte => "b",
            Self::Half => "h",
            Self::Word => "w",
            Self::Double => "d",
        }
    }
}

/// The comparison a conditional branch makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    Eq,
    Ne,
    Lt,
    Ge,
    Ltu,
    Geu,
}

/// The read-modify-write an atomic memory operation performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmoOp {
    Swap,
    Add,
    Xor,
    And,
    Or,
    Min,
    Max,
    MinU,
    MaxU,
}

/// The widths a compare-and-swap comes in: the ones the bus has, and the quadword,
/// which is two doublewords and a register pair at each end because nothing that
/// reaches the bus is 128 bits wide.
///
/// The RISC-V Instruction Set Manual Volume I, 15.1 and 16.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasWidth {
    Narrow(Width),
    Quad,
}

impl CasWidth {
    const fn suffix(self) -> &'static str {
        match self {
            Self::Narrow(width) => width.suffix(),
            Self::Quad => "q",
        }
    }
}

/// What a floating-point instruction does, with the format it does it in carried
/// beside it rather than doubling this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FpOp {
    Add,
    Sub,
    Mul,
    Div,
    Sqrt,
    Min,
    Max,
    /// The sign of one operand on the rest of another.
    SignJoin {
        negate: bool,
        xor: bool,
    },
    Equal,
    Less,
    LessEqual,
    Classify,
    /// To the other floating format.
    Convert,
    ToInteger {
        bits: u32,
        signed: bool,
    },
    FromInteger {
        bits: u32,
        signed: bool,
    },
    /// The bits themselves, between register files, uninterpreted.
    MoveOut,
    MoveIn,
}

/// What an instruction does. The operands live in [`Inst`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    // rd = rs1 op imm
    Addi,
    Slti,
    Sltiu,
    Xori,
    Ori,
    Andi,
    Slli,
    Srli,
    Srai,
    Addiw,
    Slliw,
    Srliw,
    Sraiw,
    // rd = rs1 op rs2
    Add,
    Sub,
    Sll,
    Slt,
    Sltu,
    Xor,
    Srl,
    Sra,
    Or,
    And,
    Addw,
    Subw,
    Sllw,
    Srlw,
    Sraw,
    Mul,
    Mulh,
    Mulhsu,
    Mulhu,
    Div,
    Divu,
    Rem,
    Remu,
    Mulw,
    Divw,
    Divuw,
    Remw,
    Remuw,
    CzeroEqz,
    CzeroNez,
    // memory
    Load {
        width: Width,
        signed: bool,
    },
    Store {
        width: Width,
    },
    // control transfer
    Lui,
    Auipc,
    Jal,
    Jalr,
    Branch {
        cond: Cond,
    },
    // control and status registers. The immediate forms take their source from the
    // rs1 field as a five-bit unsigned value rather than from the register.
    Csrrw {
        immediate: bool,
    },
    Csrrs {
        immediate: bool,
    },
    Csrrc {
        immediate: bool,
    },
    // ordering. A single in-order hart with no caches is already ordered, so both
    // retire without doing anything.
    Fence,
    FenceI,
    // privileged
    Ecall,
    Ebreak,
    Mret,
    Sret,
    Wrs {
        timeout: bool,
    },
    SfenceVma,
    Wfi,
    // atomics
    Lr {
        width: Width,
    },
    Sc {
        width: Width,
    },
    Amo {
        op: AmoOp,
        width: Width,
    },
    AmoCas {
        width: CasWidth,
    },
    FpLoad {
        double: bool,
    },
    FpStore {
        double: bool,
    },
    Fp {
        op: FpOp,
        double: bool,
    },
    /// `rs1 * rs2 + rs3`, rounded once, with either term optionally turned over: the
    /// four instructions are one operation and two bits of the opcode.
    FpFused {
        negate_product: bool,
        negate_addend: bool,
        double: bool,
    },
}

/// A decoded instruction. Which of the operands mean anything depends on `op`: `imm`
/// carries a sign-extended immediate, a shift amount, a csr number, or for a
/// floating-point instruction its rounding mode and, for a fused one, `rs3` above
/// that, since only those three instructions have a third source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inst {
    pub op: Op,
    pub rd: usize,
    pub rs1: usize,
    pub rs2: usize,
    pub imm: u64,
}

/// imm[11:0] = inst[31:20], sign-extended.
const fn i_imm(inst: u32) -> u64 {
    ((inst as i32 as i64) >> 20) as u64
}

/// imm[11:5|4:0] = inst[31:25|11:7], sign-extended.
const fn s_imm(inst: u32) -> u64 {
    (((inst & 0xfe00_0000) as i32 as i64 >> 20) as u64) | ((inst >> 7) & 0x1f) as u64
}

/// imm[12|10:5|4:1|11] = inst[31|30:25|11:8|7], sign-extended.
const fn b_imm(inst: u32) -> u64 {
    (((inst & 0x8000_0000) as i32 as i64 >> 19) as u64)
        | ((inst & 0x80) << 4) as u64
        | ((inst >> 20) & 0x7e0) as u64
        | ((inst >> 7) & 0x1e) as u64
}

/// imm[31:12] = inst[31:12], sign-extended.
const fn u_imm(inst: u32) -> u64 {
    (inst & 0xffff_f000) as i32 as i64 as u64
}

/// imm[20|10:1|11|19:12] = inst[31|30:21|20|19:12], sign-extended.
const fn j_imm(inst: u32) -> u64 {
    (((inst & 0x8000_0000) as i32 as i64 >> 11) as u64)
        | (inst & 0xff000) as u64
        | ((inst >> 9) & 0x800) as u64
        | ((inst >> 20) & 0x7fe) as u64
}

/// The width an atomic's `funct3` names. Byte and halfword are the Zabha extension;
/// the A extension on its own has only the two wider ones.
/// The RISC-V Instruction Set Manual Volume I, 16.1.
const fn amo_width(funct3: u32) -> Option<Width> {
    match funct3 {
        0x0 => Some(Width::Byte),
        0x1 => Some(Width::Half),
        0x2 => Some(Width::Word),
        0x3 => Some(Width::Double),
        _ => None,
    }
}

/// The rounding an instruction asks for, which is its `funct3` unless that names one
/// of the three reserved encodings. Seven means whatever `frm` currently says.
const fn rounding(inst: u32) -> u64 {
    ((inst >> 12) & 0x7) as u64
}

/// How many bytes the instruction beginning with `half` occupies. Everything with its
/// low two bits set is 32 bits wide, and everything else is compressed; the encodings
/// above that are for instructions rysk does not have.
///
/// The RISC-V Instruction Set Manual Volume I, 1.5, table 1.
#[inline]
pub const fn length(half: u16) -> u64 {
    if half & 0b11 == 0b11 { 4 } else { 2 }
}

/// Decode one 32-bit instruction, or reject it as illegal.
#[inline]
pub fn decode(inst: u32) -> Result<Inst, Exception> {
    let opcode = inst & 0x7f;
    let rd = ((inst >> 7) & 0x1f) as usize;
    let rs1 = ((inst >> 15) & 0x1f) as usize;
    let rs2 = ((inst >> 20) & 0x1f) as usize;
    let funct3 = (inst >> 12) & 0x7;
    let funct7 = (inst >> 25) & 0x7f;
    let illegal = Exception::IllegalInstruction(inst as u64);

    let (op, imm) = match opcode {
        0x03 => {
            let width = match funct3 & 0x3 {
                0x0 => Width::Byte,
                0x1 => Width::Half,
                0x2 => Width::Word,
                _ => Width::Double,
            };
            // funct3[2] selects the zero-extending form, and there is no `ldu`
            let signed = funct3 & 0x4 == 0;
            if !signed && width == Width::Double {
                return Err(illegal);
            }
            (Op::Load { width, signed }, i_imm(inst))
        }
        0x23 => {
            let width = match funct3 {
                0x0 => Width::Byte,
                0x1 => Width::Half,
                0x2 => Width::Word,
                0x3 => Width::Double,
                _ => return Err(illegal),
            };
            (Op::Store { width }, s_imm(inst))
        }
        // The immediate shifts spend funct7's low bit on the sixth shift-amount bit,
        // so they are distinguished by the top six.
        0x13 => match (funct3, funct7 >> 1) {
            (0x0, _) => (Op::Addi, i_imm(inst)),
            (0x2, _) => (Op::Slti, i_imm(inst)),
            (0x3, _) => (Op::Sltiu, i_imm(inst)),
            (0x4, _) => (Op::Xori, i_imm(inst)),
            (0x6, _) => (Op::Ori, i_imm(inst)),
            (0x7, _) => (Op::Andi, i_imm(inst)),
            (0x1, 0x00) => (Op::Slli, i_imm(inst) & 0x3f),
            (0x5, 0x00) => (Op::Srli, i_imm(inst) & 0x3f),
            (0x5, 0x10) => (Op::Srai, i_imm(inst) & 0x3f),
            _ => return Err(illegal),
        },
        // The word shifts take five bits, so funct7 is fully determined.
        0x1b => match (funct3, funct7) {
            (0x0, _) => (Op::Addiw, i_imm(inst)),
            (0x1, 0x00) => (Op::Slliw, i_imm(inst) & 0x1f),
            (0x5, 0x00) => (Op::Srliw, i_imm(inst) & 0x1f),
            (0x5, 0x20) => (Op::Sraiw, i_imm(inst) & 0x1f),
            _ => return Err(illegal),
        },
        0x33 => {
            let op = match (funct3, funct7) {
                (0x0, 0x00) => Op::Add,
                (0x0, 0x20) => Op::Sub,
                (0x1, 0x00) => Op::Sll,
                (0x2, 0x00) => Op::Slt,
                (0x3, 0x00) => Op::Sltu,
                (0x4, 0x00) => Op::Xor,
                (0x5, 0x00) => Op::Srl,
                (0x5, 0x20) => Op::Sra,
                (0x6, 0x00) => Op::Or,
                (0x7, 0x00) => Op::And,
                (0x0, 0x01) => Op::Mul,
                (0x1, 0x01) => Op::Mulh,
                (0x2, 0x01) => Op::Mulhsu,
                (0x3, 0x01) => Op::Mulhu,
                (0x4, 0x01) => Op::Div,
                (0x5, 0x01) => Op::Divu,
                (0x6, 0x01) => Op::Rem,
                (0x7, 0x01) => Op::Remu,
                (0x5, 0x07) => Op::CzeroEqz,
                (0x7, 0x07) => Op::CzeroNez,
                _ => return Err(illegal),
            };
            (op, 0)
        }
        0x3b => {
            let op = match (funct3, funct7) {
                (0x0, 0x00) => Op::Addw,
                (0x0, 0x20) => Op::Subw,
                (0x1, 0x00) => Op::Sllw,
                (0x5, 0x00) => Op::Srlw,
                (0x5, 0x20) => Op::Sraw,
                (0x0, 0x01) => Op::Mulw,
                (0x4, 0x01) => Op::Divw,
                (0x5, 0x01) => Op::Divuw,
                (0x6, 0x01) => Op::Remw,
                (0x7, 0x01) => Op::Remuw,
                _ => return Err(illegal),
            };
            (op, 0)
        }
        0x63 => {
            let cond = match funct3 {
                0x0 => Cond::Eq,
                0x1 => Cond::Ne,
                0x4 => Cond::Lt,
                0x5 => Cond::Ge,
                0x6 => Cond::Ltu,
                0x7 => Cond::Geu,
                _ => return Err(illegal),
            };
            (Op::Branch { cond }, b_imm(inst))
        }
        0x0f => match funct3 {
            0x0 => (Op::Fence, 0),
            0x1 => (Op::FenceI, 0),
            _ => return Err(illegal),
        },
        0x37 => (Op::Lui, u_imm(inst)),
        0x17 => (Op::Auipc, u_imm(inst)),
        0x6f => (Op::Jal, j_imm(inst)),
        0x67 if funct3 == 0 => (Op::Jalr, i_imm(inst)),
        0x73 => {
            // The csr number occupies the same bits as an I-immediate, unsigned.
            let csr = ((inst >> 20) & 0xfff) as u64;
            let op = match funct3 {
                // funct3 of zero is not a csr access: the immediate selects a
                // privileged instruction instead.
                0x0 if rd == 0 => match (csr >> 5, rs1) {
                    // sfence.vma is the one that takes operands: an address in rs1 and
                    // an address space in rs2, either of which being x0 means all of
                    // them. So its immediate is not a whole constant like the rest.
                    (0x09, _) => Op::SfenceVma,
                    (_, 0) => match csr {
                        0x000 => Op::Ecall,
                        0x001 => Op::Ebreak,
                        0x302 => Op::Mret,
                        0x102 => Op::Sret,
                        0x105 => Op::Wfi,
                        0x00d => Op::Wrs { timeout: false },
                        0x01d => Op::Wrs { timeout: true },
                        _ => return Err(illegal),
                    },
                    _ => return Err(illegal),
                },
                0x1 => Op::Csrrw { immediate: false },
                0x2 => Op::Csrrs { immediate: false },
                0x3 => Op::Csrrc { immediate: false },
                0x5 => Op::Csrrw { immediate: true },
                0x6 => Op::Csrrs { immediate: true },
                0x7 => Op::Csrrc { immediate: true },
                _ => return Err(illegal),
            };
            (op, csr)
        }
        // The floating-point loads and stores are their own opcodes rather than a
        // width of the integer ones, because they name a different register file.
        0x07 | 0x27 => {
            let double = match funct3 {
                0x2 => false,
                0x3 => true,
                _ => return Err(illegal),
            };
            match opcode {
                0x07 => (Op::FpLoad { double }, i_imm(inst)),
                _ => (Op::FpStore { double }, s_imm(inst)),
            }
        }
        // The four fused forms differ only in which of the three terms is turned over.
        0x43 | 0x47 | 0x4b | 0x4f => {
            let double = match funct7 & 0b11 {
                0b00 => false,
                0b01 => true,
                _ => return Err(illegal),
            };
            let op = Op::FpFused {
                negate_product: opcode & 0b1000 != 0,
                negate_addend: opcode & 0b0100 != 0,
                double,
            };
            (op, rounding(inst) | ((funct7 as u64 >> 2) << 3))
        }
        0x53 => {
            let double = match funct7 & 0b11 {
                0b00 => false,
                0b01 => true,
                _ => return Err(illegal),
            };
            let op = match funct7 >> 2 {
                0b00000 => FpOp::Add,
                0b00001 => FpOp::Sub,
                0b00010 => FpOp::Mul,
                0b00011 => FpOp::Div,
                0b01011 if rs2 == 0 => FpOp::Sqrt,
                0b00100 => match funct3 {
                    0x0 => FpOp::SignJoin {
                        negate: false,
                        xor: false,
                    },
                    0x1 => FpOp::SignJoin {
                        negate: true,
                        xor: false,
                    },
                    0x2 => FpOp::SignJoin {
                        negate: false,
                        xor: true,
                    },
                    _ => return Err(illegal),
                },
                0b00101 => match funct3 {
                    0x0 => FpOp::Min,
                    0x1 => FpOp::Max,
                    _ => return Err(illegal),
                },
                0b10100 => match funct3 {
                    0x0 => FpOp::LessEqual,
                    0x1 => FpOp::Less,
                    0x2 => FpOp::Equal,
                    _ => return Err(illegal),
                },
                // Between the two formats. rs2 names the one being left, and naming
                // the one being entered is not a conversion at all.
                0b01000 => match (rs2, double) {
                    (0b00000, true) | (0b00001, false) => FpOp::Convert,
                    _ => return Err(illegal),
                },
                // rs2 names the integer: its width, and whether it is signed.
                0b11000 | 0b11010 => {
                    let (bits, signed) = match rs2 {
                        0b00000 => (32, true),
                        0b00001 => (32, false),
                        0b00010 => (64, true),
                        0b00011 => (64, false),
                        _ => return Err(illegal),
                    };
                    match funct7 >> 2 {
                        0b11000 => FpOp::ToInteger { bits, signed },
                        _ => FpOp::FromInteger { bits, signed },
                    }
                }
                0b11100 if rs2 == 0 => match funct3 {
                    0x0 => FpOp::MoveOut,
                    0x1 => FpOp::Classify,
                    _ => return Err(illegal),
                },
                0b11110 if rs2 == 0 && funct3 == 0 => FpOp::MoveIn,
                _ => return Err(illegal),
            };
            (Op::Fp { op, double }, rounding(inst))
        }
        0x2f => {
            // The aq and rl ordering bits, funct7[1:0], constrain nothing on a single
            // in-order hart.
            if funct7 >> 2 == 0b00101 {
                let width = match funct3 {
                    0x4 => CasWidth::Quad,
                    _ => CasWidth::Narrow(amo_width(funct3).ok_or(illegal)?),
                };
                // A quadword names a register pair at each end, and a pair starts at an
                // even register: the odd encodings are reserved.
                // The RISC-V Instruction Set Manual Volume I, 15.1.
                if width == CasWidth::Quad && !(rd.is_multiple_of(2) && rs2.is_multiple_of(2)) {
                    return Err(illegal);
                }
                return Ok(Inst {
                    op: Op::AmoCas { width },
                    rd,
                    rs1,
                    rs2,
                    imm: 0,
                });
            }
            let width = amo_width(funct3).ok_or(illegal)?;
            let op = match funct7 >> 2 {
                // Zabha leaves the reserved pair alone: a byte or halfword
                // load-reserved has too little use to be worth the encoding.
                // The RISC-V Instruction Set Manual Volume I, 16.1.
                0b00010 if rs2 == 0 && width.bits() >= 32 => Op::Lr { width },
                0b00011 if width.bits() >= 32 => Op::Sc { width },
                0b00001 => Op::Amo {
                    op: AmoOp::Swap,
                    width,
                },
                0b00000 => Op::Amo {
                    op: AmoOp::Add,
                    width,
                },
                0b00100 => Op::Amo {
                    op: AmoOp::Xor,
                    width,
                },
                0b01100 => Op::Amo {
                    op: AmoOp::And,
                    width,
                },
                0b01000 => Op::Amo {
                    op: AmoOp::Or,
                    width,
                },
                0b10000 => Op::Amo {
                    op: AmoOp::Min,
                    width,
                },
                0b10100 => Op::Amo {
                    op: AmoOp::Max,
                    width,
                },
                0b11000 => Op::Amo {
                    op: AmoOp::MinU,
                    width,
                },
                0b11100 => Op::Amo {
                    op: AmoOp::MaxU,
                    width,
                },
                _ => return Err(illegal),
            };
            (op, 0)
        }
        _ => return Err(illegal),
    };

    Ok(Inst {
        op,
        rd,
        rs1,
        rs2,
        imm,
    })
}

/// The names of the integer registers in the calling convention.
pub const REG_NAMES: [&str; 32] = [
    "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3", "a4",
    "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4",
    "t5", "t6",
];

fn reg(n: usize) -> &'static str {
    REG_NAMES[n]
}

impl fmt::Display for Inst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (rd, rs1, rs2) = (reg(self.rd), reg(self.rs1), reg(self.rs2));
        let imm = self.imm as i64;
        match self.op {
            Op::Load { width, signed } => write!(
                f,
                "l{}{} {rd}, {imm}({rs1})",
                width.suffix(),
                if signed { "" } else { "u" }
            ),
            Op::Store { width } => write!(f, "s{} {rs2}, {imm}({rs1})", width.suffix()),
            Op::Branch { cond } => {
                let name = match cond {
                    Cond::Eq => "beq",
                    Cond::Ne => "bne",
                    Cond::Lt => "blt",
                    Cond::Ge => "bge",
                    Cond::Ltu => "bltu",
                    Cond::Geu => "bgeu",
                };
                write!(f, "{name} {rs1}, {rs2}, {imm:+}")
            }
            Op::Lui | Op::Auipc => {
                let name = if self.op == Op::Lui { "lui" } else { "auipc" };
                write!(f, "{name} {rd}, {:#x}", (self.imm >> 12) & 0xfffff)
            }
            Op::Jal => write!(f, "jal {rd}, {imm:+}"),
            Op::Jalr => write!(f, "jalr {rd}, {imm}({rs1})"),
            Op::Fence => write!(f, "fence"),
            Op::FenceI => write!(f, "fence.i"),
            Op::Ecall => write!(f, "ecall"),
            Op::Ebreak => write!(f, "ebreak"),
            Op::Mret => write!(f, "mret"),
            Op::Sret => write!(f, "sret"),
            Op::Wfi => write!(f, "wfi"),
            Op::SfenceVma => write!(f, "sfence.vma {rs1}, {rs2}"),
            Op::Wrs { timeout } => {
                write!(f, "wrs.{}", if timeout { "sto" } else { "nto" })
            }
            Op::Csrrw { immediate } | Op::Csrrs { immediate } | Op::Csrrc { immediate } => {
                let name = match self.op {
                    Op::Csrrw { .. } => "csrrw",
                    Op::Csrrs { .. } => "csrrs",
                    _ => "csrrc",
                };
                let i = if immediate { "i" } else { "" };
                if immediate {
                    write!(f, "{name}{i} {rd}, {:#x}, {}", self.imm, self.rs1)
                } else {
                    write!(f, "{name}{i} {rd}, {:#x}, {rs1}", self.imm)
                }
            }
            Op::Lr { width } => write!(f, "lr.{} {rd}, ({rs1})", width.suffix()),
            Op::Sc { width } => write!(f, "sc.{} {rd}, {rs2}, ({rs1})", width.suffix()),
            Op::Amo { op, width } => {
                let name = match op {
                    AmoOp::Swap => "amoswap",
                    AmoOp::Add => "amoadd",
                    AmoOp::Xor => "amoxor",
                    AmoOp::And => "amoand",
                    AmoOp::Or => "amoor",
                    AmoOp::Min => "amomin",
                    AmoOp::Max => "amomax",
                    AmoOp::MinU => "amominu",
                    AmoOp::MaxU => "amomaxu",
                };
                write!(f, "{name}.{} {rd}, {rs2}, ({rs1})", width.suffix())
            }
            Op::AmoCas { width } => {
                write!(f, "amocas.{} {rd}, {rs2}, ({rs1})", width.suffix())
            }
            Op::CzeroEqz => write!(f, "czero.eqz {rd}, {rs1}, {rs2}"),
            Op::CzeroNez => write!(f, "czero.nez {rd}, {rs1}, {rs2}"),
            // Everything left is either register-register or register-immediate, and
            // its name is its variant lowercased.
            _ => {
                let name = format!("{:?}", self.op).to_lowercase();
                match self.op {
                    Op::Addi | Op::Slti | Op::Sltiu | Op::Xori | Op::Ori | Op::Andi | Op::Addiw => {
                        write!(f, "{name} {rd}, {rs1}, {imm}")
                    }
                    Op::Slli | Op::Srli | Op::Srai | Op::Slliw | Op::Srliw | Op::Sraiw => {
                        write!(f, "{name} {rd}, {rs1}, {}", self.imm)
                    }
                    _ => write!(f, "{name} {rd}, {rs1}, {rs2}"),
                }
            }
        }
    }
}
