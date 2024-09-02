use std::ops::{BitAnd, BitOr, BitXor};

use crate::{
    bus::Bus,
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
}

const MIP: usize = 0x344;
const MIE: usize = 0x304;
const SIP: usize = 0x144;
const SIE: usize = 0x104;
const MEDELEG: usize = 0x302;
const MIDELEG: usize = 0x303;

impl Cpu {
    pub fn new(code: Vec<u8>) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: 0,
            bus: Bus {
                dram: Dram::new(code),
            },
            csrs: [0; 4096],
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_SIZE;

        cpu
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        while let Ok(inst) = self.fetch() {
            self.pc += 4;

            // 3. Decode.
            // 4. Execute.
            while self.execute(inst).is_ok() {}

            // This is a workaround for avoiding an infinite loop.
            if self.pc == 0 {
                break;
            }
        }

        Ok(())
    }

    fn load_csr(&self, addr: usize) -> u64 {
        match addr {
            SIE => self.csrs[MIE] & self.csrs[MIDELEG],
            _ => self.csrs[addr],
        }
    }

    fn store_csr(&mut self, addr: usize, value: u64) {
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

    fn execute(&mut self, inst: u64) -> Result<(), ()> {
        let opcode = inst & 0x7f;
        let rd = ((inst >> 7) & 0x1f) as usize;
        let rs1 = ((inst >> 15) & 0x1f) as usize;
        let rs2 = ((inst >> 20) & 0x1f) as usize;
        let funct3 = (inst >> 12) & 0x7;
        let funct7 = (inst >> 25) & 0x7f;
        match opcode {
            // load
            0x03 => {
                // imm[11:0] = inst[31:20]
                let imm = ((inst as i32 as i64) >> 20) as u64;
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => {
                        // lb
                        self.regs[rd] = self.bus.load(addr, 8)? as i8 as i64 as u64;
                    }
                    0x1 => {
                        // lh
                        self.regs[rd] = self.bus.load(addr, 16)? as i16 as i64 as u64;
                    }
                    0x2 => {
                        // lw
                        self.regs[rd] = self.bus.load(addr, 32)? as i32 as i64 as u64;
                    }
                    0x3 => {
                        // ld
                        self.regs[rd] = self.bus.load(addr, 64)? as i64 as u64;
                    }
                    0x4 => {
                        // lbu
                        self.regs[rd] = self.bus.load(addr, 8)?;
                    }
                    0x5 => {
                        // lhu
                        self.regs[rd] = self.bus.load(addr, 16)?;
                    }
                    0x6 => {
                        // lwu
                        self.regs[rd] = self.bus.load(addr, 32)?;
                    }
                    _ => Err(())?,
                };
            }
            // store
            0x23 => {
                // imm[11:5|4:0] = inst[31:25|11:7]
                let imm = (((inst & 0xfe000000) as i32 as i64 >> 20) as u64) | ((inst >> 7) & 0x1f);
                let addr = self.regs[rs1].wrapping_add(imm);

                match funct3 {
                    0x0 => self.bus.store(addr, 8, self.regs[rs2])?, // sb
                    0x1 => self.bus.store(addr, 16, self.regs[rs2])?, // sh
                    0x2 => self.bus.store(addr, 32, self.regs[rs2])?, // sb
                    0x3 => self.bus.store(addr, 64, self.regs[rs2])?, // sb
                    _ => Err(())?,
                }
            }
            // base imm
            0x13 => {
                let imm = ((inst & 0xfff00000) as i32 as i64 >> 20) as u64;
                let imm04 = rs2;
                match (funct3, funct7) {
                    (0x0, _) => {
                        // addi
                        self.regs[rd] = self.regs[rs1].wrapping_add(imm);
                    }
                    (0x4, _) => {
                        // xori
                        self.regs[rd] = self.regs[rs1].bitxor(imm);
                    }
                    (0x6, _) => {
                        // ori
                        self.regs[rd] = self.regs[rs1].bitor(imm);
                    }
                    (0x7, _) => {
                        // andi
                        self.regs[rd] = self.regs[rs1].bitand(imm);
                    }
                    (0x1, 0x00) => {
                        // slli
                        self.regs[rd] = self.regs[rs1].wrapping_shr(imm04 as u32);
                    }
                    (0x5, 0x00) => {
                        // srli
                        self.regs[rd] = self.regs[rs1].wrapping_shl(imm04 as u32);
                    }
                    (0x5, 0x20) => {
                        // srai
                        self.regs[rd] = (self.regs[rs1] as i64).wrapping_shr(imm04 as u32) as u64;
                    }
                    (0x2, _) => {
                        // slti
                        self.regs[rd] = ((self.regs[rs1] as i64) < (imm as i64)) as u64
                    }
                    (0x3, _) => {
                        // sltiu
                        self.regs[rd] = (self.regs[rs1] < imm) as u64
                    }
                    _ => Err(())?,
                }
            }
            // base R
            0x33 => {
                match (funct3, funct7) {
                    (0x0, 0x0) => {
                        // add
                        self.regs[rd] = self.regs[rs1].wrapping_add(self.regs[rs2]);
                    }
                    (0x0, 0x20) => {
                        // sub
                        self.regs[rd] = self.regs[rs1].wrapping_sub(self.regs[rs2]);
                    }
                    (0x4, 0x0) => {
                        // xor
                        self.regs[rd] = self.regs[rs1].bitxor(self.regs[rs2]);
                    }
                    (0x6, 0x0) => {
                        // and
                        self.regs[rd] = self.regs[rs1].bitand(self.regs[rs2]);
                    }
                    (0x1, 0x0) => {
                        // sll logical
                        self.regs[rd] = self.regs[rs1].wrapping_shl(self.regs[rs2] as u32);
                    }
                    (0x5, 0x0) => {
                        // srl logical
                        self.regs[rd] = self.regs[rs1].wrapping_shr(self.regs[rs2] as u32);
                    }
                    (0x5, 0x20) => {
                        // sra
                        self.regs[rd] =
                            (self.regs[rs1] as i64).wrapping_shr(self.regs[rs2] as u32) as u64;
                    }
                    (0x2, 0x0) => {
                        // slt
                        self.regs[rd] = ((self.regs[rs1] as i64) < (self.regs[rs2] as i64)) as u64
                    }
                    (0x3, 0x0) => {
                        // sltu
                        self.regs[rd] = (self.regs[rs1] < self.regs[rs2]) as u64
                    }
                    _ => Err(())?,
                }
            }

            0x73 => {
                // csr

                todo!();
                match funct3 {
                    _ => Err(())?,
                }
            }

            x => unimplemented!("{:#09b}", x),
        }

        Ok(())
    }

    pub fn dump_registers(&self) {
        for (i, r) in self.regs.iter().enumerate() {
            print!("x{:02} = 0x{:08x},\t\t", i, r);
            if (i + 1) % 4 == 0 {
                println!()
            }
        }
        println!()
    }
}
