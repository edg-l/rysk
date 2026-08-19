//! An assembler and a harness for running short programs on a fresh `Cpu`.

#![allow(dead_code)]

use rysk::{bus::DRAM_BASE, cpu::Cpu, dram::DRAM_SIZE};

pub const ZERO: u32 = 0;
pub const RA: u32 = 1;
pub const SP: u32 = 2;
pub const T0: u32 = 5;
pub const T1: u32 = 6;
pub const T2: u32 = 7;
pub const S0: u32 = 8;
pub const S1: u32 = 9;
pub const A0: u32 = 10;
pub const A1: u32 = 11;
pub const A2: u32 = 12;
pub const A3: u32 = 13;
pub const A4: u32 = 14;
pub const A5: u32 = 15;
pub const T3: u32 = 28;
pub const T4: u32 = 29;
pub const T5: u32 = 30;
pub const T6: u32 = 31;

/// An address in dram that no test program occupies, for tests that need memory.
pub const SCRATCH: u64 = DRAM_BASE + 0x1000;

// ---------------------------------------------------------------- formats

const fn r(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
    (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
}

const fn i(imm: i32, rs1: u32, funct3: u32, rd: u32, opcode: u32) -> u32 {
    (((imm & 0xfff) as u32) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | opcode
}

const fn s(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 5) & 0x7f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | ((imm & 0x1f) << 7)
        | opcode
}

const fn b(imm: i32, rs2: u32, rs1: u32, funct3: u32, opcode: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 12) & 1) << 31)
        | (((imm >> 5) & 0x3f) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (((imm >> 1) & 0xf) << 8)
        | (((imm >> 11) & 1) << 7)
        | opcode
}

const fn u(imm20: u32, rd: u32, opcode: u32) -> u32 {
    ((imm20 & 0xfffff) << 12) | (rd << 7) | opcode
}

const fn j(imm: i32, rd: u32, opcode: u32) -> u32 {
    let imm = imm as u32;
    (((imm >> 20) & 1) << 31)
        | (((imm >> 1) & 0x3ff) << 21)
        | (((imm >> 11) & 1) << 20)
        | (((imm >> 12) & 0xff) << 12)
        | (rd << 7)
        | opcode
}

// ---------------------------------------------------------------- mnemonics

macro_rules! rr {
    ($($name:ident = ($opcode:expr, $funct3:expr, $funct7:expr);)*) => {$(
        pub const fn $name(rd: u32, rs1: u32, rs2: u32) -> u32 {
            r($funct7, rs2, rs1, $funct3, rd, $opcode)
        }
    )*};
}

macro_rules! ri {
    ($($name:ident = ($opcode:expr, $funct3:expr);)*) => {$(
        pub const fn $name(rd: u32, rs1: u32, imm: i32) -> u32 {
            i(imm, rs1, $funct3, rd, $opcode)
        }
    )*};
}

macro_rules! shifti {
    ($($name:ident = ($opcode:expr, $funct3:expr, $funct6:expr);)*) => {$(
        pub const fn $name(rd: u32, rs1: u32, shamt: u32) -> u32 {
            i((($funct6 << 6) | (shamt & 0x3f)) as i32, rs1, $funct3, rd, $opcode)
        }
    )*};
}

macro_rules! store {
    ($($name:ident = $funct3:expr;)*) => {$(
        pub const fn $name(rs2: u32, rs1: u32, imm: i32) -> u32 { s(imm, rs2, rs1, $funct3, 0x23) }
    )*};
}

macro_rules! branch {
    ($($name:ident = $funct3:expr;)*) => {$(
        pub const fn $name(rs1: u32, rs2: u32, imm: i32) -> u32 { b(imm, rs2, rs1, $funct3, 0x63) }
    )*};
}

macro_rules! amo {
    ($($name:ident = ($funct3:expr, $funct5:expr);)*) => {$(
        pub const fn $name(rd: u32, rs2: u32, rs1: u32) -> u32 {
            r($funct5 << 2, rs2, rs1, $funct3, rd, 0x2f)
        }
    )*};
}

rr! {
    add = (0x33, 0x0, 0x00); sub = (0x33, 0x0, 0x20);
    sll = (0x33, 0x1, 0x00); slt = (0x33, 0x2, 0x00); sltu = (0x33, 0x3, 0x00);
    xor = (0x33, 0x4, 0x00); srl = (0x33, 0x5, 0x00); sra = (0x33, 0x5, 0x20);
    or = (0x33, 0x6, 0x00); and = (0x33, 0x7, 0x00);
    mul = (0x33, 0x0, 0x01); mulh = (0x33, 0x1, 0x01); mulhsu = (0x33, 0x2, 0x01);
    mulhu = (0x33, 0x3, 0x01); div = (0x33, 0x4, 0x01); divu = (0x33, 0x5, 0x01);
    rem = (0x33, 0x6, 0x01); remu = (0x33, 0x7, 0x01);
    czero_eqz = (0x33, 0x5, 0x07); czero_nez = (0x33, 0x7, 0x07);
    addw = (0x3b, 0x0, 0x00); subw = (0x3b, 0x0, 0x20);
    sllw = (0x3b, 0x1, 0x00); srlw = (0x3b, 0x5, 0x00); sraw = (0x3b, 0x5, 0x20);
    mulw = (0x3b, 0x0, 0x01); divw = (0x3b, 0x4, 0x01); divuw = (0x3b, 0x5, 0x01);
    remw = (0x3b, 0x6, 0x01); remuw = (0x3b, 0x7, 0x01);
}

ri! {
    addi = (0x13, 0x0); slti = (0x13, 0x2); sltiu = (0x13, 0x3);
    xori = (0x13, 0x4); ori = (0x13, 0x6); andi = (0x13, 0x7);
    addiw = (0x1b, 0x0);
    lb = (0x03, 0x0); lh = (0x03, 0x1); lw = (0x03, 0x2); ld = (0x03, 0x3);
    lbu = (0x03, 0x4); lhu = (0x03, 0x5); lwu = (0x03, 0x6);
    jalr = (0x67, 0x0);
}

shifti! {
    slli = (0x13, 0x1, 0x00); srli = (0x13, 0x5, 0x00); srai = (0x13, 0x5, 0x10);
    slliw = (0x1b, 0x1, 0x00); srliw = (0x1b, 0x5, 0x00); sraiw = (0x1b, 0x5, 0x10);
}

store! { sb = 0x0; sh = 0x1; sw = 0x2; sd = 0x3; }

branch! { beq = 0x0; bne = 0x1; blt = 0x4; bge = 0x5; bltu = 0x6; bgeu = 0x7; }

amo! {
    lr_w = (0x2, 0x02); sc_w = (0x2, 0x03); amoswap_w = (0x2, 0x01); amoadd_w = (0x2, 0x00);
    amoxor_w = (0x2, 0x04); amoand_w = (0x2, 0x0c); amoor_w = (0x2, 0x08);
    amomin_w = (0x2, 0x10); amomax_w = (0x2, 0x14); amominu_w = (0x2, 0x18);
    amomaxu_w = (0x2, 0x1c);
    lr_d = (0x3, 0x02); sc_d = (0x3, 0x03); amoswap_d = (0x3, 0x01); amoadd_d = (0x3, 0x00);
    amoxor_d = (0x3, 0x04); amoand_d = (0x3, 0x0c); amoor_d = (0x3, 0x08);
    amomin_d = (0x3, 0x10); amomax_d = (0x3, 0x14); amominu_d = (0x3, 0x18);
    amomaxu_d = (0x3, 0x1c);
}

pub const fn lui(rd: u32, imm20: u32) -> u32 {
    u(imm20, rd, 0x37)
}

pub const fn auipc(rd: u32, imm20: u32) -> u32 {
    u(imm20, rd, 0x17)
}

pub const fn jal(rd: u32, imm: i32) -> u32 {
    j(imm, rd, 0x6f)
}

pub const fn csrrw(rd: u32, csr: u32, rs1: u32) -> u32 {
    i(csr as i32, rs1, 0x1, rd, 0x73)
}

pub const fn csrrs(rd: u32, csr: u32, rs1: u32) -> u32 {
    i(csr as i32, rs1, 0x2, rd, 0x73)
}

pub const fn csrrc(rd: u32, csr: u32, rs1: u32) -> u32 {
    i(csr as i32, rs1, 0x3, rd, 0x73)
}

pub const fn csrrwi(rd: u32, csr: u32, uimm: u32) -> u32 {
    i(csr as i32, uimm, 0x5, rd, 0x73)
}

pub const fn csrrsi(rd: u32, csr: u32, uimm: u32) -> u32 {
    i(csr as i32, uimm, 0x6, rd, 0x73)
}

pub const fn csrrci(rd: u32, csr: u32, uimm: u32) -> u32 {
    i(csr as i32, uimm, 0x7, rd, 0x73)
}

/// `nop`, for padding a program out to a known length.
pub const fn nop() -> u32 {
    addi(ZERO, ZERO, 0)
}

// ---------------------------------------------------------------- harness

/// A program to run, plus the register state it starts from.
pub struct Program {
    code: Vec<u32>,
    regs: Vec<(u32, u64)>,
}

/// Assemble `code` into a program starting from a zeroed register file.
pub fn prog(code: &[u32]) -> Program {
    Program {
        code: code.to_vec(),
        regs: Vec::new(),
    }
}

impl Program {
    /// Preload a register, so a test does not have to build its inputs in assembly.
    pub fn reg(mut self, reg: u32, value: u64) -> Self {
        self.regs.push((reg, value));
        self
    }

    /// Run to completion. Execution ends on the zeroed word past the last instruction,
    /// which decodes as an illegal opcode.
    pub fn run(self) -> Cpu {
        let mut bytes = Vec::with_capacity(self.code.len() * 4);
        for inst in &self.code {
            bytes.extend_from_slice(&inst.to_le_bytes());
        }

        let mut cpu = Cpu::new(bytes);
        for (reg, value) in self.regs {
            cpu.regs[reg as usize] = value;
        }
        cpu.run().unwrap();
        cpu
    }
}

/// Run `code` and return the resulting machine.
pub fn run(code: &[u32]) -> Cpu {
    prog(code).run()
}

pub trait Inspect {
    fn reg(&self, reg: u32) -> u64;
    fn load(&self, addr: u64, bytes: u64) -> u64;
}

impl Inspect for Cpu {
    fn reg(&self, reg: u32) -> u64 {
        self.regs[reg as usize]
    }

    fn load(&self, addr: u64, bytes: u64) -> u64 {
        assert!(
            (DRAM_BASE..DRAM_BASE + DRAM_SIZE).contains(&addr),
            "address {addr:#x} is outside dram"
        );
        self.bus.load(addr, bytes * 8).expect("load failed")
    }
}
