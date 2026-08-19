//! The compressed instructions, expanded into the ones they stand for.
//!
//! Every instruction here is a shorter spelling of something the base set already has,
//! so decoding one produces the same [`Inst`] its 32-bit form would. That is why there
//! is nothing to execute: `cpu` never learns that an instruction arrived compressed,
//! only that it was two bytes long. A trace therefore disassembles the expansion, so
//! `c.addi16sp` reads as the `addi sp, sp, -16` it is.
//!
//! The RISC-V Instruction Set Manual Volume I, chapter 27.

use crate::{
    inst::{Cond, Inst, Op, Width},
    trap::Exception,
};

/// The eight registers the three-bit fields name, which are `x8` to `x15`.
/// The RISC-V Instruction Set Manual Volume I, 27.2, table 38.
const fn popular(field: u16) -> usize {
    (field & 0b111) as usize + 8
}

const fn bits(half: u16, high: u32, low: u32) -> u64 {
    ((half >> low) & ((1 << (high - low + 1)) - 1)) as u64
}

const fn bit(half: u16, at: u32) -> u64 {
    ((half >> at) & 1) as u64
}

/// Sign-extend `value`, whose top bit is `top`.
const fn sext(value: u64, top: u32) -> u64 {
    ((value << (63 - top)) as i64 >> (63 - top)) as u64
}

const fn make(op: Op, rd: usize, rs1: usize, rs2: usize, imm: u64) -> Inst {
    Inst {
        op,
        rd,
        rs1,
        rs2,
        imm,
    }
}

/// Expand one compressed instruction, or reject it.
///
/// The encodings that are reserved are rejected here rather than executed as something
/// else, but the ones defined as hints are not: every hint expands to a base
/// instruction whose destination is `x0`, which retires and changes nothing, so they
/// need no case of their own.
pub fn decode(half: u16) -> Result<Inst, Exception> {
    let illegal = Exception::IllegalInstruction(half as u64);
    let funct3 = (half >> 13) & 0b111;
    // The three-bit register fields, and the five-bit ones that share their positions.
    let rd_short = popular(half >> 2);
    let rs1_short = popular(half >> 7);
    let wide = bits(half, 11, 7) as usize;
    let rs2_wide = bits(half, 6, 2) as usize;

    let inst = match (half & 0b11, funct3) {
        // ------------------------------------------------------------ quadrant 0
        (0b00, 0b000) => {
            // The stack pointer plus a scaled unsigned immediate, which is how a frame
            // hands out addresses to what it holds.
            let imm = (bits(half, 12, 11) << 4)
                | (bits(half, 10, 7) << 6)
                | (bit(half, 6) << 2)
                | (bit(half, 5) << 3);
            if imm == 0 {
                return Err(illegal);
            }
            make(Op::Addi, rd_short, 2, 0, imm)
        }
        (0b00, 0b010 | 0b110 | 0b011 | 0b111) => {
            let width = if funct3 & 0b001 == 0 {
                Width::Word
            } else {
                Width::Double
            };
            let imm = match width {
                Width::Word => {
                    (bits(half, 12, 10) << 3) | (bit(half, 6) << 2) | (bit(half, 5) << 6)
                }
                _ => (bits(half, 12, 10) << 3) | (bits(half, 6, 5) << 6),
            };
            if funct3 & 0b100 == 0 {
                make(
                    Op::Load {
                        width,
                        signed: true,
                    },
                    rd_short,
                    rs1_short,
                    0,
                    imm,
                )
            } else {
                make(Op::Store { width }, 0, rs1_short, rd_short, imm)
            }
        }

        // ------------------------------------------------------------ quadrant 1
        (0b01, 0b000) => {
            let imm = sext((bit(half, 12) << 5) | bits(half, 6, 2), 5);
            make(Op::Addi, wide, wide, 0, imm)
        }
        (0b01, 0b001) => {
            // The word form has no register to widen when it names none.
            if wide == 0 {
                return Err(illegal);
            }
            let imm = sext((bit(half, 12) << 5) | bits(half, 6, 2), 5);
            make(Op::Addiw, wide, wide, 0, imm)
        }
        (0b01, 0b010) => {
            let imm = sext((bit(half, 12) << 5) | bits(half, 6, 2), 5);
            make(Op::Addi, wide, 0, 0, imm)
        }
        (0b01, 0b011) if wide == 2 => {
            // Growing or shrinking the frame, in units of sixteen bytes.
            let imm = sext(
                (bit(half, 12) << 9)
                    | (bit(half, 6) << 4)
                    | (bit(half, 5) << 6)
                    | (bits(half, 4, 3) << 7)
                    | (bit(half, 2) << 5),
                9,
            );
            if imm == 0 {
                return Err(illegal);
            }
            make(Op::Addi, 2, 2, 0, imm)
        }
        (0b01, 0b011) => {
            let imm = sext((bit(half, 12) << 17) | (bits(half, 6, 2) << 12), 17);
            if imm == 0 {
                return Err(illegal);
            }
            make(Op::Lui, wide, 0, 0, imm)
        }
        (0b01, 0b100) => {
            let imm = (bit(half, 12) << 5) | bits(half, 6, 2);
            match bits(half, 11, 10) {
                0b00 => make(Op::Srli, rs1_short, rs1_short, 0, imm),
                0b01 => make(Op::Srai, rs1_short, rs1_short, 0, imm),
                0b10 => make(Op::Andi, rs1_short, rs1_short, 0, sext(imm, 5)),
                _ => {
                    let op = match (bit(half, 12), bits(half, 6, 5)) {
                        (0, 0b00) => Op::Sub,
                        (0, 0b01) => Op::Xor,
                        (0, 0b10) => Op::Or,
                        (0, 0b11) => Op::And,
                        (1, 0b00) => Op::Subw,
                        (1, 0b01) => Op::Addw,
                        _ => return Err(illegal),
                    };
                    make(op, rs1_short, rs1_short, rd_short, 0)
                }
            }
        }
        (0b01, 0b101) => make(Op::Jal, 0, 0, 0, jump_offset(half)),
        (0b01, 0b110 | 0b111) => {
            let cond = if funct3 == 0b110 { Cond::Eq } else { Cond::Ne };
            let imm = sext(
                (bit(half, 12) << 8)
                    | (bits(half, 11, 10) << 3)
                    | (bits(half, 6, 5) << 6)
                    | (bits(half, 4, 3) << 1)
                    | (bit(half, 2) << 5),
                8,
            );
            make(Op::Branch { cond }, 0, rs1_short, 0, imm)
        }

        // ------------------------------------------------------------ quadrant 2
        (0b10, 0b000) => {
            let imm = (bit(half, 12) << 5) | bits(half, 6, 2);
            make(Op::Slli, wide, wide, 0, imm)
        }
        (0b10, 0b010 | 0b011) => {
            // A load from the stack that names no destination has nowhere to put what
            // it read, which is reserved rather than a hint.
            if wide == 0 {
                return Err(illegal);
            }
            let (width, imm) = if funct3 == 0b010 {
                (
                    Width::Word,
                    (bit(half, 12) << 5) | (bits(half, 6, 4) << 2) | (bits(half, 3, 2) << 6),
                )
            } else {
                (
                    Width::Double,
                    (bit(half, 12) << 5) | (bits(half, 6, 5) << 3) | (bits(half, 4, 2) << 6),
                )
            };
            make(
                Op::Load {
                    width,
                    signed: true,
                },
                wide,
                2,
                0,
                imm,
            )
        }
        (0b10, 0b100) => match (bit(half, 12), wide, rs2_wide) {
            // A jump through a register that names none has nowhere to go.
            (0, 0, 0) => return Err(illegal),
            (0, _, 0) => make(Op::Jalr, 0, wide, 0, 0),
            (0, _, _) => make(Op::Add, wide, 0, rs2_wide, 0),
            (_, 0, 0) => make(Op::Ebreak, 0, 0, 0, 1),
            (_, _, 0) => make(Op::Jalr, 1, wide, 0, 0),
            (_, _, _) => make(Op::Add, wide, wide, rs2_wide, 0),
        },
        (0b10, 0b110 | 0b111) => {
            let (width, imm) = if funct3 == 0b110 {
                (
                    Width::Word,
                    (bits(half, 12, 9) << 2) | (bits(half, 8, 7) << 6),
                )
            } else {
                (
                    Width::Double,
                    (bits(half, 12, 10) << 3) | (bits(half, 9, 7) << 6),
                )
            };
            make(Op::Store { width }, 0, 2, rs2_wide, imm)
        }

        // Everything left is a floating-point form, a reserved encoding, or the
        // all-zero halfword the manual defines as illegal so that a jump into blank
        // memory stops rather than wanders.
        // The RISC-V Instruction Set Manual Volume I, 27.5.4.
        _ => return Err(illegal),
    };
    Ok(inst)
}

/// The offset of `c.j`, whose eleven bits are scattered across the halfword.
fn jump_offset(half: u16) -> u64 {
    sext(
        (bit(half, 12) << 11)
            | (bit(half, 11) << 4)
            | (bits(half, 10, 9) << 8)
            | (bit(half, 8) << 10)
            | (bit(half, 7) << 6)
            | (bit(half, 6) << 7)
            | (bits(half, 5, 3) << 1)
            | (bit(half, 2) << 5),
        11,
    )
}
