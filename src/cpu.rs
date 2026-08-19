use std::{
    ops::{BitAnd, BitOr, BitXor},
    time::Instant,
};

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    bus::{Bus, DRAM_BASE},
    dram::{DRAM_SIZE, Dram},
    exception::Exception,
};

#[derive(Debug, Clone)]
pub struct Cpu {
    pub regs: [u64; 32],
    /// The instruction being executed.
    pub pc: u64,
    /// Where control goes when it retires. Jumps and taken branches overwrite it.
    pub next_pc: u64,
    pub bus: Bus,
    /// Control and status registers. RISC-V ISA sets aside a 12-bit encoding
    /// space (csr[11:0]) for up to 4096 CSRs.
    pub csrs: [u64; 4096],
    pub start: Instant,
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

impl Cpu {
    pub fn new(code: Vec<u8>) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: DRAM_BASE,
            next_pc: DRAM_BASE,
            bus: Bus {
                dram: Dram::new(code),
                reservation: None,
            },
            csrs: [0; 4096],
            start: Instant::now(),
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_BASE + DRAM_SIZE;

        cpu
    }

    /// Run until a trap that nothing is installed to handle, and return it. With no
    /// handler in `mtvec` there is nowhere for a trap to go, so that is where a program
    /// ends: normally by running off its own code into the zeroed dram behind it.
    pub fn run(&mut self) -> Exception {
        loop {
            if let Err(exception) = self.step() {
                if self.csrs[MTVEC] == 0 {
                    return exception;
                }
                self.take_trap(exception);
            }
        }
    }

    /// Fetch, decode and execute one instruction.
    #[inline]
    pub fn step(&mut self) -> Result<(), Exception> {
        let inst = self.fetch()?;

        self.next_pc = self.pc.wrapping_add(4);
        self.csrs[RDCYCLE] += 1;
        self.csrs[INSTRET] += 1;

        self.execute(inst)?;

        self.regs[0] = 0;
        self.pc = self.next_pc;
        Ok(())
    }

    /// Enter the machine-mode trap handler: record where and why, stack the
    /// interrupt-enable bit, and jump through `mtvec`.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.6.1 and 3.1.7.
    fn take_trap(&mut self, exception: Exception) {
        self.csrs[MEPC] = self.pc;
        self.csrs[MCAUSE] = exception.cause();
        self.csrs[MTVAL] = exception.value();

        let status = self.csrs[MSTATUS];
        let mie = (status >> MSTATUS_MIE) & 1;
        // MIE moves to MPIE and clears, and the mode we came from lands in MPP, which
        // is always machine mode until there are other privilege levels.
        let status = (status & !(1 << MSTATUS_MPIE) & !(1 << MSTATUS_MIE) & !MSTATUS_MPP)
            | (mie << MSTATUS_MPIE)
            | MSTATUS_MPP_M;
        self.csrs[MSTATUS] = status;

        // Vectored mode only spreads interrupts out; exceptions always enter at the base.
        self.pc = self.csrs[MTVEC] & !0b11;
    }

    /// Return from a machine-mode trap: unstack the interrupt-enable bit and resume at
    /// `mepc`.
    fn trap_return(&mut self) {
        let status = self.csrs[MSTATUS];
        let mpie = (status >> MSTATUS_MPIE) & 1;
        self.csrs[MSTATUS] =
            (status & !(1 << MSTATUS_MIE)) | (mpie << MSTATUS_MIE) | (1 << MSTATUS_MPIE);
        self.next_pc = self.csrs[MEPC];
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    fn load_csr(&self, addr: usize) -> u64 {
        trace_insn!("loading csr");
        match addr {
            SIE => self.csrs[MIE] & self.csrs[MIDELEG],
            RDTIME => self.start.elapsed().as_secs(),
            _ => self.csrs[addr],
        }
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    fn store_csr(&mut self, addr: usize, value: u64) {
        trace_insn!("storing csr");
        match addr {
            SIE => {
                self.csrs[MIE] =
                    (self.csrs[MIE] & !self.csrs[MIDELEG]) | (value & self.csrs[MIDELEG]);
            }
            _ => self.csrs[addr] = value,
        }
    }

    /// Widen a value loaded from memory to the full register, sign-extending anything
    /// narrower than XLEN.
    #[inline]
    fn sext(&self, value: u64, size: u64) -> u64 {
        match size {
            8 => value as i8 as i64 as u64,
            16 => value as i16 as i64 as u64,
            32 => value as i32 as i64 as u64,
            _ => value,
        }
    }

    /// Load `size` bits at `addr`, store `op` applied to the loaded value and rs2 back
    /// over them, and leave the loaded value in rd.
    #[cfg_attr(not(feature = "trace"), allow(unused_variables))]
    fn amo(
        &mut self,
        rd: usize,
        addr: u64,
        src: u64,
        size: u64,
        name: &str,
        op: impl Fn(u64, u64) -> u64,
    ) -> Result<(), Exception> {
        trace_insn!("{name}");
        let data = self.bus.load(addr, size)?;
        let value = op(data, src);
        self.bus.store(addr, size, value)?;
        self.regs[rd] = self.sext(data, size);
        Ok(())
    }

    /// Take a branch or jump relative to the instruction being executed.
    #[inline]
    fn branch(&mut self, offset: u64) -> Result<(), Exception> {
        self.jump(self.pc.wrapping_add(offset))
    }

    /// Transfer control to `addr`, which has to be where an instruction can start.
    /// Without compressed instructions that means a multiple of four.
    #[inline]
    fn jump(&mut self, addr: u64) -> Result<(), Exception> {
        if addr & 0b11 != 0 {
            return Err(Exception::InstructionAddressMisaligned(addr));
        }
        self.next_pc = addr;
        Ok(())
    }

    #[inline]
    fn fetch(&self) -> Result<u64, Exception> {
        self.bus
            .load(self.pc, 32)
            .map_err(|_| Exception::InstructionAccessFault(self.pc))
    }

    #[cfg_attr(
        feature = "trace",
        instrument(
            skip(self),
            fields(opcode, rd, rs1, rs2, funct3, funct7, imm, shamt, csr, csr_addr)
        )
    )]
    fn execute(&mut self, inst: u64) -> Result<(), Exception> {
        let opcode = inst & 0x7f;
        let rd = ((inst >> 7) & 0x1f) as usize;
        let rs1 = ((inst >> 15) & 0x1f) as usize;
        let rs2 = ((inst >> 20) & 0x1f) as usize;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;

        trace_field!("opcode", opcode);
        trace_field!("rd", rd);
        trace_field!("rs1", rs1);
        trace_field!("rs2", rs2);
        trace_field!("funct3", funct3);
        trace_field!("funct7", funct7);

        match opcode {
            // load
            0x03 => {
                // imm[11:0] = inst[31:20]
                let imm = ((inst as i32 as i64) >> 20) as u64;
                trace_field!("imm", imm);
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        // lb
                        trace_insn!("LB");
                        self.regs[rd] = self.bus.load(addr, 8)? as i8 as i64 as u64;
                    }
                    0x1 => {
                        // lh
                        trace_insn!("LH");
                        self.regs[rd] = self.bus.load(addr, 16)? as i16 as i64 as u64;
                    }
                    0x2 => {
                        // lw
                        trace_insn!("LW");
                        self.regs[rd] = self.bus.load(addr, 32)? as i32 as i64 as u64;
                    }
                    0x3 => {
                        // ld
                        trace_insn!("LD");
                        self.regs[rd] = self.bus.load(addr, 64)? as i64 as u64;
                    }
                    0x4 => {
                        // lbu
                        trace_insn!("LBU");
                        self.regs[rd] = self.bus.load(addr, 8)?;
                    }
                    0x5 => {
                        // lhu
                        trace_insn!("LHU");
                        self.regs[rd] = self.bus.load(addr, 16)?;
                    }
                    0x6 => {
                        // lwu
                        trace_insn!("LWU");
                        self.regs[rd] = self.bus.load(addr, 32)?;
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                };
            }
            // store
            0x23 => {
                // imm[11:5|4:0] = inst[31:25|11:7]
                let imm = (((inst & 0xfe000000) as i32 as i64 >> 20) as u64) | ((inst >> 7) & 0x1f);
                trace_field!("imm", imm);
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        trace_insn!("SB");
                        self.bus.store(addr, 8, self.regs[rs2])?
                    }
                    0x1 => {
                        trace_insn!("SH");
                        self.bus.store(addr, 16, self.regs[rs2])?
                    }
                    0x2 => {
                        trace_insn!("SW");
                        self.bus.store(addr, 32, self.regs[rs2])?
                    }
                    0x3 => {
                        trace_insn!("SD");
                        self.bus.store(addr, 64, self.regs[rs2])?
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            // base imm
            0x13 => {
                let imm = ((inst & 0xfff00000) as i32 as i64 >> 20) as u64;
                trace_field!("imm", imm);

                // "The shift amount is encoded in the lower 6 bits of the I-immediate field for RV64I."
                let shamt = (imm & 0x3f) as u32;
                trace_field!("shamt", shamt);

                // The immediate shifts take the top six bits of the I-immediate as funct6,
                // since the sixth shift-amount bit occupies funct7's low bit.
                match (funct3, funct7 >> 1) {
                    (0x0, _) => {
                        // addi
                        trace_insn!("ADDI");
                        self.regs[rd] = self.regs[rs1].wrapping_add(imm);
                    }
                    (0x4, _) => {
                        // xori
                        trace_insn!("XORI");
                        self.regs[rd] = self.regs[rs1].bitxor(imm);
                    }
                    (0x6, _) => {
                        // ori
                        trace_insn!("ORI");
                        self.regs[rd] = self.regs[rs1].bitor(imm);
                    }
                    (0x7, _) => {
                        // andi
                        trace_insn!("ANDI");
                        self.regs[rd] = self.regs[rs1].bitand(imm);
                    }
                    (0x1, 0x00) => {
                        // slli
                        trace_insn!("SLLI");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt);
                    }
                    (0x5, 0x00) => {
                        // srli
                        trace_insn!("SRLI");
                        self.regs[rd] = self.regs[rs1].wrapping_shr(shamt);
                    }
                    (0x5, 0x10) => {
                        // srai
                        trace_insn!("SRAI");
                        self.regs[rd] = (self.regs[rs1] as i64).wrapping_shr(shamt) as u64;
                    }
                    (0x2, _) => {
                        // slti
                        trace_insn!("SLTI");
                        self.regs[rd] = ((self.regs[rs1] as i64) < (imm as i64)) as u64
                    }
                    (0x3, _) => {
                        // sltiu
                        trace_insn!("SLTIU");
                        self.regs[rd] = (self.regs[rs1] < imm) as u64
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            // base R
            0x33 => {
                // In RV64I, only the low 6 bits of rs2 are considered for the shift amount."
                let shamt = (self.regs[rs2] & 0x3f) as u32;
                trace_field!("shamt", shamt);

                match (funct3, funct7) {
                    (0x0, 0x0) => {
                        // add
                        trace_insn!("ADD");
                        self.regs[rd] = self.regs[rs1].wrapping_add(self.regs[rs2]);
                    }
                    (0x0, 0x20) => {
                        // sub
                        trace_insn!("SUB");
                        self.regs[rd] = self.regs[rs1].wrapping_sub(self.regs[rs2]);
                    }
                    (0x4, 0x0) => {
                        // xor
                        trace_insn!("XOR");
                        self.regs[rd] = self.regs[rs1].bitxor(self.regs[rs2]);
                    }
                    (0x6, 0x0) => {
                        // and
                        trace_insn!("OR");
                        self.regs[rd] = self.regs[rs1].bitor(self.regs[rs2]);
                    }
                    (0x7, 0x0) => {
                        // and
                        trace_insn!("AND");
                        self.regs[rd] = self.regs[rs1].bitand(self.regs[rs2]);
                    }
                    (0x1, 0x0) => {
                        // sll logical
                        trace_insn!("SLL");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt);
                    }
                    (0x5, 0x0) => {
                        // srl logical
                        trace_insn!("SRL");
                        self.regs[rd] = self.regs[rs1].wrapping_shr(shamt);
                    }
                    (0x5, 0x20) => {
                        // sra
                        trace_insn!("SRA");
                        self.regs[rd] = (self.regs[rs1] as i64).wrapping_shr(shamt) as u64;
                    }
                    (0x2, 0x0) => {
                        // slt
                        trace_insn!("SLT");
                        self.regs[rd] = ((self.regs[rs1] as i64) < (self.regs[rs2] as i64)) as u64
                    }
                    (0x3, 0x0) => {
                        // sltu
                        trace_insn!("SLTU");
                        self.regs[rd] = (self.regs[rs1] < self.regs[rs2]) as u64
                    }
                    (0x5, 0x7) => {
                        trace_insn!("CZERO.EQZ");

                        if self.regs[rs2] == 0 {
                            self.regs[rd] = 0;
                        } else {
                            self.regs[rd] = self.regs[rs1];
                        }
                    }
                    (0x7, 0x7) => {
                        trace_insn!("CZERO.NEZ");

                        if self.regs[rs2] != 0 {
                            self.regs[rd] = 0;
                        } else {
                            self.regs[rd] = self.regs[rs1];
                        }
                    }
                    (0x0, 0x1) => {
                        // mul
                        trace_insn!("MUL");
                        self.regs[rd] = self.regs[rs1].wrapping_mul(self.regs[rs2]);
                    }
                    (0x1, 0x1) => {
                        // mulh
                        trace_insn!("MULH");
                        self.regs[rd] = ((self.regs[rs1] as i64 as i128)
                            .wrapping_mul(self.regs[rs2] as i64 as i128)
                            >> 64) as u64;
                    }
                    (0x3, 0x1) => {
                        // mulhu
                        trace_insn!("MULHU");
                        self.regs[rd] = ((self.regs[rs1] as u128)
                            .wrapping_mul(self.regs[rs2] as u128)
                            >> 64) as u64;
                    }
                    (0x2, 0x1) => {
                        // mulhsu
                        trace_insn!("MULHSU");
                        self.regs[rd] = ((self.regs[rs1] as i64 as i128)
                            .wrapping_mul(self.regs[rs2] as u128 as i128)
                            >> 64) as u64;
                    }
                    (0x4, 0x1) => {
                        // div
                        trace_insn!("DIV");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = u64::MAX;
                        } else {
                            self.regs[rd] =
                                (self.regs[rs1] as i64).wrapping_div(self.regs[rs2] as i64) as u64;
                        }
                    }
                    (0x5, 0x1) => {
                        // divu
                        trace_insn!("DIVU");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = u64::MAX;
                        } else {
                            self.regs[rd] = self.regs[rs1].wrapping_div(self.regs[rs2]);
                        }
                    }
                    (0x6, 0x1) => {
                        // rem
                        trace_insn!("REM");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = self.regs[rs1];
                        } else {
                            self.regs[rd] =
                                (self.regs[rs1] as i64).wrapping_rem(self.regs[rs2] as i64) as u64;
                        }
                    }
                    (0x7, 0x1) => {
                        // remu
                        trace_insn!("REMU");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = self.regs[rs1];
                        } else {
                            self.regs[rd] = self.regs[rs1].wrapping_rem(self.regs[rs2]);
                        }
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            0x3b => {
                // addw and family
                let shamt = (self.regs[rs2] & 0x1f) as u32;
                match (funct3, funct7) {
                    (0x0, 0x0) => {
                        trace_insn!("ADDW");
                        self.regs[rd] =
                            self.regs[rs1].wrapping_add(self.regs[rs2]) as i32 as i64 as u64;
                    }
                    (0x0, 0x20) => {
                        trace_insn!("SUBW");
                        self.regs[rd] =
                            self.regs[rs1].wrapping_sub(self.regs[rs2]) as i32 as i64 as u64;
                    }
                    (0x1, 0x00) => {
                        trace_insn!("SLLW");
                        self.regs[rd] = (self.regs[rs1] as u32).wrapping_shl(shamt) as i32 as u64;
                    }
                    (0x5, 0x00) => {
                        trace_insn!("SRLW");
                        self.regs[rd] = (self.regs[rs1] as u32).wrapping_shr(shamt) as i32 as u64;
                    }
                    (0x5, 0x20) => {
                        trace_insn!("SRAW");
                        self.regs[rd] = ((self.regs[rs1] as i32) >> (shamt as i32)) as u64;
                    }
                    (0x0, 0x1) => {
                        trace_insn!("MULW");
                        self.regs[rd] = (self.regs[rs1] as i32).wrapping_mul(self.regs[rs2] as i32)
                            as i64 as u64
                    }
                    (0x4, 0x1) => {
                        trace_insn!("DIVW");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = u64::MAX;
                        } else {
                            self.regs[rd] = (self.regs[rs1] as i32)
                                .wrapping_div(self.regs[rs2] as i32)
                                as i64 as u64
                        }
                    }
                    (0x5, 0x1) => {
                        trace_insn!("DIVUW");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = u64::MAX;
                        } else {
                            self.regs[rd] =
                                (self.regs[rs1] as u32).wrapping_div(self.regs[rs2] as u32) as u64;
                        }
                    }
                    (0x6, 0x1) => {
                        trace_insn!("REMW");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = self.regs[rs1] as i32 as i64 as u64;
                        } else {
                            self.regs[rd] = (self.regs[rs1] as i32)
                                .wrapping_rem(self.regs[rs2] as i32)
                                as i64 as u64;
                        }
                    }
                    (0x7, 0x1) => {
                        trace_insn!("REMUW");
                        if self.regs[rs2] == 0 {
                            self.regs[rd] = self.regs[rs1] as u32 as i32 as i64 as u64;
                        } else {
                            self.regs[rd] =
                                (self.regs[rs1] as u32).wrapping_rem(self.regs[rs2] as u32) as u64;
                        }
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            0x1b => {
                // addiw and family

                let imm = ((inst as i32 as i64) >> 20) as u64;
                let shamt = (imm & 0x1f) as u32;

                match (funct3, funct7) {
                    (0x0, _) => {
                        trace_field!("imm", imm);
                        trace_insn!("ADDIW");
                        self.regs[rd] = self.regs[rs1].wrapping_add(imm) as i32 as i64 as u64;
                    }
                    (0x1, _) => {
                        trace_field!("shamt", shamt);
                        trace_insn!("SLLIW");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt) as i32 as i64 as u64;
                    }
                    (0x5, 0) => {
                        trace_field!("shamt", shamt);
                        trace_insn!("SRLIW");
                        self.regs[rd] =
                            (self.regs[rs1] as u32).wrapping_shr(shamt) as i32 as i64 as u64;
                    }
                    (0x5, 0x20) => {
                        trace_field!("shamt", shamt);
                        trace_insn!("SRAIW");
                        self.regs[rd] = (self.regs[rs1] as i32).wrapping_shr(shamt) as i64 as u64;
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            0x63 => {
                // branching
                // imm[12|10:5|4:1|11] = inst[31|30:25|11:8|7]
                let imm = (((inst & 0x80000000) as i32 as i64 >> 19) as u64)
                    | ((inst & 0x80) << 4) // imm[11]
                    | ((inst >> 20) & 0x7e0) // imm[10:5]
                    | ((inst >> 7) & 0x1e); // imm[4:1]
                trace_field!("imm", imm);

                match funct3 {
                    0x0 => {
                        trace_insn!("BEQ");

                        if self.regs[rs1] == self.regs[rs2] {
                            self.branch(imm)?;
                        }
                    }
                    0x1 => {
                        trace_insn!("BNE");

                        if self.regs[rs1] != self.regs[rs2] {
                            self.branch(imm)?;
                        }
                    }
                    0x4 => {
                        trace_insn!("BLT");

                        if (self.regs[rs1] as i64) < (self.regs[rs2] as i64) {
                            self.branch(imm)?;
                        }
                    }
                    0x5 => {
                        trace_insn!("BGE");

                        if (self.regs[rs1] as i64) >= (self.regs[rs2] as i64) {
                            self.branch(imm)?;
                        }
                    }
                    0x6 => {
                        trace_insn!("BLTU");

                        if self.regs[rs1] < self.regs[rs2] {
                            self.branch(imm)?;
                        }
                    }
                    0x7 => {
                        trace_insn!("BGEU");

                        if self.regs[rs1] >= self.regs[rs2] {
                            self.branch(imm)?;
                        }
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            0x37 => {
                // LUI
                let imm32 = (inst & 0xfffff000) as i32 as i64 as u64;
                trace_field!("imm", imm32);
                trace_insn!("LUI");
                self.regs[rd] = imm32;
            }
            0x17 => {
                // AUIPC, relative to the address of this instruction, which run() has
                // already stepped past
                let imm32 = (inst & 0xfffff000) as i32 as i64 as u64;
                trace_field!("imm", imm32);
                trace_insn!("AUIPC");
                self.regs[rd] = self.pc.wrapping_add(imm32);
            }
            0x6f => {
                // JAL
                // imm[20|10:1|11|19:12] = inst[31|30:21|20|19:12]
                let imm = (((inst & 0x80000000) as i32 as i64 >> 11) as u64) // imm[20]
                    | (inst & 0xff000) // imm[19:12]
                    | ((inst >> 9) & 0x800) // imm[11]
                    | ((inst >> 20) & 0x7fe); // imm[10:1]
                trace_field!("imm", imm);
                trace_insn!("JAL");
                let link = self.next_pc;
                self.branch(imm)?;
                self.regs[rd] = link;
            }
            0x67 => {
                // JALR
                let imm = ((((inst & 0xfff00000) as i32) as i64) >> 20) as u64;
                trace_field!("imm", imm);

                // The target comes from rs1's value before the link is written, since
                // rd and rs1 are commonly the same register.
                let addr = self.regs[rs1].wrapping_add(imm) & !1;
                let link = self.next_pc;
                self.jump(addr)?;
                self.regs[rd] = link;
                trace_insn!("JALR");
            }
            0x73 => {
                let csr_addr = ((inst & 0xfff00000) >> 20) as usize;
                trace_field!("csr_addr", csr_addr);
                let imm = rs1 as u64;
                match funct3 {
                    // funct3 of zero is not a csr access: the whole 12-bit immediate
                    // selects a privileged instruction.
                    0x0 => match csr_addr {
                        0x000 => {
                            trace_insn!("ECALL");
                            return Err(Exception::EnvironmentCallFromMMode);
                        }
                        0x001 => {
                            trace_insn!("EBREAK");
                            return Err(Exception::Breakpoint(self.pc));
                        }
                        0x302 => {
                            trace_insn!("MRET");
                            self.trap_return();
                        }
                        0x105 => {
                            // Nothing can raise an interrupt yet, so waiting for one
                            // would never end. Retiring immediately is permitted.
                            trace_insn!("WFI");
                        }
                        _ => return Err(Exception::IllegalInstruction(inst)),
                    },
                    0x1 => {
                        // CSRRW

                        // dont read if rd is 0
                        if rd != 0 {
                            let csr = self.load_csr(csr_addr);
                            trace_field!("csr", csr);

                            self.store_csr(csr_addr, self.regs[rs1]);
                            self.regs[rd] = csr;
                        } else {
                            self.store_csr(csr_addr, self.regs[rs1]);
                        }
                        trace_insn!("CSRRW");
                    }
                    0x2 => {
                        // CSRRS

                        let csr = self.load_csr(csr_addr);
                        trace_field!("csr", csr);
                        trace_insn!("CSRRS");
                        self.regs[rd] = csr;
                        if rs1 != 0 {
                            self.store_csr(csr_addr, csr | self.regs[rs1]);
                        }
                    }
                    0x3 => {
                        // CSRRC
                        let csr = self.load_csr(csr_addr);
                        trace_field!("csr", csr);
                        trace_insn!("CSRRC");
                        self.regs[rd] = csr;
                        if rs1 != 0 {
                            self.store_csr(csr_addr, csr & !self.regs[rs1]);
                        }
                    }
                    0x5 => {
                        // CSRRWI

                        // dont read if rd is 0
                        if rd != 0 {
                            let csr = self.load_csr(csr_addr);
                            trace_field!("csr", csr);
                            self.store_csr(csr_addr, imm);
                            self.regs[rd] = csr;
                        } else {
                            self.store_csr(csr_addr, imm);
                        }
                        trace_insn!("CSRRWI");
                    }
                    0x6 => {
                        // CSRRSI

                        let csr = self.load_csr(csr_addr);
                        trace_field!("csr", csr);

                        self.regs[rd] = csr;
                        if imm != 0 {
                            self.store_csr(csr_addr, csr | imm);
                        }
                        trace_insn!("CSRRWSI");
                    }
                    0x7 => {
                        // CSRRCI

                        let csr = self.load_csr(csr_addr);
                        trace_field!("csr", csr);
                        trace_insn!("CSRRCI");
                        self.regs[rd] = csr;
                        if imm != 0 {
                            self.store_csr(csr_addr, csr & !imm);
                        }
                    }
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            0x2f => {
                // The aq and rl ordering bits, funct7[1] and funct7[0], constrain
                // nothing on a single in-order hart.
                let funct5 = (funct7 >> 2) & 0x1f;
                let size = match funct3 {
                    0b010 => 32,
                    0b011 => 64,
                    _ => return Err(Exception::IllegalInstruction(inst)),
                };
                let addr = self.regs[rs1];
                if addr & (size / 8 - 1) != 0 {
                    return Err(Exception::StoreAmoAddressMisaligned(addr));
                }

                match funct5 {
                    0b00010 => {
                        // lr: load, and reserve a set of bytes subsuming what was read
                        trace_insn!("LR");
                        self.regs[rd] = self.sext(self.bus.load(addr, size)?, size);
                        self.bus.reserve(addr, size);
                    }
                    0b00011 => {
                        // sc: write rs2 only if the reservation still covers these
                        // bytes, leaving zero in rd on success and nonzero on failure
                        trace_insn!("SC");
                        if self.bus.take_reservation(addr, size) {
                            self.bus.store(addr, size, self.regs[rs2])?;
                            self.regs[rd] = 0;
                        } else {
                            self.regs[rd] = 1;
                        }
                    }
                    // The amos load, apply a binary operator to the loaded value and
                    // rs2, and store the result back, returning the loaded value.
                    0b00001 => self.amo(rd, addr, self.regs[rs2], size, "AMOSWAP", |_, src| src)?,
                    0b00000 => self.amo(
                        rd,
                        addr,
                        self.regs[rs2],
                        size,
                        "AMOADD",
                        |data, src| match size {
                            32 => (data as u32).wrapping_add(src as u32) as u64,
                            _ => data.wrapping_add(src),
                        },
                    )?,
                    0b00100 => {
                        self.amo(rd, addr, self.regs[rs2], size, "AMOXOR", |data, src| {
                            data ^ src
                        })?
                    }
                    0b01100 => {
                        self.amo(rd, addr, self.regs[rs2], size, "AMOAND", |data, src| {
                            data & src
                        })?
                    }
                    0b01000 => self.amo(rd, addr, self.regs[rs2], size, "AMOOR", |data, src| {
                        data | src
                    })?,
                    0b10000 => self.amo(
                        rd,
                        addr,
                        self.regs[rs2],
                        size,
                        "AMOMIN",
                        |data, src| match size {
                            32 => (data as i32).min(src as i32) as u32 as u64,
                            _ => (data as i64).min(src as i64) as u64,
                        },
                    )?,
                    0b10100 => self.amo(
                        rd,
                        addr,
                        self.regs[rs2],
                        size,
                        "AMOMAX",
                        |data, src| match size {
                            32 => (data as i32).max(src as i32) as u32 as u64,
                            _ => (data as i64).max(src as i64) as u64,
                        },
                    )?,
                    0b11000 => self.amo(
                        rd,
                        addr,
                        self.regs[rs2],
                        size,
                        "AMOMINU",
                        |data, src| match size {
                            32 => (data as u32).min(src as u32) as u64,
                            _ => data.min(src),
                        },
                    )?,
                    0b11100 => self.amo(
                        rd,
                        addr,
                        self.regs[rs2],
                        size,
                        "AMOMAXU",
                        |data, src| match size {
                            32 => (data as u32).max(src as u32) as u64,
                            _ => data.max(src),
                        },
                    )?,
                    _ => return Err(Exception::IllegalInstruction(inst)),
                }
            }
            _ => return Err(Exception::IllegalInstruction(inst)),
        }

        Ok(())
    }

    pub fn dump_registers(&self) {
        let abi = [
            "zero", " ra ", " sp ", " gp ", " tp ", " t0 ", " t1 ", " t2 ", " s0 ", " s1 ", " a0 ",
            " a1 ", " a2 ", " a3 ", " a4 ", " a5 ", " a6 ", " a7 ", " s2 ", " s3 ", " s4 ", " s5 ",
            " s6 ", " s7 ", " s8 ", " s9 ", " s10", " s11", " t3 ", " t4 ", " t5 ", " t6 ",
        ];

        for (i, r) in self.regs.iter().enumerate() {
            print!("x{:02} ({}) = {:>#18x} | ", i, abi[i], r);
            if (i + 1) % 4 == 0 {
                println!()
            }
        }
        println!()
    }

    pub fn dump_csr(&self) {
        for (i, x) in self
            .csrs
            .iter()
            .enumerate()
            .filter(|x| x.1 != &0)
            .enumerate()
        {
            print!("{:02} = {:>#18x} | ", x.0, x.1);
            if (i + 1) % 4 == 0 {
                println!()
            }
        }
        println!()
    }
}
