use std::time::Instant;

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    bus::{Bus, DRAM_BASE},
    csr::*,
    dram::{DRAM_SIZE, Dram},
    elf::{Error as ElfError, Image},
    exception::Exception,
    inst::{AmoOp, Cond, Inst, Op, Width, decode},
};

#[derive(Debug)]
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
    /// The privilege the hart is running at. A trap raises it, an `xRET` lowers it.
    pub mode: Mode,
    pub start: Instant,
}

impl Cpu {
    pub fn new(code: Vec<u8>) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: DRAM_BASE,
            next_pc: DRAM_BASE,
            bus: Bus::new(Dram::new(code)),
            csrs: [0; 4096],
            mode: Mode::Machine,
            start: Instant::now(),
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_BASE + DRAM_SIZE;
        cpu.csrs[MISA] = MISA_MXL_64
            | misa_extension(b'i')
            | misa_extension(b'm')
            | misa_extension(b'a')
            | misa_extension(b's')
            | misa_extension(b'u');
        cpu.csrs[MSTATUS] = MSTATUS_XL_64;

        cpu
    }

    /// Run until a trap that nothing is installed to handle, and return it. With no
    /// handler in the vector the trap would use there is nowhere for it to go, so that
    /// is where a program ends: normally by running off its own code into the zeroed
    /// dram behind it.
    /// Place an image in memory and start at its entry point.
    pub fn from_elf(image: &Image) -> Result<Self, ElfError> {
        let mut cpu = Self::new(Vec::new());
        for segment in &image.segments {
            if !cpu
                .bus
                .dram
                .write(segment.addr, &segment.bytes, segment.zeroes)
            {
                return Err(ElfError::SegmentOutsideDram(segment.addr));
            }
        }
        cpu.pc = image.entry;
        cpu.next_pc = image.entry;
        Ok(cpu)
    }

    pub fn run(&mut self) -> Exception {
        loop {
            if let Err(exception) = self.step()
                && !self.take_trap(exception)
            {
                return exception;
            }
        }
    }

    /// Fetch, decode and execute one instruction.
    #[inline]
    pub fn step(&mut self) -> Result<(), Exception> {
        let word = self.fetch()?;
        let inst = decode(word)?;
        trace_insn!("{:#x}  {inst}", self.pc);

        self.next_pc = self.pc.wrapping_add(4);
        self.csrs[RDCYCLE] += 1;
        self.csrs[INSTRET] += 1;

        self.execute(inst, word)?;

        self.regs[0] = 0;
        self.pc = self.next_pc;
        Ok(())
    }

    /// Enter a trap handler: record where and why, stack the interrupt-enable bit and
    /// the mode it happened in, and jump through the handler's vector.
    ///
    /// A trap goes to supervisor mode when it happened no higher than there and
    /// `medeleg` delegates its cause, and to machine mode otherwise; a trap never moves
    /// to a mode less privileged than the one it happened in.
    ///
    /// Answers whether anything took it: a zero vector is no handler at all, and the
    /// caller decides what to do with a trap that has nowhere to go.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.6.1, 3.1.7 and 3.1.8.
    pub fn take_trap(&mut self, exception: Exception) -> bool {
        let cause = exception.cause();
        let delegated = self.mode <= Mode::Supervisor && (self.csrs[MEDELEG] >> cause) & 1 == 1;
        let status = self.csrs[MSTATUS];
        if self.csrs[if delegated { STVEC } else { MTVEC }] == 0 {
            return false;
        }

        if delegated {
            self.csrs[SEPC] = self.pc;
            self.csrs[SCAUSE] = cause;
            self.csrs[STVAL] = exception.value();

            // SIE moves to SPIE and clears, and the mode we came from lands in SPP,
            // which is one bit because a supervisor can only be entered from below.
            let sie = (status >> MSTATUS_SIE) & 1;
            self.csrs[MSTATUS] =
                (status & !(1 << MSTATUS_SPIE) & !(1 << MSTATUS_SIE) & !(1 << MSTATUS_SPP))
                    | (sie << MSTATUS_SPIE)
                    | ((self.mode as u64) << MSTATUS_SPP);
            self.mode = Mode::Supervisor;
            self.pc = self.csrs[STVEC] & !0b11;
            return true;
        }

        self.csrs[MEPC] = self.pc;
        self.csrs[MCAUSE] = cause;
        self.csrs[MTVAL] = exception.value();

        // MIE moves to MPIE and clears, and the mode we came from lands in MPP.
        let mie = (status >> MSTATUS_MIE) & 1;
        self.csrs[MSTATUS] = (status & !(1 << MSTATUS_MPIE) & !(1 << MSTATUS_MIE) & !MSTATUS_MPP)
            | (mie << MSTATUS_MPIE)
            | ((self.mode as u64) << MSTATUS_MPP_SHIFT);
        self.mode = Mode::Machine;

        // Vectored mode only spreads interrupts out; exceptions always enter at the base.
        self.pc = self.csrs[MTVEC] & !0b11;
        true
    }

    /// Return from a trap taken into `mode`: pop that mode's interrupt-enable and
    /// privilege stack out of `mstatus` and resume at its `epc`. The popped `xPP`
    /// becomes the least privileged mode there is, which makes a stack-management bug
    /// in the handler visible rather than silent.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.6.1 and 3.3.2.
    fn trap_return(&mut self, mode: Mode) {
        let status = self.csrs[MSTATUS];
        let (previous, epc) = match mode {
            Mode::Machine => {
                let previous = Mode::from_bits((status & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT);
                let mpie = (status >> MSTATUS_MPIE) & 1;
                self.csrs[MSTATUS] = (status & !(1 << MSTATUS_MIE) & !MSTATUS_MPP)
                    | (mpie << MSTATUS_MIE)
                    | (1 << MSTATUS_MPIE);
                (previous, self.csrs[MEPC])
            }
            _ => {
                let previous = Mode::from_bits((status >> MSTATUS_SPP) & 1);
                let spie = (status >> MSTATUS_SPIE) & 1;
                self.csrs[MSTATUS] = (status & !(1 << MSTATUS_SIE) & !(1 << MSTATUS_SPP))
                    | (spie << MSTATUS_SIE)
                    | (1 << MSTATUS_SPIE);
                (previous, self.csrs[SEPC])
            }
        };
        self.mode = previous;
        self.next_pc = epc;
    }

    /// A CSR address carries its own access rules: bits 11:10 hold the encoding
    /// read-only when both are set, and bits 9:8 name the lowest privilege that may
    /// reach it. Neither is an encoding question, so unlike everything `decode`
    /// refuses, this one can only be asked here.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 2.1.
    fn check_csr(&self, addr: usize, word: u32, write: bool) -> Result<(), Exception> {
        let least = ((addr >> 8) & 0b11) as u64;
        if (write && addr >> 10 == 0b11) || (self.mode as u64) < least {
            return Err(Exception::IllegalInstruction(word as u64));
        }
        Ok(())
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    fn load_csr(&self, addr: usize) -> u64 {
        trace_insn!("loading csr");
        if let Some((base, mask, readable)) = alias(addr, self.csrs[MIDELEG]) {
            return self.csrs[base] & (mask | readable);
        }
        match addr {
            RDTIME => self.start.elapsed().as_secs(),
            _ => self.csrs[addr],
        }
    }

    #[cfg_attr(feature = "trace", instrument(skip(self)))]
    fn store_csr(&mut self, addr: usize, value: u64) {
        // misa is writable in principle, to turn extensions off. rysk cannot, and the
        // manual allows ignoring a write that names an unsupported configuration.
        if addr == MISA {
            return;
        }
        trace_insn!("storing csr");
        if let Some((base, mask, _)) = alias(addr, self.csrs[MIDELEG]) {
            self.csrs[base] = (self.csrs[base] & !mask) | (value & mask);
            return;
        }
        match addr {
            // Only supervisor-level interrupts are delegatable, so the rest of mideleg
            // is read-only zero, and what it does hold is what sie and sip may reach.
            // The RISC-V Instruction Set Manual Volume II, 3.1.8.
            MIDELEG => self.csrs[MIDELEG] = value & S_INTERRUPTS,
            MSTATUS => self.csrs[MSTATUS] = warl_mstatus(self.csrs[MSTATUS], value),
            _ => self.csrs[addr] = value,
        }
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
    fn fetch(&mut self) -> Result<u32, Exception> {
        self.bus
            .load(self.pc, 32)
            .map(|word| word as u32)
            .map_err(|_| Exception::InstructionAccessFault(self.pc))
    }

    /// Carry out one decoded instruction.
    fn execute(&mut self, inst: Inst, word: u32) -> Result<(), Exception> {
        let Inst {
            op,
            rd,
            rs1,
            rs2,
            imm,
        } = inst;
        let (a, b) = (self.regs[rs1], self.regs[rs2]);

        match op {
            // ---------------------------------------------------------- integer
            Op::Addi => self.regs[rd] = a.wrapping_add(imm),
            Op::Slti => self.regs[rd] = ((a as i64) < imm as i64) as u64,
            Op::Sltiu => self.regs[rd] = (a < imm) as u64,
            Op::Xori => self.regs[rd] = a ^ imm,
            Op::Ori => self.regs[rd] = a | imm,
            Op::Andi => self.regs[rd] = a & imm,
            Op::Slli => self.regs[rd] = a << imm,
            Op::Srli => self.regs[rd] = a >> imm,
            Op::Srai => self.regs[rd] = ((a as i64) >> imm) as u64,

            Op::Add => self.regs[rd] = a.wrapping_add(b),
            Op::Sub => self.regs[rd] = a.wrapping_sub(b),
            Op::Sll => self.regs[rd] = a << (b & 0x3f),
            Op::Slt => self.regs[rd] = ((a as i64) < b as i64) as u64,
            Op::Sltu => self.regs[rd] = (a < b) as u64,
            Op::Xor => self.regs[rd] = a ^ b,
            Op::Srl => self.regs[rd] = a >> (b & 0x3f),
            Op::Sra => self.regs[rd] = ((a as i64) >> (b & 0x3f)) as u64,
            Op::Or => self.regs[rd] = a | b,
            Op::And => self.regs[rd] = a & b,

            // ---------------------------------------------------------- word forms
            // Every result is sign-extended from bit 31 into the full register.
            Op::Addiw => self.regs[rd] = a.wrapping_add(imm) as i32 as i64 as u64,
            Op::Slliw => self.regs[rd] = ((a as u32) << imm) as i32 as i64 as u64,
            Op::Srliw => self.regs[rd] = ((a as u32) >> imm) as i32 as i64 as u64,
            Op::Sraiw => self.regs[rd] = ((a as i32) >> imm) as i64 as u64,

            Op::Addw => self.regs[rd] = a.wrapping_add(b) as i32 as i64 as u64,
            Op::Subw => self.regs[rd] = a.wrapping_sub(b) as i32 as i64 as u64,
            Op::Sllw => self.regs[rd] = ((a as u32) << (b & 0x1f)) as i32 as i64 as u64,
            Op::Srlw => self.regs[rd] = ((a as u32) >> (b & 0x1f)) as i32 as i64 as u64,
            Op::Sraw => self.regs[rd] = ((a as i32) >> (b & 0x1f)) as i64 as u64,

            // ---------------------------------------------------------- multiply
            Op::Mul => self.regs[rd] = a.wrapping_mul(b),
            Op::Mulh => {
                self.regs[rd] = ((a as i64 as i128).wrapping_mul(b as i64 as i128) >> 64) as u64
            }
            Op::Mulhsu => {
                self.regs[rd] = ((a as i64 as i128).wrapping_mul(b as u128 as i128) >> 64) as u64
            }
            Op::Mulhu => self.regs[rd] = ((a as u128).wrapping_mul(b as u128) >> 64) as u64,
            Op::Mulw => self.regs[rd] = (a as i32).wrapping_mul(b as i32) as i64 as u64,

            // The quotient of a division by zero has all bits set and its remainder is
            // the dividend; signed overflow returns the dividend and no remainder.
            // The RISC-V Instruction Set Manual Volume I, 12.2, table 11.
            Op::Div => self.regs[rd] = Self::div(a as i64, b as i64) as u64,
            Op::Rem => self.regs[rd] = Self::rem(a as i64, b as i64) as u64,
            Op::Divu => self.regs[rd] = a.checked_div(b).unwrap_or(u64::MAX),
            Op::Remu => self.regs[rd] = a.checked_rem(b).unwrap_or(a),
            Op::Divw => {
                self.regs[rd] = Self::div(a as i32 as i64, b as i32 as i64) as i32 as i64 as u64
            }
            Op::Remw => {
                self.regs[rd] = Self::rem(a as i32 as i64, b as i32 as i64) as i32 as i64 as u64
            }
            Op::Divuw => {
                let (a, b) = (a as u32, b as u32);
                self.regs[rd] = a.checked_div(b).unwrap_or(u32::MAX) as i32 as i64 as u64
            }
            Op::Remuw => {
                let (a, b) = (a as u32, b as u32);
                self.regs[rd] = a.checked_rem(b).unwrap_or(a) as i32 as i64 as u64
            }

            // ---------------------------------------------------------- zicond
            Op::CzeroEqz => self.regs[rd] = if b == 0 { 0 } else { a },
            Op::CzeroNez => self.regs[rd] = if b != 0 { 0 } else { a },

            // ---------------------------------------------------------- memory
            Op::Load { width, signed } => {
                let value = self.bus.load(a.wrapping_add(imm), width.bits())?;
                self.regs[rd] = if signed {
                    Self::sext(value, width.bits())
                } else {
                    value
                };
            }
            Op::Store { width } => self.bus.store(a.wrapping_add(imm), width.bits(), b)?,

            // ---------------------------------------------------------- control
            Op::Lui => self.regs[rd] = imm,
            Op::Auipc => self.regs[rd] = self.pc.wrapping_add(imm),
            Op::Jal => {
                let link = self.next_pc;
                self.branch(imm)?;
                self.regs[rd] = link;
            }
            Op::Jalr => {
                // The target comes from rs1's value before the link is written, since
                // rd and rs1 are commonly the same register.
                let target = a.wrapping_add(imm) & !1;
                let link = self.next_pc;
                self.jump(target)?;
                self.regs[rd] = link;
            }
            Op::Branch { cond } => {
                let taken = match cond {
                    Cond::Eq => a == b,
                    Cond::Ne => a != b,
                    Cond::Lt => (a as i64) < b as i64,
                    Cond::Ge => (a as i64) >= b as i64,
                    Cond::Ltu => a < b,
                    Cond::Geu => a >= b,
                };
                if taken {
                    self.branch(imm)?;
                }
            }

            // ---------------------------------------------------------- zicsr
            // A csr is read before it is written, and left alone when the instruction
            // names no source: x0 for the register forms, zero for the immediate ones.
            Op::Csrrw { immediate } => {
                let source = if immediate { rs1 as u64 } else { a };
                self.check_csr(imm as usize, word, true)?;
                // With nowhere to put the result there is no read at all.
                if rd != 0 {
                    self.regs[rd] = self.load_csr(imm as usize);
                }
                self.store_csr(imm as usize, source);
            }
            Op::Csrrs { immediate } | Op::Csrrc { immediate } => {
                let source = if immediate { rs1 as u64 } else { a };
                // Naming no bits to change is not a write, so a read-only csr is still
                // readable this way.
                self.check_csr(imm as usize, word, rs1 != 0)?;
                let csr = self.load_csr(imm as usize);
                // A source of x0, or of zero for the immediate forms, names no bits to
                // change, and then the csr is not written at all.
                if rs1 != 0 {
                    let value = match op {
                        Op::Csrrs { .. } => csr | source,
                        _ => csr & !source,
                    };
                    self.store_csr(imm as usize, value);
                }
                self.regs[rd] = csr;
            }

            // ---------------------------------------------------------- privileged
            Op::Ecall => return Err(Exception::EnvironmentCall(self.mode)),
            Op::Ebreak => return Err(Exception::Breakpoint(self.pc)),
            // An xRET below the mode it returns from has no stack to pop.
            // The RISC-V Instruction Set Manual Volume II, 3.3.2.
            Op::Mret | Op::Sret => {
                let mode = if op == Op::Mret {
                    Mode::Machine
                } else {
                    Mode::Supervisor
                };
                if self.mode < mode {
                    return Err(Exception::IllegalInstruction(word as u64));
                }
                self.trap_return(mode);
            }
            // Nothing can raise an interrupt yet, so waiting for one would never end.
            // Retiring immediately is permitted.
            Op::Wfi => {}
            // One in-order hart observes its own accesses in order, and there is no
            // instruction cache to keep coherent.
            Op::Fence | Op::FenceI => {}

            // ---------------------------------------------------------- atomics
            Op::Lr { width } | Op::Sc { width } | Op::Amo { width, .. } => {
                let bits = width.bits();
                if a & (bits / 8 - 1) != 0 {
                    return Err(Exception::StoreAmoAddressMisaligned(a));
                }
                match op {
                    Op::Lr { .. } => {
                        self.regs[rd] = Self::sext(self.bus.load(a, bits)?, bits);
                        self.bus.reserve(a, bits);
                    }
                    Op::Sc { .. } => {
                        // The store happens only if the reservation still covers these
                        // bytes, and rd reports zero for success, nonzero for failure.
                        self.regs[rd] = if self.bus.take_reservation(a, bits) {
                            self.bus.store(a, bits, b)?;
                            0
                        } else {
                            1
                        };
                    }
                    Op::Amo { op, .. } => {
                        let data = self.bus.load(a, bits)?;
                        let value = Self::amo(op, width, data, b);
                        self.bus.store(a, bits, value)?;
                        self.regs[rd] = Self::sext(data, bits);
                    }
                    _ => unreachable!(),
                }
            }
        }

        Ok(())
    }

    /// The quotient the manual specifies, including division by zero and the one
    /// signed case that overflows.
    #[inline]
    fn div(a: i64, b: i64) -> i64 {
        match b {
            0 => -1,
            _ => a.wrapping_div(b),
        }
    }

    /// The matching remainder: the dividend when there is no divisor, and none when
    /// the division overflows.
    #[inline]
    fn rem(a: i64, b: i64) -> i64 {
        match b {
            0 => a,
            _ => a.wrapping_rem(b),
        }
    }

    /// Widen a value loaded from memory to the full register, sign-extending anything
    /// narrower than XLEN.
    #[inline]
    fn sext(value: u64, bits: u64) -> u64 {
        match bits {
            8 => value as i8 as i64 as u64,
            16 => value as i16 as i64 as u64,
            32 => value as i32 as i64 as u64,
            _ => value,
        }
    }

    /// The read-modify-write an atomic performs, at the width it performs it.
    fn amo(op: AmoOp, width: Width, data: u64, src: u64) -> u64 {
        if width == Width::Word {
            let (d, s) = (data as u32, src as u32);
            return match op {
                AmoOp::Swap => s,
                AmoOp::Add => d.wrapping_add(s),
                AmoOp::Xor => d ^ s,
                AmoOp::And => d & s,
                AmoOp::Or => d | s,
                AmoOp::Min => (d as i32).min(s as i32) as u32,
                AmoOp::Max => (d as i32).max(s as i32) as u32,
                AmoOp::MinU => d.min(s),
                AmoOp::MaxU => d.max(s),
            } as u64;
        }
        match op {
            AmoOp::Swap => src,
            AmoOp::Add => data.wrapping_add(src),
            AmoOp::Xor => data ^ src,
            AmoOp::And => data & src,
            AmoOp::Or => data | src,
            AmoOp::Min => (data as i64).min(src as i64) as u64,
            AmoOp::Max => (data as i64).max(src as i64) as u64,
            AmoOp::MinU => data.min(src),
            AmoOp::MaxU => data.max(src),
        }
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
