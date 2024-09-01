use crate::{
    bus::Bus,
    dram::{Dram, DRAM_SIZE},
};

#[derive(Debug, Clone)]
pub struct Cpu {
    pub regs: [u64; 32],
    pub pc: u64,
    pub bus: Bus,
}

impl Cpu {
    pub fn new(code: Vec<u8>) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: 0,
            bus: Bus {
                dram: Dram::new(code),
            },
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_SIZE;

        cpu
    }

    pub fn run(&mut self) -> Result<(), std::io::Error> {
        loop {
            let inst = match self.fetch() {
                Ok(inst) => inst,
                Err(_) => break,
            };

            self.pc += 4;

            // 3. Decode.
            // 4. Execute.
            match self.execute(inst) {
                // Break the loop if an error occurs.
                Ok(_) => {}
                Err(_) => break,
            }

            // This is a workaround for avoiding an infinite loop.
            if self.pc == 0 {
                break;
            }
        }

        Ok(())
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
                        self.regs[rd] = self.bus.load(addr, 32)? as i64 as u64;
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
            // addi
            0x13 => {
                let imm = ((inst & 0xfff00000) as i32 as i64 >> 20) as u64;
                self.regs[rd] = self.regs[rs1].wrapping_add(imm);
            }
            // add
            0x33 => {
                self.regs[rd] = self.regs[rs1].wrapping_add(self.regs[rs2]);
            }

            x => unimplemented!("{:#09b}", x),
        }

        Ok(())
    }

    pub fn dump_registers(&self) {
        for (i, r) in self.regs.iter().enumerate() {
            print!("r{:02} = 0x{:08x},\t\t", i, r);
            if (i + 1) % 4 == 0 {
                println!()
            }
        }
        println!()
    }
}
