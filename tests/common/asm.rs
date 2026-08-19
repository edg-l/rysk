//! An assembler: one function per instruction, each returning its encoding.
//!
//! `tests/encodings.s` is the same instructions run through a real assembler, and
//! `encoder_matches_the_toolchain` holds the two against each other.

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
    amocas_w = (0x2, 0x05); amocas_d = (0x3, 0x05); amocas_q = (0x4, 0x05);
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

/// `fence iorw, iorw`, the conservative form an assembler emits for a bare `fence`.
pub const fn fence() -> u32 {
    i(0x0ff, 0, 0x0, 0, 0x0f)
}

pub const fn fence_i() -> u32 {
    i(0x000, 0, 0x1, 0, 0x0f)
}

pub const fn ecall() -> u32 {
    i(0x000, 0, 0x0, 0, 0x73)
}

pub const fn ebreak() -> u32 {
    i(0x001, 0, 0x0, 0, 0x73)
}

pub const fn mret() -> u32 {
    i(0x302, 0, 0x0, 0, 0x73)
}

pub const fn sret() -> u32 {
    i(0x102, 0, 0x0, 0, 0x73)
}

pub const fn wfi() -> u32 {
    i(0x105, 0, 0x0, 0, 0x73)
}

/// `nop`, for padding a program out to a known length.
pub const fn nop() -> u32 {
    addi(ZERO, ZERO, 0)
}

// ------------------------------------------------------------- compressed

/// The three-bit register fields name `x8` to `x15`, so an encoder takes the real
/// register number and this checks it is one of them.
const fn short(reg: u32) -> u16 {
    assert!(
        reg >= 8 && reg < 16,
        "not a register the short fields reach"
    );
    (reg - 8) as u16
}

const fn part(imm: i32, high: u32, low: u32, at: u32) -> u16 {
    (((imm >> low) as u32 & ((1 << (high - low + 1)) - 1)) << at) as u16
}

/// `funct4 | rd/rs1 | rs2 | op`, the register form.
const fn cr(funct4: u16, rd: u32, rs2: u32, op: u16) -> u16 {
    (funct4 << 12) | ((rd as u16) << 7) | ((rs2 as u16) << 2) | op
}

/// `funct3 | imm[5] | rd/rs1 | imm[4:0] | op`, the immediate form.
const fn ci(funct3: u16, imm: i32, rd: u32, op: u16) -> u16 {
    (funct3 << 13) | part(imm, 5, 5, 12) | ((rd as u16) << 7) | part(imm, 4, 0, 2) | op
}

/// `funct6 | rd'/rs1' | funct2 | rs2' | op`, the arithmetic form.
const fn ca(funct6: u16, rd: u32, funct2: u16, rs2: u32, op: u16) -> u16 {
    (funct6 << 10) | (short(rd) << 7) | (funct2 << 5) | (short(rs2) << 2) | op
}

pub const fn c_nop() -> u16 {
    ci(0b000, 0, 0, 0b01)
}

pub const fn c_addi(rd: u32, imm: i32) -> u16 {
    ci(0b000, imm, rd, 0b01)
}

pub const fn c_addiw(rd: u32, imm: i32) -> u16 {
    ci(0b001, imm, rd, 0b01)
}

pub const fn c_li(rd: u32, imm: i32) -> u16 {
    ci(0b010, imm, rd, 0b01)
}

pub const fn c_slli(rd: u32, shamt: u32) -> u16 {
    ci(0b000, shamt as i32, rd, 0b10)
}

/// `lui`'s immediate is the value it puts in bits 17:12, so it is given here the way
/// the assembler takes it: already shifted down.
pub const fn c_lui(rd: u32, imm: i32) -> u16 {
    ci(0b011, imm, rd, 0b01)
}

/// The stack pointer moved in units of sixteen bytes.
pub const fn c_addi16sp(imm: i32) -> u16 {
    (0b011 << 13)
        | part(imm, 9, 9, 12)
        | (2 << 7)
        | part(imm, 4, 4, 6)
        | part(imm, 6, 6, 5)
        | part(imm, 8, 7, 3)
        | part(imm, 5, 5, 2)
        | 0b01
}

/// The stack pointer plus a scaled unsigned offset, into one of the short registers.
///
/// This and the two below are quadrant zero, whose opcode bits are both zero and so go
/// unwritten, as does this one's funct3.
pub const fn c_addi4spn(rd: u32, imm: i32) -> u16 {
    part(imm, 5, 4, 11)
        | part(imm, 9, 6, 7)
        | part(imm, 2, 2, 6)
        | part(imm, 3, 3, 5)
        | (short(rd) << 2)
}

pub const fn c_lw(rd: u32, rs1: u32, imm: i32) -> u16 {
    (0b010 << 13)
        | part(imm, 5, 3, 10)
        | (short(rs1) << 7)
        | part(imm, 2, 2, 6)
        | part(imm, 6, 6, 5)
        | (short(rd) << 2)
}

pub const fn c_ld(rd: u32, rs1: u32, imm: i32) -> u16 {
    (0b011 << 13) | part(imm, 5, 3, 10) | (short(rs1) << 7) | part(imm, 7, 6, 5) | (short(rd) << 2)
}

pub const fn c_sw(rs2: u32, rs1: u32, imm: i32) -> u16 {
    c_lw(rs2, rs1, imm) | (0b100 << 13)
}

pub const fn c_sd(rs2: u32, rs1: u32, imm: i32) -> u16 {
    c_ld(rs2, rs1, imm) | (0b100 << 13)
}

pub const fn c_lwsp(rd: u32, imm: i32) -> u16 {
    (0b010 << 13)
        | part(imm, 5, 5, 12)
        | ((rd as u16) << 7)
        | part(imm, 4, 2, 4)
        | part(imm, 7, 6, 2)
        | 0b10
}

pub const fn c_ldsp(rd: u32, imm: i32) -> u16 {
    (0b011 << 13)
        | part(imm, 5, 5, 12)
        | ((rd as u16) << 7)
        | part(imm, 4, 3, 5)
        | part(imm, 8, 6, 2)
        | 0b10
}

pub const fn c_swsp(rs2: u32, imm: i32) -> u16 {
    (0b110 << 13) | part(imm, 5, 2, 9) | part(imm, 7, 6, 7) | ((rs2 as u16) << 2) | 0b10
}

pub const fn c_sdsp(rs2: u32, imm: i32) -> u16 {
    (0b111 << 13) | part(imm, 5, 3, 10) | part(imm, 8, 6, 7) | ((rs2 as u16) << 2) | 0b10
}

pub const fn c_srli(rd: u32, shamt: u32) -> u16 {
    (0b100 << 13)
        | part(shamt as i32, 5, 5, 12)
        | (short(rd) << 7)
        | part(shamt as i32, 4, 0, 2)
        | 0b01
}

pub const fn c_srai(rd: u32, shamt: u32) -> u16 {
    c_srli(rd, shamt) | (0b01 << 10)
}

pub const fn c_andi(rd: u32, imm: i32) -> u16 {
    (0b100 << 13)
        | part(imm, 5, 5, 12)
        | (0b10 << 10)
        | (short(rd) << 7)
        | part(imm, 4, 0, 2)
        | 0b01
}

pub const fn c_sub(rd: u32, rs2: u32) -> u16 {
    ca(0b100011, rd, 0b00, rs2, 0b01)
}

pub const fn c_xor(rd: u32, rs2: u32) -> u16 {
    ca(0b100011, rd, 0b01, rs2, 0b01)
}

pub const fn c_or(rd: u32, rs2: u32) -> u16 {
    ca(0b100011, rd, 0b10, rs2, 0b01)
}

pub const fn c_and(rd: u32, rs2: u32) -> u16 {
    ca(0b100011, rd, 0b11, rs2, 0b01)
}

pub const fn c_subw(rd: u32, rs2: u32) -> u16 {
    ca(0b100111, rd, 0b00, rs2, 0b01)
}

pub const fn c_addw(rd: u32, rs2: u32) -> u16 {
    ca(0b100111, rd, 0b01, rs2, 0b01)
}

pub const fn c_j(imm: i32) -> u16 {
    (0b101 << 13)
        | part(imm, 11, 11, 12)
        | part(imm, 4, 4, 11)
        | part(imm, 9, 8, 9)
        | part(imm, 10, 10, 8)
        | part(imm, 6, 6, 7)
        | part(imm, 7, 7, 6)
        | part(imm, 3, 1, 3)
        | part(imm, 5, 5, 2)
        | 0b01
}

const fn cb(funct3: u16, rs1: u32, imm: i32) -> u16 {
    (funct3 << 13)
        | part(imm, 8, 8, 12)
        | part(imm, 4, 3, 10)
        | (short(rs1) << 7)
        | part(imm, 7, 6, 5)
        | part(imm, 2, 1, 3)
        | part(imm, 5, 5, 2)
        | 0b01
}

pub const fn c_beqz(rs1: u32, imm: i32) -> u16 {
    cb(0b110, rs1, imm)
}

pub const fn c_bnez(rs1: u32, imm: i32) -> u16 {
    cb(0b111, rs1, imm)
}

pub const fn c_jr(rs1: u32) -> u16 {
    cr(0b1000, rs1, 0, 0b10)
}

pub const fn c_mv(rd: u32, rs2: u32) -> u16 {
    cr(0b1000, rd, rs2, 0b10)
}

pub const fn c_jalr(rs1: u32) -> u16 {
    cr(0b1001, rs1, 0, 0b10)
}

pub const fn c_add(rd: u32, rs2: u32) -> u16 {
    cr(0b1001, rd, rs2, 0b10)
}

pub const fn c_ebreak() -> u16 {
    cr(0b1001, 0, 0, 0b10)
}
