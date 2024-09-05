use std::{
    ops::{BitAnd, BitOr, BitXor},
    time::Instant,
};

use tracing::{debug, error, instrument};

use crate::{
    bus::{Bus, DRAM_BASE},
    dram::{Dram, DRAM_SIZE},
};

#[derive(Debug, Clone)]
pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    pub bus: Bus,
    /// Control and status registers. RISC-V ISA sets aside a 12-bit encoding
    /// space (csr[11:0]) for up to 4096 CSRs.
    pub csrs: [u64; 4096],
    pub start: Instant,
}

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
            bus: Bus {
                dram: Dram::new(code),
            },
            csrs: [0; 4096],
            start: Instant::now(),
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_BASE + DRAM_SIZE;

        cpu
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        while let Ok(inst) = self.fetch() {
            self.pc += 4;

            // Update counters
            self.csrs[RDCYCLE] += 1;
            self.csrs[INSTRET] += 1;
            self.csrs[RDTIME] = self.start.elapsed().as_secs();

            // 3. Decode.
            // 4. Execute.
            if self.execute(inst).is_err() {
                break;
            }

            self.regs[0] = 0;

            // This is a workaround for avoiding an infinite loop.
            if self.pc == 0 {
                break;
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    fn load_csr(&self, addr: usize) -> u64 {
        debug!("loading csr");
        match addr {
            SIE => self.csrs[MIE] & self.csrs[MIDELEG],
            _ => self.csrs[addr],
        }
    }

    #[instrument(skip(self))]
    fn store_csr(&mut self, addr: usize, value: u64) {
        debug!("storing csr");
        match addr {
            SIE => {
                self.csrs[MIE] =
                    (self.csrs[MIE] & !self.csrs[MIDELEG]) | (value & self.csrs[MIDELEG]);
            }
            _ => self.csrs[addr] = value,
        }
    }

    #[inline]
    fn fetch(&self) -> Result<u64, ()> {
        self.bus.load(self.pc, 32)
    }

    #[instrument(
        skip(self),
        fields(opcode, rd, rs1, rs2, funct3, funct7, imm, shamt, csr, csr_addr)
    )]
    fn execute(&mut self, inst: u64) -> Result<(), ()> {
        let opcode = inst & 0x7f;
        let rd = ((inst >> 7) & 0x1f) as usize;
        let rs1 = ((inst >> 15) & 0x1f) as usize;
        let rs2 = ((inst >> 20) & 0x1f) as usize;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;

        tracing::Span::current().record("opcode", opcode);
        tracing::Span::current().record("rd", rd);
        tracing::Span::current().record("rs1", rs1);
        tracing::Span::current().record("rs2", rs2);
        tracing::Span::current().record("funct3", funct3);
        tracing::Span::current().record("funct7", funct7);

        match opcode {
            // load
            0x03 => {
                // imm[11:0] = inst[31:20]
                let imm = ((inst as i32 as i64) >> 20) as u64;
                tracing::Span::current().record("imm", imm);
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        // lb
                        debug!("LB");
                        self.regs[rd] = self.bus.load(addr, 8)? as i8 as i64 as u64;
                    }
                    0x1 => {
                        // lh
                        debug!("LH");
                        self.regs[rd] = self.bus.load(addr, 16)? as i16 as i64 as u64;
                    }
                    0x2 => {
                        // lw
                        debug!("LW");
                        self.regs[rd] = self.bus.load(addr, 32)? as i32 as i64 as u64;
                    }
                    0x3 => {
                        // ld
                        debug!("LD");
                        self.regs[rd] = self.bus.load(addr, 64)? as i64 as u64;
                    }
                    0x4 => {
                        // lbu
                        debug!("LBU");
                        self.regs[rd] = self.bus.load(addr, 8)?;
                    }
                    0x5 => {
                        // lhu
                        debug!("LHU");
                        self.regs[rd] = self.bus.load(addr, 16)?;
                    }
                    0x6 => {
                        // lwu
                        debug!("LWU");
                        self.regs[rd] = self.bus.load(addr, 32)?;
                    }
                    _ => Err(())?,
                };
            }
            // store
            0x23 => {
                // imm[11:5|4:0] = inst[31:25|11:7]
                let imm = (((inst & 0xfe000000) as i32 as i64 >> 20) as u64) | ((inst >> 7) & 0x1f);
                tracing::Span::current().record("imm", imm);
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        debug!("SB");
                        self.bus.store(addr, 8, self.regs[rs2])?
                    }
                    0x1 => {
                        debug!("SH");
                        self.bus.store(addr, 16, self.regs[rs2])?
                    }
                    0x2 => {
                        debug!("SW");
                        self.bus.store(addr, 32, self.regs[rs2])?
                    }
                    0x3 => {
                        debug!("SD");
                        self.bus.store(addr, 64, self.regs[rs2])?
                    }
                    _ => Err(())?,
                }
            }
            // base imm
            0x13 => {
                let imm = ((inst & 0xfff00000) as i32 as i64 >> 20) as u64;
                tracing::Span::current().record("imm", imm);

                // "The shift amount is encoded in the lower 6 bits of the I-immediate field for RV64I."
                let shamt = (imm & 0x3f) as u32;
                tracing::Span::current().record("shamt", shamt);

                match (funct3, funct7) {
                    (0x0, _) => {
                        // addi
                        debug!("ADDI");
                        self.regs[rd] = self.regs[rs1].wrapping_add(imm);
                    }
                    (0x4, _) => {
                        // xori
                        debug!("XORI");
                        self.regs[rd] = self.regs[rs1].bitxor(imm);
                    }
                    (0x6, _) => {
                        // ori
                        debug!("ORI");
                        self.regs[rd] = self.regs[rs1].bitor(imm);
                    }
                    (0x7, _) => {
                        // andi
                        debug!("ANDI");
                        self.regs[rd] = self.regs[rs1].bitand(imm);
                    }
                    (0x1, 0x00) => {
                        // slli
                        debug!("SLLI");
                        self.regs[rd] = self.regs[rs1].wrapping_shr(shamt);
                    }
                    (0x5, 0x00) => {
                        // srli
                        debug!("SRLI");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt);
                    }
                    (0x5, 0x20) => {
                        // srai
                        debug!("SRAI");
                        self.regs[rd] = (self.regs[rs1] as i64).wrapping_shr(shamt) as u64;
                    }
                    (0x2, _) => {
                        // slti
                        debug!("SLTI");
                        self.regs[rd] = ((self.regs[rs1] as i64) < (imm as i64)) as u64
                    }
                    (0x3, _) => {
                        // sltiu
                        debug!("SLTIU");
                        self.regs[rd] = (self.regs[rs1] < imm) as u64
                    }
                    _ => Err(())?,
                }
            }
            // base R
            0x33 => {
                // In RV64I, only the low 6 bits of rs2 are considered for the shift amount."
                let shamt = (self.regs[rs2] & 0x3f) as u32;
                tracing::Span::current().record("shamt", shamt);

                match (funct3, funct7) {
                    (0x0, 0x0) => {
                        // add
                        debug!("ADD");
                        self.regs[rd] = self.regs[rs1].wrapping_add(self.regs[rs2]);
                    }
                    (0x0, 0x20) => {
                        // sub
                        debug!("SUB");
                        self.regs[rd] = self.regs[rs1].wrapping_sub(self.regs[rs2]);
                    }
                    (0x4, 0x0) => {
                        // xor
                        debug!("XOR");
                        self.regs[rd] = self.regs[rs1].bitxor(self.regs[rs2]);
                    }
                    (0x6, 0x0) => {
                        // and
                        debug!("AND");
                        self.regs[rd] = self.regs[rs1].bitand(self.regs[rs2]);
                    }
                    (0x1, 0x0) => {
                        // sll logical
                        debug!("SLL");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt);
                    }
                    (0x5, 0x0) => {
                        // srl logical
                        debug!("SRL");
                        self.regs[rd] = self.regs[rs1].wrapping_shr(shamt);
                    }
                    (0x5, 0x20) => {
                        // sra
                        debug!("SRA");
                        self.regs[rd] = (self.regs[rs1] as i64).wrapping_shr(shamt) as u64;
                    }
                    (0x2, 0x0) => {
                        // slt
                        debug!("SLT");
                        self.regs[rd] = ((self.regs[rs1] as i64) < (self.regs[rs2] as i64)) as u64
                    }
                    (0x3, 0x0) => {
                        // sltu
                        debug!("SLTU");
                        self.regs[rd] = (self.regs[rs1] < self.regs[rs2]) as u64
                    }
                    _ => Err(())?,
                }
            }
            0x3b => {
                // addw and family
                let shamt = (self.regs[rs2] & 0x1f) as u32;
                match (funct3, funct7) {
                    (0x0, 0x0) => {
                        debug!("ADDW");
                        self.regs[rd] =
                            self.regs[rs1].wrapping_add(self.regs[rs2]) as i32 as i64 as u64;
                    }
                    (0x0, 0x20) => {
                        debug!("SUBW");
                        self.regs[rd] =
                            self.regs[rs1].wrapping_sub(self.regs[rs2]) as i32 as i64 as u64;
                    }
                    (0x1, 0x00) => {
                        debug!("SLLW");
                        self.regs[rd] = (self.regs[rs1] as u32).wrapping_shl(shamt) as i32 as u64;
                    }
                    (0x5, 0x00) => {
                        debug!("SRLW");
                        self.regs[rd] = (self.regs[rs1] as u32).wrapping_shr(shamt) as i32 as u64;
                    }
                    (0x5, 0x20) => {
                        debug!("SRAW");
                        self.regs[rd] = ((self.regs[rs1] as i32) >> (shamt as i32)) as u64;
                    }
                    _ => {
                        error!("unimplemented instruction");
                        unimplemented!("{:#09b} {:#03b} {:#03b}", inst, funct3, funct7)
                    }
                }
            }
            0x1b => {
                // addiw and family

                let imm = ((inst as i32 as i64) >> 20) as u64;
                let shamt = (imm & 0x1f) as u32;

                match (funct3, funct7) {
                    (0x0, _) => {
                        tracing::Span::current().record("imm", imm);
                        debug!("ADDIW");
                        self.regs[rd] = self.regs[rs1].wrapping_add(imm) as i32 as i64 as u64;
                    }
                    (0x1, _) => {
                        tracing::Span::current().record("shamt", shamt);
                        debug!("SLLIW");
                        self.regs[rd] = self.regs[rs1].wrapping_shl(shamt) as i32 as i64 as u64;
                    }
                    (0x5, 0) => {
                        tracing::Span::current().record("shamt", shamt);
                        debug!("SRLIW");
                        self.regs[rd] =
                            (self.regs[rs1] as u32).wrapping_shr(shamt) as i32 as i64 as u64;
                    }
                    (0x5, 0x20) => {
                        tracing::Span::current().record("shamt", shamt);
                        debug!("SRAIW");
                        self.regs[rd] = (self.regs[rs1] as i32).wrapping_shr(shamt) as i64 as u64;
                    }
                    _ => {
                        error!("unimplemented instruction");
                        unimplemented!("{:#09b} {:#03b} {:#03b}", inst, funct3, funct7)
                    }
                }
            }
            0x63 => {
                // branching
                // imm[12|10:5|4:1|11] = inst[31|30:25|11:8|7]
                let imm = (((inst & 0x80000000) as i32 as i64 >> 19) as u64)
                    | ((inst & 0x80) << 4) // imm[11]
                    | ((inst >> 20) & 0x7e0) // imm[10:5]
                    | ((inst >> 7) & 0x1e); // imm[4:1]
                tracing::Span::current().record("imm", imm);

                match funct3 {
                    0x0 => {
                        debug!("BEQ");

                        if self.regs[rs1] == self.regs[rs2] {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    0x1 => {
                        debug!("BNE");

                        if self.regs[rs1] != self.regs[rs2] {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    0x4 => {
                        debug!("BLT");

                        if (self.regs[rs1] as i64) < (self.regs[rs2] as i64) {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    0x5 => {
                        debug!("BGE");

                        if (self.regs[rs1] as i64) >= (self.regs[rs2] as i64) {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    0x6 => {
                        debug!("BLTU");

                        if self.regs[rs1] < self.regs[rs2] {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    0x7 => {
                        debug!("BGEU");

                        if self.regs[rs1] >= self.regs[rs2] {
                            self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
                        }
                    }
                    x => {
                        error!("unimplemented instruction");
                        unimplemented!("{:#09b} {:#03b}", x, funct3)
                    }
                }
            }
            0x37 => {
                // LUI
                let imm32 = (inst & 0xfffff000) as i32 as i64 as u64;
                tracing::Span::current().record("imm", imm32);
                debug!("LUI");
                self.regs[rd] = imm32;
            }
            0x17 => {
                // AUIPC
                let imm32 = (inst & 0xfffff000) as i32 as i64 as u64;
                tracing::Span::current().record("imm", imm32);
                debug!("AUIPC");
            }
            0x6f => {
                // JAL
                // imm[20|10:1|11|19:12] = inst[31|30:21|20|19:12]
                let imm = (((inst & 0x80000000) as i32 as i64 >> 11) as u64) // imm[20]
                    | (inst & 0xff000) // imm[19:12]
                    | ((inst >> 9) & 0x800) // imm[11]
                    | ((inst >> 20) & 0x7fe); // imm[10:1]
                tracing::Span::current().record("imm", imm);
                debug!("JAL");
                self.regs[rd] = self.pc;
                self.pc = self.pc.wrapping_add(imm).wrapping_sub(4);
            }
            0x67 => {
                // JALR
                let imm = ((((inst & 0xfff00000) as i32) as i64) >> 20) as u64;
                tracing::Span::current().record("imm", imm);

                self.regs[rd] = self.pc;
                let addr = self.regs[rs1].wrapping_add(imm) & !1;
                self.pc = addr;
                debug!("JALR");
            }
            0x73 => {
                // csr
                let csr_addr = ((inst & 0xfff00000) >> 20) as usize;
                tracing::Span::current().record("csr_addr", csr_addr);
                let imm = rs1 as u64;
                match funct3 {
                    0x1 => {
                        // CSRRW

                        // dont read if rd is 0
                        if rd != 0 {
                            let csr = self.load_csr(csr_addr);
                            tracing::Span::current().record("csr", csr);

                            self.store_csr(csr_addr, self.regs[rs1]);
                            self.regs[rd] = csr;
                        } else {
                            self.store_csr(csr_addr, self.regs[rs1]);
                        }
                        debug!("CSRRW");
                    }
                    0x2 => {
                        // CSRRS

                        let csr = self.load_csr(csr_addr);
                        tracing::Span::current().record("csr", csr);
                        debug!("CSRRS");
                        self.regs[rd] = csr;
                        if rs1 != 0 {
                            self.store_csr(csr_addr, csr | self.regs[rs1]);
                        }
                    }
                    0x3 => {
                        // CSRRC
                        let csr = self.load_csr(csr_addr);
                        tracing::Span::current().record("csr", csr);
                        debug!("CSRRC");
                        self.regs[rd] = csr;
                        if rs1 != 0 {
                            self.store_csr(csr_addr, csr & self.regs[rs1]);
                        }
                    }
                    0x5 => {
                        // CSRRWI

                        // dont read if rd is 0
                        if rd != 0 {
                            let csr = self.load_csr(csr_addr);
                            tracing::Span::current().record("csr", csr);
                            self.store_csr(csr_addr, imm);
                            self.regs[rd] = csr;
                        } else {
                            self.store_csr(csr_addr, imm);
                        }
                        debug!("CSRRWI");
                    }
                    0x6 => {
                        // CSRRSI

                        let csr = self.load_csr(csr_addr);
                        tracing::Span::current().record("csr", csr);

                        self.regs[rd] = csr;
                        if imm != 0 {
                            self.store_csr(csr_addr, csr | imm);
                        }
                        debug!("CSRRWSI");
                    }
                    0x7 => {
                        // CSRRCI

                        let csr = self.load_csr(csr_addr);
                        tracing::Span::current().record("csr", csr);
                        debug!("CSRRCI");
                        self.regs[rd] = csr;
                        if imm != 0 {
                            self.store_csr(csr_addr, csr & imm);
                        }
                    }
                    _ => Err(())?,
                }
            }
            0 => Err(())?,

            x => {
                error!("unimplemented instruction");
                unimplemented!("{:#09b}", x)
            }
        }

        // page 554

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
