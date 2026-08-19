use std::time::Instant;

#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    bus::{Bus, DRAM_BASE},
    clint,
    csr::{self, *},
    dram::Dram,
    elf::{Error as ElfError, Image},
    fpu::{self, F32, F64, Format, Round},
    inst::{self, AmoOp, CasWidth, Cond, FpOp, Inst, Op, Width, decode},
    mmu::{Access, Tlb},
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
    /// The floating-point registers. A value narrower than the widest format is kept
    /// with every bit above it set, so a register holding a single can be told from
    /// one holding a double whose bits happen to look like one.
    /// The RISC-V Instruction Set Manual Volume I, 21.2.
    pub fregs: [u64; 32],
    /// The privilege the hart is running at. A trap raises it, an `xRET` lowers it.
    pub mode: Mode,
    /// What the last few page table walks found, so most of them do not happen.
    pub tlb: Tlb,
    pub start: Instant,
}

impl Cpu {
    pub fn new(code: Vec<u8>) -> Self {
        Self::with_memory(code, crate::dram::DRAM_SIZE)
    }

    /// A machine with `memory` bytes of dram, its stack pointer at the top of it.
    pub fn with_memory(code: Vec<u8>, memory: u64) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: DRAM_BASE,
            next_pc: DRAM_BASE,
            bus: Bus::new(Dram::with_size(code, memory)),
            csrs: [0; 4096],
            fregs: [0; 32],
            mode: Mode::Machine,
            tlb: Tlb::default(),
            start: Instant::now(),
        };

        cpu.regs[0] = 0;
        cpu.regs[2] = DRAM_BASE + memory;
        cpu.csrs[MISA] = MISA_MXL_64
            | misa_extension(b'i')
            | misa_extension(b'm')
            | misa_extension(b'a')
            | misa_extension(b'c')
            | misa_extension(b'f')
            | misa_extension(b'd')
            | misa_extension(b's')
            | misa_extension(b'u');
        // The floating-point registers are there and untouched, which is what a
        // supervisor needs to know to skip saving them.
        cpu.csrs[MSTATUS] = MSTATUS_XL_64 | (1 << MSTATUS_FS_SHIFT);

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
        // Each counter runs unless `mcountinhibit` says to hold it still.
        // The RISC-V Instruction Set Manual Volume II, 3.1.12.
        let inhibit = self.csrs[MCOUNTINHIBIT];
        if inhibit & 1 == 0 {
            self.csrs[MCYCLE] = self.csrs[MCYCLE].wrapping_add(1);
        }
        // An instruction that names the retired-instruction counter has said what it
        // should hold, so it does not also count itself.
        let wrote_instret = matches!(
            inst.op,
            Op::Csrrw { .. } | Op::Csrrs { .. } | Op::Csrrc { .. }
        ) && matches!(inst.imm as usize, MINSTRET | MINSTRETH);

        self.execute(inst, encoding)?;

        // Counted here rather than before executing, because this is where it retires:
        // one that trapped did not, and `ecall` and `ebreak` are specified as never
        // retiring at all. The RISC-V Instruction Set Manual Volume II, 3.3.1.
        if inhibit & 0b100 == 0 && !wrote_instret {
            self.csrs[MINSTRET] = self.csrs[MINSTRET].wrapping_add(1);
        }

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
        if !csr::exists(addr)
            || (write && addr >> 10 == 0b11)
            || (self.mode as u64) < least
            // satp is the one CSR whose address does not say everything: a machine
            // that has set TVM wants to be told before a supervisor changes the page
            // table. The RISC-V Instruction Set Manual Volume II, 3.1.6.5.
            || (addr == SATP && self.trapped(MSTATUS_TVM))
            // The floating-point status is part of the floating-point state, so it is
            // out of reach on a machine that has turned that off.
            || (matches!(addr, FFLAGS | FRM | FCSR) && !self.fp_usable())
        {
            return Err(Exception::IllegalInstruction(word as u64));
        }
        Ok(())
    }

    /// Whether one of `mstatus`'s trap-enable bits is holding this supervisor back.
    /// None of them applies in machine mode, which is where they are set from.
    fn trapped(&self, bit: u64) -> bool {
        self.mode < Mode::Machine && (self.csrs[MSTATUS] >> bit) & 1 == 1
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
            // The three unprivileged counters are read-only windows onto the machine's
            // own, not registers of their own.
            // The RISC-V Instruction Set Manual Volume II, 11.
            // The two halves of fcsr have numbers of their own, and are windows onto
            // it rather than registers beside it.
            FFLAGS => self.csrs[FCSR] & FFLAGS_MASK,
            FRM => (self.csrs[FCSR] >> FRM_SHIFT) & 0x7,
            CYCLE => self.csrs[MCYCLE],
            INSTRET => self.csrs[MINSTRET],
            MCYCLEH => self.csrs[MCYCLE] >> 32,
            MINSTRETH => self.csrs[MINSTRET] >> 32,
            // `time` is defined to be the same counter the timer compares against, and
            // on this machine that counter lives in the clint.
            TIME => self.bus.load(clint::BASE + clint::MTIME, 64).unwrap_or(0),
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
            FFLAGS => {
                self.csrs[FCSR] = (self.csrs[FCSR] & !FFLAGS_MASK) | (value & FFLAGS_MASK);
                self.dirty_fp();
            }
            FRM => {
                self.csrs[FCSR] =
                    (self.csrs[FCSR] & !(0x7 << FRM_SHIFT)) | ((value & 0x7) << FRM_SHIFT);
                self.dirty_fp();
            }
            // Nothing above the rounding mode is defined, so nothing above it is kept.
            FCSR => {
                self.csrs[FCSR] = value & 0xff;
                self.dirty_fp();
            }
            MCYCLEH => self.csrs[MCYCLE] = (self.csrs[MCYCLE] & 0xffff_ffff) | (value << 32),
            MINSTRETH => self.csrs[MINSTRET] = (self.csrs[MINSTRET] & 0xffff_ffff) | (value << 32),
            // satp's mode is WARL over the schemes the machine has, which is how
            // software finds out which those are: it writes one and reads it back.
            // rysk has bare and Sv39. The RISC-V Instruction Set Manual Volume II,
            // 12.1.11.
            SATP => {
                if matches!(value >> 60, 0 | 8) {
                    self.csrs[SATP] = value;
                    // A different table, or none, is a different set of answers.
                    self.tlb.flush();
                }
            }
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
        let pa = self.translate(self.pc, Access::Fetch)?;
        let half = self.halfword(pa)?;
        if inst::length(half) == 2 {
            return Ok((rvc::decode(half)?, half as u32));
        }
        // Both halves are in the same page unless the first one ends it, which is the
        // only place a translation could differ between them.
        let next = if self.pc & 0xfff == 0xffe {
            self.translate(self.pc + 2, Access::Fetch)?
        } else {
            pa + 2
        };
        let word = half as u32 | (self.halfword(next)? as u32) << 16;
        Ok((decode(word)?, word))
    }

    #[inline]
    fn halfword(&mut self, pa: u64) -> Result<u16, Exception> {
        self.bus
            .load(pa, 16)
            .map(|half| half as u16)
            .map_err(|_| Exception::InstructionAccessFault(self.pc))
    }

    /// Read `bits` at a virtual address.
    ///
    /// An access that is not aligned to its own width can straddle two pages, which
    /// are separate translations and may be separately absent, so that case is read a
    /// byte at a time. It is rare enough to be worth the branch and wrong enough to be
    /// worth handling. Only while translating: a device is entitled to see the access
    /// it was given rather than a row of byte-sized ones.
    fn read(&mut self, va: u64, bits: u64) -> Result<u64, Exception> {
        if Self::straddles(va, bits) && self.translating(Access::Load) {
            let mut value = 0;
            for byte in 0..bits / 8 {
                let pa = self.translate(va + byte, Access::Load)?;
                value |= self.bus.load(pa, 8)? << (byte * 8);
            }
            return Ok(value);
        }
        let pa = self.translate(va, Access::Load)?;
        self.bus.load(pa, bits)
    }

    /// Write `bits` at a virtual address, with the same care about page boundaries.
    fn write(&mut self, va: u64, bits: u64, value: u64) -> Result<(), Exception> {
        if Self::straddles(va, bits) && self.translating(Access::Store) {
            // Both halves are translated before either is written, so an access that
            // faults part way through has not half happened.
            for byte in 0..bits / 8 {
                self.translate(va + byte, Access::Store)?;
            }
            for byte in 0..bits / 8 {
                let pa = self.translate(va + byte, Access::Store)?;
                self.bus.store(pa, 8, value >> (byte * 8))?;
            }
            return Ok(());
        }
        let pa = self.translate(va, Access::Store)?;
        self.bus.store(pa, bits, value)
    }

    /// Whether an access of `bits` at `va` reaches into the page after it.
    #[inline]
    const fn straddles(va: u64, bits: u64) -> bool {
        (va & 0xfff) + bits / 8 > 0x1000
    }

    /// Carry out one decoded instruction.
    fn execute(&mut self, inst: Inst, encoding: u32) -> Result<(), Exception> {
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
                let value = self.read(a.wrapping_add(imm), width.bits())?;
                self.regs[rd] = if signed {
                    Self::sext(value, width.bits())
                } else {
                    value
                };
            }
            Op::Store { width } => self.write(a.wrapping_add(imm), width.bits(), b)?,

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
                self.check_csr(imm as usize, encoding, true)?;
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
                self.check_csr(imm as usize, encoding, rs1 != 0)?;
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
                if self.mode < mode || (mode == Mode::Supervisor && self.trapped(MSTATUS_TSR)) {
                    return Err(Exception::IllegalInstruction(encoding as u64));
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
            // A reservation can only be broken by a store, and the only thing on this
            // machine that stores is the hart that is waiting. Waiting for a store
            // that cannot arrive is waiting forever, so the stall ends at once, which
            // the manual permits for any reason. Devices that master the bus would
            // make this a real wait.
            // The RISC-V Instruction Set Manual Volume I, 14.1.
            // There is no address-translation cache to invalidate yet, so ordering
            // the walk against the stores that changed the table is all this has to
            // do, and one in-order hart does that by itself. It still has to be a
            // supervisor asking. The RISC-V Instruction Set Manual Volume II, 12.2.1.
            Op::SfenceVma => {
                if self.mode < Mode::Supervisor || self.trapped(MSTATUS_TVM) {
                    return Err(Exception::IllegalInstruction(encoding as u64));
                }
                self.tlb.flush();
            }

            Op::Wrs { .. } => {}

            Op::Wfi => {
                // With TW set, a wait below machine mode has to end within a bounded
                // time or trap, and rysk's bound is no time at all.
                if self.trapped(MSTATUS_TW) {
                    return Err(Exception::IllegalInstruction(encoding as u64));
                }
                self.refresh_mip();
                while self.csrs[MIP] & self.csrs[MIE] == 0 {
                    std::hint::spin_loop();
                    self.refresh_mip();
                }
            }
            // One in-order hart observes its own accesses in order, and there is no
            // instruction cache to keep coherent.
            Op::Fence | Op::FenceI => {}

            // ------------------------------------------------- floating point
            Op::FpLoad { .. } | Op::FpStore { .. } | Op::Fp { .. } | Op::FpFused { .. }
                if !self.fp_usable() =>
            {
                return Err(Exception::IllegalInstruction(encoding as u64));
            }

            Op::FpLoad { double } => {
                let format = if double { F64 } else { F32 };
                let value = self.read(a.wrapping_add(imm), format.bits as u64)?;
                self.write_fp(rd, format, value);
            }
            Op::FpStore { double } => {
                let format = if double { F64 } else { F32 };
                let value = self.read_fp_raw(rs2, format);
                self.write(a.wrapping_add(imm), format.bits as u64, value)?;
            }

            Op::FpFused {
                negate_product,
                negate_addend,
                double,
            } => {
                let format = if double { F64 } else { F32 };
                let mode = self.rounding(imm & 0x7, encoding)?;
                let rs3 = (imm >> 3) as usize;
                // Turning the product over is turning one of its factors over, and
                // the addend is its own term, so the four instructions are two bits.
                let flip = |value: u64, when: bool| value ^ (format.sign_bit() * when as u64);
                let x = flip(self.read_fp(rs1, format), negate_product);
                let z = flip(self.read_fp(rs3, format), negate_addend);
                let (value, flags) = fpu::fma(format, x, self.read_fp(rs2, format), z, mode);
                self.write_fp(rd, format, value);
                self.accrue(flags);
            }

            Op::Fp { op, double } => {
                let format = if double { F64 } else { F32 };
                let other = if double { F32 } else { F64 };
                let mode = self.rounding(imm, encoding)?;
                let x = self.read_fp(rs1, format);
                let y = self.read_fp(rs2, format);
                // The comparisons, the classify and the moves out land in an integer
                // register; everything else stays in the floating-point file.
                let ((value, flags), integer) = match op {
                    FpOp::Add => (fpu::add(format, x, y, mode), false),
                    FpOp::Sub => (fpu::sub(format, x, y, mode), false),
                    FpOp::Mul => (fpu::mul(format, x, y, mode), false),
                    FpOp::Div => (fpu::div(format, x, y, mode), false),
                    FpOp::Sqrt => (fpu::sqrt(format, x, mode), false),
                    FpOp::Min => (fpu::min_max(format, x, y, false), false),
                    FpOp::Max => (fpu::min_max(format, x, y, true), false),
                    FpOp::SignJoin { negate, xor } => {
                        ((fpu::sign_inject(format, x, y, negate, xor), 0), false)
                    }
                    FpOp::Equal => (fpu::eq(format, x, y), true),
                    FpOp::Less => (fpu::less(format, x, y, false), true),
                    FpOp::LessEqual => (fpu::less(format, x, y, true), true),
                    FpOp::Classify => ((fpu::classify(format, x), 0), true),
                    FpOp::Convert => (
                        fpu::convert(other, format, self.read_fp(rs1, other), mode),
                        false,
                    ),
                    FpOp::ToInteger { bits, signed } => {
                        (fpu::to_integer(format, x, bits, signed, mode), true)
                    }
                    FpOp::FromInteger { bits, signed } => (
                        fpu::from_integer(format, self.regs[rs1], bits, signed, mode),
                        false,
                    ),
                    // Moving the bits does not interpret them, so a single is
                    // sign-extended out of the register and boxed back into it.
                    FpOp::MoveOut => {
                        let bits = self.read_fp_raw(rs1, format);
                        ((Self::sext(bits, format.bits as u64), 0), true)
                    }
                    FpOp::MoveIn => ((self.regs[rs1], 0), false),
                };
                if integer {
                    self.regs[rd] = value;
                } else {
                    self.write_fp(rd, format, value);
                }
                self.accrue(flags);
            }

            // ---------------------------------------------------------- atomics
            // Compare-and-swap: load, compare against what `rd` holds, and store only
            // if they match. `rd` takes the value that was there either way, so a
            // caller learns whether it won without a second load.
            //
            // The RISC-V Instruction Set Manual Volume I, 15.1.
            Op::AmoCas { width } => {
                let bytes = match width {
                    CasWidth::Narrow(width) => width.bits() / 8,
                    CasWidth::Quad => 16,
                };
                if a & (bytes - 1) != 0 {
                    return Err(Exception::StoreAmoAddressMisaligned(a));
                }
                // A compare-and-swap writes, so it needs a page it may write, and a
                // quadword's second half is inside the page its first half is in.
                let a = self.translate(a, Access::Store)?;
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
                    CasWidth::Narrow(width) => {
                        let bits = width.bits();
                        let loaded = self.bus.load(a, bits)?;
                        // Anything narrower looks at the low bits of rd only, and
                        // stores the low bits of rs2: what is above the width is not
                        // part of either operand.
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
                // A load-reserved only reads; everything else here writes. The
                // reservation is on the physical address, which is what another hart
                // would be storing to.
                let access = match op {
                    Op::Lr { .. } => Access::Load,
                    _ => Access::Store,
                };
                let a = self.translate(a, access)?;
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

    /// Read a floating-point register at `f`'s width. A single that is not held with
    /// every bit above it set is not a single at all, and reads as the one NaN this
    /// machine makes. The RISC-V Instruction Set Manual Volume I, 21.2.
    fn read_fp(&self, reg: usize, format: Format) -> u64 {
        let bits = self.fregs[reg];
        match format {
            F64 => bits,
            _ if bits >> 32 == u32::MAX as u64 => bits & 0xffff_ffff,
            _ => F32.canonical_nan(),
        }
    }

    /// The bits of a floating-point register, without asking whether they are a
    /// number. A store and a move are not interpreting the value, so a register
    /// holding a double is not turned into a NaN on its way past them.
    fn read_fp_raw(&self, reg: usize, format: Format) -> u64 {
        format.trim(self.fregs[reg])
    }

    /// Write one, boxing a narrow value and recording that the registers now hold
    /// something worth saving.
    fn write_fp(&mut self, reg: usize, format: Format, value: u64) {
        self.fregs[reg] = match format {
            F64 => value,
            _ => value | 0xffff_ffff_0000_0000,
        };
        self.dirty_fp();
    }

    fn dirty_fp(&mut self) {
        self.csrs[MSTATUS] |= MSTATUS_FS_DIRTY | MSTATUS_SD;
    }

    /// What an exception the arithmetic raised accumulates into.
    fn accrue(&mut self, flags: u64) {
        if flags != 0 {
            self.csrs[FCSR] |= flags;
            self.dirty_fp();
        }
    }

    /// The rounding an instruction asked for. Seven means whatever `frm` says, and the
    /// three encodings that name nothing make the instruction illegal rather than
    /// rounding it some other way.
    fn rounding(&self, asked: u64, encoding: u32) -> Result<Round, Exception> {
        let bits = match asked {
            7 => (self.csrs[FCSR] >> FRM_SHIFT) & 0x7,
            other => other,
        };
        Round::from_bits(bits).ok_or(Exception::IllegalInstruction(encoding as u64))
    }

    /// Whether the floating-point registers are there at all. Turning them off is how
    /// a supervisor says it is not going to save them.
    fn fp_usable(&self) -> bool {
        self.csrs[MSTATUS] & MSTATUS_FS != 0
    }

    /// The read-modify-write an atomic performs, at the width it performs it.
    ///
    /// Everything narrower than a doubleword has to be done at its own width and not
    /// at sixty-four bits: the sum wraps there, the signed comparisons read the sign
    /// there, and the bits of the source above it are not part of the operand at all.
    ///
    /// The RISC-V Instruction Set Manual Volume I, 13.4 and 16.1.
    fn amo(op: AmoOp, width: Width, data: u64, src: u64) -> u64 {
        let bits = width.bits();
        let mask = u64::MAX >> (64 - bits);
        let (d, s) = (data & mask, src & mask);
        let (signed_d, signed_s) = (Self::sext(d, bits) as i64, Self::sext(s, bits) as i64);
        mask & match op {
            AmoOp::Swap => s,
            AmoOp::Add => d.wrapping_add(s),
            AmoOp::Xor => d ^ s,
            AmoOp::And => d & s,
            AmoOp::Or => d | s,
            AmoOp::Min => signed_d.min(signed_s) as u64,
            AmoOp::Max => signed_d.max(signed_s) as u64,
            AmoOp::MinU => d.min(s),
            AmoOp::MaxU => d.max(s),
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
