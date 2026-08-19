use std::time::Instant;

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    bus::{Bus, DRAM_BASE},
    csr::*,
    dram::{DRAM_SIZE, Dram},
    elf::{Error as ElfError, Image},
    inst::{self, AmoOp, CasWidth, Cond, Inst, Op, Width, decode},
    rvc,
    trap::{Exception, Interrupt, Trap},
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
            | misa_extension(b'c')
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

    pub fn run(&mut self) -> Trap {
        loop {
            if let Some(interrupt) = self.interrupt() {
                let trap = Trap::Interrupt(interrupt);
                if !self.take_trap(trap) {
                    return trap;
                }
            }
            if let Err(exception) = self.step()
                && !self.take_trap(exception.into())
            {
                return exception.into();
            }
        }
    }

    /// Refresh the bits of `mip` that a device drives. They are not storage software
    /// writes: each one is asserted for exactly as long as its device asserts it.
    fn refresh_mip(&mut self) {
        self.csrs[MIP] = (self.csrs[MIP] & !MIP_DEVICE) | self.bus.interrupts();
    }

    /// The interrupt to take before the next instruction, if there is one: the highest
    /// priority one that is pending, enabled, and not held off by the mode it targets.
    ///
    /// Asking here rather than mid-instruction is what makes `mepc` right for an
    /// interrupt with no special case, since `pc` is still the instruction that has not
    /// run. It also satisfies the requirement to re-evaluate immediately after an
    /// `xRET` or a write to `mip`, `mie`, `mstatus` or `mideleg`, since every one of
    /// those retires before the next time round.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9 and 12.1.3.
    pub fn interrupt(&mut self) -> Option<Interrupt> {
        // Asking the devices costs a read of the host clock, and it cannot change the
        // answer unless one of the bits they drive is enabled, so when none is the
        // question is answered out of `mip` alone. Software still sees the true value:
        // reading the register is what refreshes it.
        if self.csrs[MIE] & MIP_DEVICE != 0 {
            self.refresh_mip();
        }
        let ready = self.csrs[MIP] & self.csrs[MIE];
        if ready == 0 {
            return None;
        }
        Interrupt::PRIORITY.into_iter().find(|interrupt| {
            let bit = 1 << *interrupt as u64;
            if ready & bit == 0 {
                return false;
            }
            // An interrupt is enabled for the mode it targets when the hart is below
            // that mode, or is in it with that mode's global enable set. A more
            // privileged mode's interrupts are never held off by a less privileged one.
            let (target, enable) = if self.csrs[MIDELEG] & bit == 0 {
                (Mode::Machine, MSTATUS_MIE)
            } else {
                (Mode::Supervisor, MSTATUS_SIE)
            };
            self.mode < target || (self.mode == target && (self.csrs[MSTATUS] >> enable) & 1 == 1)
        })
    }

    /// Fetch, decode and execute one instruction.
    #[inline]
    pub fn step(&mut self) -> Result<(), Exception> {
        let (inst, encoding) = self.fetch()?;
        trace_insn!("{:#x}  {inst}", self.pc);

        self.next_pc = self.pc.wrapping_add(inst::length(encoding as u16));
        self.csrs[RDCYCLE] += 1;
        self.csrs[INSTRET] += 1;

        self.execute(inst, encoding)?;

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
    pub fn take_trap(&mut self, trap: Trap) -> bool {
        let code = trap.code();
        let delegate = if trap.is_interrupt() {
            self.csrs[MIDELEG]
        } else {
            self.csrs[MEDELEG]
        };
        let delegated = self.mode <= Mode::Supervisor && (delegate >> code) & 1 == 1;
        let status = self.csrs[MSTATUS];
        let tvec = self.csrs[if delegated { STVEC } else { MTVEC }];
        if tvec == 0 {
            return false;
        }
        // Vectored mode spreads the interrupts out over one entry each; an exception
        // enters at the base whichever mode the vector is in.
        // The RISC-V Instruction Set Manual Volume II, 3.1.7.
        let entry = match tvec & 0b11 {
            1 if trap.is_interrupt() => (tvec & !0b11) + 4 * code,
            _ => tvec & !0b11,
        };

        if delegated {
            self.csrs[SEPC] = self.pc;
            self.csrs[SCAUSE] = trap.cause();
            self.csrs[STVAL] = trap.value();

            // SIE moves to SPIE and clears, and the mode we came from lands in SPP,
            // which is one bit because a supervisor can only be entered from below.
            let sie = (status >> MSTATUS_SIE) & 1;
            self.csrs[MSTATUS] =
                (status & !(1 << MSTATUS_SPIE) & !(1 << MSTATUS_SIE) & !(1 << MSTATUS_SPP))
                    | (sie << MSTATUS_SPIE)
                    | ((self.mode as u64) << MSTATUS_SPP);
            self.mode = Mode::Supervisor;
            self.pc = entry;
            return true;
        }

        self.csrs[MEPC] = self.pc;
        self.csrs[MCAUSE] = trap.cause();
        self.csrs[MTVAL] = trap.value();

        // MIE moves to MPIE and clears, and the mode we came from lands in MPP.
        let mie = (status >> MSTATUS_MIE) & 1;
        self.csrs[MSTATUS] = (status & !(1 << MSTATUS_MPIE) & !(1 << MSTATUS_MIE) & !MSTATUS_MPP)
            | (mie << MSTATUS_MPIE)
            | ((self.mode as u64) << MSTATUS_MPP_SHIFT);
        self.mode = Mode::Machine;

        self.pc = entry;
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
    fn load_csr(&mut self, addr: usize) -> u64 {
        trace_insn!("loading csr");
        // The device-driven bits of mip are wires, so reading them is reading the
        // devices rather than anything software last wrote.
        if addr == MIP || addr == SIP {
            self.refresh_mip();
        }
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
            // The low bit of an exception program counter is read-only zero, since no
            // instruction begins at an odd address. It is the only way a trap return
            // could have produced one, compressed instructions having made every other
            // two-byte target legal.
            // The RISC-V Instruction Set Manual Volume II, 3.1.14.
            MEPC | SEPC => self.csrs[addr] = value & !1,
            // The bits a device drives are read-only here: a write cannot argue with a
            // wire. The RISC-V Instruction Set Manual Volume II, 3.1.9.
            MIP => self.csrs[MIP] = (self.csrs[MIP] & MIP_DEVICE) | (value & !MIP_DEVICE),
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
        // Compressed instructions make IALIGN sixteen, so only an odd address is
        // misaligned. Nothing can reach one: `jalr` clears the bit, a `jal` or branch
        // immediate has none, and an exception program counter cannot hold one either.
        // The check stands for the rule rather than for a case that arises.
        if addr & 1 != 0 {
            return Err(Exception::InstructionAddressMisaligned(addr));
        }
        self.next_pc = addr;
        Ok(())
    }

    /// Read the instruction at `pc`, and hand back its encoding along with it: a trap
    /// this instruction raises owes that encoding to `mtval`, and how long it was is
    /// read back out of it.
    ///
    /// It arrives a halfword at a time because its length is in its first two bytes: a
    /// compressed instruction can sit in the last two bytes of memory, and reading four
    /// there would fault on bytes it does not have.
    #[inline]
    fn fetch(&mut self) -> Result<(Inst, u32), Exception> {
        let half = self.halfword(self.pc)?;
        if inst::length(half) == 2 {
            return Ok((rvc::decode(half)?, half as u32));
        }
        let word = half as u32 | (self.halfword(self.pc + 2)? as u32) << 16;
        Ok((decode(word)?, word))
    }

    #[inline]
    fn halfword(&mut self, addr: u64) -> Result<u16, Exception> {
        self.bus
            .load(addr, 16)
            .map(|half| half as u16)
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
            // Stall until something is pending and enabled, whatever `mstatus` says
            // about taking it: the hart waits, it does not enter a handler. The wait is
            // inside the instruction rather than a re-execution of it, because `wfi`
            // retires either way and the trap is taken on the instruction after it, so
            // that returning from the handler resumes past the wait. A hart waiting on
            // an interrupt nothing can deliver waits forever, which is what the
            // hardware does too.
            //
            // The devices advance with the wall clock rather than with retired
            // instructions, so there is something to wait for.
            //
            // The RISC-V Instruction Set Manual Volume II, 3.3.3.
            Op::Wfi => {
                self.refresh_mip();
                while self.csrs[MIP] & self.csrs[MIE] == 0 {
                    std::hint::spin_loop();
                    self.refresh_mip();
                }
            }
            // One in-order hart observes its own accesses in order, and there is no
            // instruction cache to keep coherent.
            Op::Fence | Op::FenceI => {}

            // ---------------------------------------------------------- atomics
            // Compare-and-swap: load, compare against what `rd` holds, and store only
            // if they match. `rd` takes the value that was there either way, so a
            // caller learns whether it won without a second load.
            //
            // The RISC-V Instruction Set Manual Volume I, 15.1.
            Op::AmoCas { width } => {
                let bytes = match width {
                    CasWidth::Word => 4,
                    CasWidth::Double => 8,
                    CasWidth::Quad => 16,
                };
                if a & (bytes - 1) != 0 {
                    return Err(Exception::StoreAmoAddressMisaligned(a));
                }
                match width {
                    // A pair beginning at x0 reads as zero at both halves, and one
                    // named as the destination discards the result entirely rather
                    // than writing only its odd register.
                    CasWidth::Quad => {
                        let (low, high) = (self.bus.load(a, 64)?, self.bus.load(a + 8, 64)?);
                        let compare = if rd == 0 {
                            (0, 0)
                        } else {
                            (self.regs[rd], self.regs[rd + 1])
                        };
                        let swap = if rs2 == 0 {
                            (0, 0)
                        } else {
                            (b, self.regs[rs2 + 1])
                        };
                        if (low, high) == compare {
                            self.bus.store(a, 64, swap.0)?;
                            self.bus.store(a + 8, 64, swap.1)?;
                        }
                        if rd != 0 {
                            self.regs[rd] = low;
                            self.regs[rd + 1] = high;
                        }
                    }
                    _ => {
                        let bits = bytes * 8;
                        let loaded = self.bus.load(a, bits)?;
                        // A narrow compare-and-swap looks at the low bits of rd only,
                        // and stores the low bits of rs2.
                        let mask = u64::MAX >> (64 - bits);
                        if loaded == self.regs[rd] & mask {
                            self.bus.store(a, bits, b)?;
                        }
                        self.regs[rd] = Self::sext(loaded, bits);
                    }
                }
            }

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
