#[cfg(feature = "trace")]
use tracing::instrument;

use crate::{
    block::{Block, Blocks, Decoded, LENGTH, ends},
    bus::{Bus, DRAM_BASE},
    clint,
    csr::{self, *},
    device::Level,
    fpu::{self, F32, F64, Format, Round},
    imsic::{self, Imsic},
    inst::{self, AmoOp, CasWidth, Cond, FpOp, Inst, Op, Width, decode},
    mmu::{Access, PAGE_BITS, PAGE_SIZE, Tlb},
    rvc,
    trap::{Exception, Interrupt, Trap},
};

/// A `fetch_page` naming no page, so that a fetch translates rather than believing it.
/// The top page number is one no fetch reaches: a virtual address has to repeat its
/// sign above bit 38 to translate at all, and an untranslated one is a physical address
/// of thirty-four page-number bits.
const NO_PAGE: (u64, u64) = (u64::MAX, 0);

#[derive(Debug)]
pub struct Cpu {
    pub regs: [u64; 32],
    /// The instruction being executed.
    pub pc: u64,
    /// Where control goes when it retires. Jumps and taken branches overwrite it.
    pub next_pc: u64,
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
    /// What the last few runs of straight-line code decoded to, so most fetches do
    /// not happen either.
    pub blocks: Blocks,
    /// Which hart this is. It is the index the machine holds it at, the reservation on
    /// the bus that is its own, the interrupt bits the controllers drive for it, and
    /// what `mhartid` reads.
    pub hart: usize,
    /// The interrupt files this hart receives messages in, if the machine gave it any.
    /// A hart with one has Smaia and Ssaia and the CSRs that reach the files; a hart
    /// without one is interrupted by wires alone and those CSRs do not exist for it.
    pub imsic: Option<Imsic>,
    /// Whether a `wfi` has parked it. A parked hart executes nothing until an
    /// interrupt it has enabled is pending; noticing that is the scheduler's job,
    /// because on a machine with more than one hart the interrupt that ends the wait
    /// is usually one another hart has to send.
    pub waiting: bool,
    /// The page `pc` was last fetched from: its page number, and the physical address
    /// that page starts at. Straight-line code stays inside one, and while it does a
    /// fetch is a compare rather than a walk of the translation cache and the
    /// permission rules again for an answer that has not moved.
    ///
    /// It is forgotten wherever that answer could change: a different address space, a
    /// fence saying the tables moved, or a different privilege mode, since a machine
    /// mode that translates nothing and a supervisor that does are not the same
    /// question. `MPRV`, `SUM` and `MXR` do not come into it, because none of the three
    /// says anything about a fetch.
    fetch_page: (u64, u64),
    /// What the controllers and `mvip` were last seen driving into `mip`. The
    /// controllers publish rather than being asked, so the usual answer before an
    /// instruction is the same one as before the last, and writing it into `mip` again
    /// changes nothing.
    driven: u64,
}

impl Cpu {
    /// Hart `hart` of a machine, out of reset: the register file zeroed, machine mode,
    /// and `pc` at the bottom of dram, which is where this machine starts.
    pub fn new(hart: usize) -> Self {
        let mut cpu = Cpu {
            regs: Default::default(),
            pc: DRAM_BASE,
            next_pc: DRAM_BASE,
            csrs: [0; 4096],
            fregs: [0; 32],
            mode: Mode::Machine,
            tlb: Tlb::default(),
            blocks: Blocks::default(),
            hart,
            imsic: None,
            waiting: false,
            fetch_page: NO_PAGE,
            driven: 0,
        };

        // Read-only, and the only piece of machine information that says anything. No
        // two harts may report the same one.
        // The RISC-V Instruction Set Manual Volume II, 3.1.5.
        cpu.csrs[MHARTID] = hart as u64;
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

    /// Let a parked hart go if anything it has enabled is pending.
    ///
    /// `wfi` waits on `mip & mie` alone, whatever `mstatus` says about actually taking
    /// the interrupt: the hart resumes, and whether it enters a handler is then the
    /// ordinary question asked before the next instruction.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.3.3.
    pub fn wake(&mut self, bus: &Bus) {
        self.refresh_mip(bus);
        if self.csrs[MIP] & self.csrs[MIE] != 0 {
            self.waiting = false;
        }
    }

    /// Refresh the bits of `mip` that a device drives. They are not storage software
    /// writes: each one is asserted for exactly as long as its device asserts it.
    ///
    /// Kept out of line so that asking whether there is an interrupt stays a pair of
    /// register reads at the call site, whether or not any device drives one.
    #[inline(never)]
    fn refresh_mip(&mut self, bus: &Bus) {
        self.drive(self.driving(bus));
    }

    /// What the machine is driving into this hart's `mip`.
    ///
    /// `SEIP` is the one pending bit with two sources: the controller's wire, and a bit
    /// machine mode may set by hand to post a supervisor external interrupt no device
    /// raised. What `mip` reports is the two together, and the bit software owns is the
    /// one `mvip` exposes on its own.
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9.
    #[inline]
    fn driving(&self, bus: &Bus) -> u64 {
        bus.interrupts(self.hart) | (self.csrs[MVIP] & SEIP)
    }

    /// Put `driven` where `mip` reports it. Nothing else writes those bits: a software
    /// write to `mip` keeps them, which is what lets the last value be remembered.
    #[inline]
    fn drive(&mut self, driven: u64) {
        self.driven = driven;
        self.csrs[MIP] = (self.csrs[MIP] & !MIP_DEVICE) | driven;
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
    /// Asked once per instruction and answered no almost every time, so the question
    /// itself is two register reads and a branch, and everything past the point where
    /// the answer might be yes is out of line.
    ///
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9 and 12.1.3.
    #[inline]
    pub fn interrupt(&mut self, bus: &Bus) -> Option<Interrupt> {
        // Asking the devices costs a read of the host clock, and it cannot change the
        // answer unless one of the bits they drive is enabled, so when none is the
        // question is answered out of `mip` alone. Software still sees the true value:
        // reading the register is what refreshes it.
        if self.csrs[MIE] & MIP_DEVICE != 0 {
            // Almost always the same answer as last time, since it changes only when a
            // controller publishes, and writing it into `mip` again would be a store
            // per instruction to say nothing.
            let driven = self.driving(bus);
            if driven != self.driven {
                self.drive(driven);
            }
        }
        let ready = self.csrs[MIP] & self.csrs[MIE];
        if ready == 0 {
            return None;
        }
        self.highest_priority(ready)
    }

    /// The one to take out of those that are pending and enabled.
    #[inline(never)]
    fn highest_priority(&self, ready: u64) -> Option<Interrupt> {
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
    pub fn step(&mut self, bus: &mut Bus) -> Result<(), Exception> {
        let block = self.block(bus)?;
        self.retire(bus, self.blocks.at(block.start))
    }

    /// Execute one instruction of a run that has already been decoded.
    #[inline]
    pub fn retire(&mut self, bus: &mut Bus, decoded: Decoded) -> Result<(), Exception> {
        let Decoded {
            inst,
            encoding,
            length,
            writes_instret,
        } = decoded;
        trace_insn!("{:#x}  {inst}", self.pc);

        self.next_pc = self.pc.wrapping_add(length as u64);
        // Each counter runs unless `mcountinhibit` says to hold it still.
        // The RISC-V Instruction Set Manual Volume II, 3.1.12.
        let inhibit = self.csrs[MCOUNTINHIBIT];
        if inhibit & 1 == 0 {
            self.csrs[MCYCLE] = self.csrs[MCYCLE].wrapping_add(1);
        }
        self.execute(bus, inst, encoding)?;

        // Counted here rather than before executing, because this is where it retires:
        // one that trapped did not, and `ecall` and `ebreak` are specified as never
        // retiring at all. The RISC-V Instruction Set Manual Volume II, 3.3.1.
        // An instruction that names the retired-instruction counter has said what it
        // should hold, so it does not also count itself.
        if inhibit & 0b100 == 0 && !writes_instret {
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
            self.fetch_page = NO_PAGE;
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
        self.fetch_page = NO_PAGE;

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
        self.fetch_page = NO_PAGE;
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
        if !self.has_csr(addr)
            || (write && addr >> 10 == 0b11)
            // The window registers are the one pair whose legality is not a property
            // of their own address: what may be reached through them is whatever the
            // partner select register currently names.
            // The RISC-V Advanced Interrupt Architecture, 3.8.3.
            || (matches!(addr, MIREG | SIREG)
                && indirect(self.csrs[selects(addr)]) == Indirect::Reserved)
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

    /// Whether this hart has the CSR `addr` names. Everything Smaia and Ssaia add
    /// exists only on a hart the machine gave an IMSIC to, and everything else is the
    /// same on every hart rysk builds.
    fn has_csr(&self, addr: usize) -> bool {
        csr::exists(addr) || (self.imsic.is_some() && csr::aia(addr))
    }

    /// The interrupt file `addr` reaches, for the four registers that come in a
    /// machine-level and a supervisor-level spelling of the same thing.
    fn file(&self, addr: usize) -> Option<(&Imsic, Level)> {
        let level = match addr {
            MIREG | MTOPEI => Level::Machine,
            SIREG | STOPEI => Level::Supervisor,
            _ => return None,
        };
        Some((self.imsic.as_ref()?, level))
    }

    /// The highest-priority interrupt out of `ready`, in the format `mtopi` and
    /// `stopi` report: its identity, and the one priority this machine has.
    /// The RISC-V Advanced Interrupt Architecture, 5.2.2.
    fn topi(&self, ready: u64) -> u64 {
        match Interrupt::PRIORITY
            .into_iter()
            .find(|interrupt| ready >> (*interrupt as u64) & 1 == 1)
        {
            Some(interrupt) => ((interrupt as u64) << TOPI_IDENTITY) | TOPI_PRIORITY,
            None => 0,
        }
    }

    /// Whether one of `mstatus`'s trap-enable bits is holding this supervisor back.
    /// None of them applies in machine mode, which is where they are set from.
    fn trapped(&self, bit: u64) -> bool {
        self.mode < Mode::Machine && (self.csrs[MSTATUS] >> bit) & 1 == 1
    }

    #[cfg_attr(feature = "trace", instrument(skip(self, bus)))]
    fn load_csr(&mut self, bus: &mut Bus, addr: usize) -> u64 {
        trace_insn!("loading csr");
        // The device-driven bits of mip are wires, so reading them is reading the
        // devices rather than anything software last wrote.
        if addr == MIP || addr == SIP {
            self.refresh_mip(bus);
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
            TIME => bus.load(clint::BASE + clint::MTIME, 64).unwrap_or(0),
            // The window onto an interrupt file, and onto the priority array that is
            // read-only zero here.
            MIREG | SIREG => {
                let select = self.csrs[selects(addr)];
                match self.file(addr) {
                    Some((imsic, level)) if indirect(select) == Indirect::File => {
                        imsic.read(self.hart, level, select)
                    }
                    _ => 0,
                }
            }
            MTOPEI | STOPEI => match self.file(addr) {
                Some((imsic, level)) => imsic.topei(self.hart, level),
                None => 0,
            },
            // The top interrupt of either level, out of what is pending and enabled
            // and belongs to that level. Neither is affected by the global enable of
            // the mode it reports for.
            MTOPI | STOPI => {
                self.refresh_mip(bus);
                let ready = self.csrs[MIP] & self.csrs[MIE];
                let mine = match addr {
                    MTOPI => ready & !self.csrs[MIDELEG],
                    _ => ready & self.csrs[MIDELEG],
                };
                self.topi(mine)
            }
            // No interrupt may be made virtual on this machine, so every bit of it is
            // read-only zero and `sip` and `sie` keep the shape delegation gives them.
            // The RISC-V Advanced Interrupt Architecture, 5.3.
            MVIEN => 0,
            MVIP => (self.csrs[MIP] & MVIP_ALIAS) | (self.csrs[MVIP] & SEIP),
            _ => self.csrs[addr],
        }
    }

    /// What a read-modify-write of `addr` modifies, which is not always what a read of
    /// it returns.
    ///
    /// `SEIP` is the one place the two differ: a read gives the interrupt controller's
    /// signal and the bit machine mode owns together, and only the bit machine mode
    /// owns takes part in the sequence. A `csrrs` that read the signal back and wrote
    /// it would leave that bit set for good, and the interrupt would never end.
    /// The RISC-V Instruction Set Manual Volume II, 3.1.9.
    fn modified(&self, addr: usize, read: u64) -> u64 {
        match addr {
            MIP => (read & !SEIP) | (self.csrs[MVIP] & SEIP),
            _ => read,
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
                    self.fetch_page = NO_PAGE;
                }
            }
            // The bits a device drives are read-only here: a write cannot argue with a
            // wire. `SEIP` is the exception, since half of what it reports is a bit
            // machine mode owns, and writing `mip` is one of the two ways to reach it.
            // The RISC-V Instruction Set Manual Volume II, 3.1.9.
            MIP => {
                self.csrs[MVIP] = (self.csrs[MVIP] & !SEIP) | (value & SEIP);
                self.csrs[MIP] = (self.csrs[MIP] & MIP_DEVICE) | (value & !MIP_DEVICE);
            }
            // WARL over the eight bits every value it has to hold fits in. Nothing
            // above them is a register space this machine implements.
            // The RISC-V Advanced Interrupt Architecture, 2.1.
            MISELECT | SISELECT => self.csrs[addr] = value & 0xff,
            MIREG | SIREG => {
                let select = self.csrs[selects(addr)];
                if let Some((imsic, level)) = self.file(addr)
                    && indirect(select) == Indirect::File
                {
                    imsic.write(self.hart, level, select, value);
                }
            }
            // A write claims whatever the register reads as, not what was written.
            // The RISC-V Advanced Interrupt Architecture, 3.9.
            MTOPEI | STOPEI => {
                if let Some((imsic, level)) = self.file(addr) {
                    imsic.claim(self.hart, level);
                }
            }
            MVIEN => {}
            // Two of its bits are a window onto `mip` and the third is the only
            // storage behind `SEIP` there.
            MVIP => {
                self.csrs[MIP] = (self.csrs[MIP] & !MVIP_ALIAS) | (value & MVIP_ALIAS);
                self.csrs[MVIP] = value & SEIP;
            }
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

    /// The run of instructions starting at `pc`: the one already decoded there, or a
    /// new one decoded and kept now.
    ///
    /// Only the first instruction of a run is translated. The rest are in the page it
    /// is in, which is what a run ending at the page edge is for, so they share its
    /// permissions and its frame.
    #[inline]
    pub fn block(&mut self, bus: &mut Bus) -> Result<Block, Exception> {
        // Where the last instruction came from answers for the whole page it was in,
        // and most instructions are in the page the one before them was.
        let vpn = self.pc >> PAGE_BITS;
        let pa = if vpn == self.fetch_page.0 {
            self.fetch_page.1 | (self.pc & (PAGE_SIZE - 1))
        } else {
            let pa = self.translate(bus, self.pc, Access::Fetch)?;
            self.fetch_page = (vpn, pa & !(PAGE_SIZE - 1));
            pa
        };
        match self.blocks.get(pa) {
            Some(block) => Ok(block),
            None => self.build(bus, pa),
        }
    }

    /// Decode from `pa` until the run ends, and keep what was decoded.
    ///
    /// Out of line: it runs once for as many instructions as it decodes, and letting it
    /// into the caller would spread its registers through a path that mostly hits.
    #[inline(never)]
    fn build(&mut self, bus: &mut Bus, pa: u64) -> Result<Block, Exception> {
        let mut run = [Decoded::NONE; LENGTH];
        let mut decoded = 0;
        let (mut at, mut va) = (pa, self.pc);
        while decoded < LENGTH {
            let next = match self.decode_at(bus, at, va) {
                Ok(next) => next,
                // Only the first instruction is one the hart has asked for. Everything
                // past it is read ahead of being reached, so an encoding this machine
                // refuses ends the run instead of raising: the hart may never arrive
                // there, and if it does, the run starting there raises it then.
                Err(exception) if decoded == 0 => return Err(exception),
                Err(_) => break,
            };
            run[decoded] = next;
            decoded += 1;
            at = at.wrapping_add(next.length as u64);
            va = va.wrapping_add(next.length as u64);
            // A run holds one page's instructions, so that the translation its first
            // one paid for answers for all of them. An instruction whose halves are in
            // two pages is left as the last of the run that reaches it: it is the one
            // place the second half translates on its own.
            if ends(next.inst.op) || (at ^ pa) & !(PAGE_SIZE - 1) != 0 {
                break;
            }
        }
        Ok(self.blocks.insert(pa, &run[..decoded]))
    }

    /// Read and decode the instruction at `pa`, whose virtual address is `va`.
    ///
    /// It arrives a halfword at a time because its length is in its first two bytes: a
    /// compressed instruction can sit in the last two bytes of memory, and reading four
    /// there would fault on bytes it does not have.
    fn decode_at(&mut self, bus: &mut Bus, pa: u64, va: u64) -> Result<Decoded, Exception> {
        // The whole word at once, where both halves are certain to be in the same page
        // and in dram. A page is the granularity of translation and of dram alike, so
        // reading the wider one there cannot fault where the two narrower ones would
        // not. A device may answer for one width and refuse another, so an address
        // that is not dram is read a half at a time, and so is the last halfword of a
        // page, which is the only place the two halves translate differently.
        let word = if pa & 0xfff <= 0xffc && bus.in_dram(pa, 32) {
            self.word(bus, pa, va)?
        } else {
            self.halves(bus, pa, va)?
        };
        // A compressed instruction is the low half alone, and the high half of what was
        // read is not part of it.
        let length = inst::length(word as u16);
        let inst = if length == 2 {
            rvc::decode(word as u16)?
        } else {
            decode(word)?
        };
        Ok(Decoded {
            inst,
            encoding: if length == 2 { word & 0xffff } else { word },
            length: length as u8,
            writes_instret: matches!(
                inst.op,
                Op::Csrrw { .. } | Op::Csrrs { .. } | Op::Csrrc { .. }
            ) && matches!(inst.imm as usize, MINSTRET | MINSTRETH),
        })
    }

    /// The instruction at `pa`, read a halfword at a time, with the second half
    /// translated on its own where the first one ends a page. A compressed instruction
    /// is answered by its own half and the one after it is never read.
    fn halves(&mut self, bus: &mut Bus, pa: u64, va: u64) -> Result<u32, Exception> {
        let half = self.halfword(bus, pa, va)?;
        if inst::length(half) == 2 {
            return Ok(half as u32);
        }
        let next = if va & 0xfff == 0xffe {
            self.translate(bus, va + 2, Access::Fetch)?
        } else {
            pa + 2
        };
        Ok(half as u32 | (self.halfword(bus, next, va)? as u32) << 16)
    }

    #[inline]
    fn halfword(&mut self, bus: &mut Bus, pa: u64, va: u64) -> Result<u16, Exception> {
        bus.load(pa, 16)
            .map(|half| half as u16)
            .map_err(|_| Exception::InstructionAccessFault(va))
    }

    #[inline]
    fn word(&mut self, bus: &mut Bus, pa: u64, va: u64) -> Result<u32, Exception> {
        bus.load(pa, 32)
            .map(|word| word as u32)
            .map_err(|_| Exception::InstructionAccessFault(va))
    }

    /// Read `bits` at a virtual address.
    ///
    /// An access that is not aligned to its own width can straddle two pages, which
    /// are separate translations and may be separately absent, so that case is read a
    /// byte at a time. It is rare enough to be worth the branch and wrong enough to be
    /// worth handling. Only while translating: a device is entitled to see the access
    /// it was given rather than a row of byte-sized ones.
    /// The width is a constant rather than an argument because it decides what every
    /// step below it does: whether the access can straddle a page at all, how wide the
    /// load is, and how far to sign-extend it. As an argument each of those was a jump
    /// table, three of them per access.
    ///
    /// Out of line because there are four of it: letting all four into the run loop
    /// costs more in what it does to the scheduling there than the folded width saves.
    #[inline(never)]
    fn read<const BITS: u64>(&mut self, bus: &mut Bus, va: u64) -> Result<u64, Exception> {
        if Self::straddles(va, BITS) && self.translating(Access::Load) {
            let mut value = 0;
            for byte in 0..BITS / 8 {
                let pa = self.translate(bus, va + byte, Access::Load)?;
                value |= bus.load(pa, 8)? << (byte * 8);
            }
            return Ok(value);
        }
        let pa = self.translate(bus, va, Access::Load)?;
        bus.load(pa, BITS)
    }

    /// Write `BITS` at a virtual address, with the same care about page boundaries and
    /// for the same reasons out of line.
    #[inline(never)]
    fn write<const BITS: u64>(
        &mut self,
        bus: &mut Bus,
        va: u64,
        value: u64,
    ) -> Result<(), Exception> {
        if Self::straddles(va, BITS) && self.translating(Access::Store) {
            // Both halves are translated before either is written, so an access that
            // faults part way through has not half happened.
            for byte in 0..BITS / 8 {
                self.translate(bus, va + byte, Access::Store)?;
            }
            for byte in 0..BITS / 8 {
                let pa = self.translate(bus, va + byte, Access::Store)?;
                bus.store(pa, 8, value >> (byte * 8))?;
            }
            return Ok(());
        }
        let pa = self.translate(bus, va, Access::Store)?;
        bus.store(pa, BITS, value)
    }

    /// A load of `width` bits, widened the way the instruction asked for.
    #[inline]
    fn load_width(
        &mut self,
        bus: &mut Bus,
        va: u64,
        width: Width,
        signed: bool,
    ) -> Result<u64, Exception> {
        let widen = |value, bits| {
            if signed {
                Self::sext(value, bits)
            } else {
                value
            }
        };
        Ok(match width {
            Width::Byte => widen(self.read::<8>(bus, va)?, 8),
            Width::Half => widen(self.read::<16>(bus, va)?, 16),
            Width::Word => widen(self.read::<32>(bus, va)?, 32),
            Width::Double => widen(self.read::<64>(bus, va)?, 64),
        })
    }

    /// A store of `width` bits.
    #[inline]
    fn store_width(
        &mut self,
        bus: &mut Bus,
        va: u64,
        width: Width,
        value: u64,
    ) -> Result<(), Exception> {
        match width {
            Width::Byte => self.write::<8>(bus, va, value),
            Width::Half => self.write::<16>(bus, va, value),
            Width::Word => self.write::<32>(bus, va, value),
            Width::Double => self.write::<64>(bus, va, value),
        }
    }

    /// Whether an access of `bits` at `va` reaches into the page after it.
    #[inline]
    const fn straddles(va: u64, bits: u64) -> bool {
        (va & 0xfff) + bits / 8 > 0x1000
    }

    /// Carry out one decoded instruction.
    fn execute(&mut self, bus: &mut Bus, inst: Inst, encoding: u32) -> Result<(), Exception> {
        let Inst {
            op,
            rd,
            rs1,
            rs2,
            imm,
        } = inst;
        // Widened once here, so naming a register below is an index rather than a
        // conversion.
        let (rd, rs1, rs2) = (rd as usize, rs1 as usize, rs2 as usize);
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
                self.regs[rd] = self.load_width(bus, a.wrapping_add(imm), width, signed)?;
            }
            Op::Store { width } => self.store_width(bus, a.wrapping_add(imm), width, b)?,

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
                    self.regs[rd] = self.load_csr(bus, imm as usize);
                }
                self.store_csr(imm as usize, source);
            }
            Op::Csrrs { immediate } | Op::Csrrc { immediate } => {
                let source = if immediate { rs1 as u64 } else { a };
                // Naming no bits to change is not a write, so a read-only csr is still
                // readable this way.
                self.check_csr(imm as usize, encoding, rs1 != 0)?;
                let csr = self.load_csr(bus, imm as usize);
                // A source of x0, or of zero for the immediate forms, names no bits to
                // change, and then the csr is not written at all.
                if rs1 != 0 {
                    let held = self.modified(imm as usize, csr);
                    let value = match op {
                        Op::Csrrs { .. } => held | source,
                        _ => held & !source,
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
            // Ordering the walk against the stores that changed the table is what
            // this owes, and a hart that executes its own accesses one at a time does
            // that by itself. What is left is the cache of walks, which is emptied
            // whole. It still has to be a supervisor asking, and a machine that set
            // `TVM` wants to hear about it first.
            //
            // An `sfence.vma` speaks for the hart that executes it and for no other.
            // Software that has changed a table another hart is using sends that hart
            // an interrupt and has it execute its own, which is what the supervisor
            // binary interface calls a remote fence.
            //
            // The RISC-V Instruction Set Manual Volume II, 12.2.1.
            Op::SfenceVma => {
                if self.mode < Mode::Supervisor || self.trapped(MSTATUS_TVM) {
                    return Err(Exception::IllegalInstruction(encoding as u64));
                }
                self.tlb.flush();
                self.fetch_page = NO_PAGE;
            }

            // A reservation is broken by a store, so the wait is for another hart, and
            // the manual permits ending it for any reason at any time. Ending it at
            // once is a correct implementation of both `wrs.nto` and `wrs.sto`: the
            // loop around it is what actually waits.
            // The RISC-V Instruction Set Manual Volume I, 14.1.
            Op::Wrs { .. } => {}

            // Park until something is pending and enabled, whatever `mstatus` says
            // about taking it: the hart waits, it does not enter a handler. `wfi`
            // retires either way, so the trap is taken on the instruction after it and
            // returning from the handler resumes past the wait.
            //
            // The waiting happens outside the instruction because the interrupt that
            // ends it usually comes from somewhere this hart cannot reach while it is
            // executing: another hart's software interrupt, or a device that advances
            // with the wall clock. A hart waiting on an interrupt nothing can deliver
            // waits forever, which is what the hardware does too.
            //
            // The RISC-V Instruction Set Manual Volume II, 3.3.3.
            Op::Wfi => {
                // With TW set, a wait below machine mode has to end within a bounded
                // time or trap, and rysk's bound is no time at all.
                if self.trapped(MSTATUS_TW) {
                    return Err(Exception::IllegalInstruction(encoding as u64));
                }
                self.waiting = true;
            }

            // A hart executes its own accesses in order and every other hart sees them
            // in that order, since the harts share one memory and take turns at whole
            // instructions, so a fence has nothing left to order.
            //
            // `fence.i` orders the stores this hart has made against its own
            // instruction fetch, which is exactly the question the decoded
            // instructions answer, and it speaks for this hart alone: a store meant to
            // be fetched by another needs that hart to execute its own.
            // The RISC-V Instruction Set Manual Volume I, 5.
            Op::Fence => {}
            Op::FenceI => self.blocks.flush(),

            // ------------------------------------------------- floating point
            Op::FpLoad { .. } | Op::FpStore { .. } | Op::Fp { .. } | Op::FpFused { .. }
                if !self.fp_usable() =>
            {
                return Err(Exception::IllegalInstruction(encoding as u64));
            }

            Op::FpLoad { double } => {
                let format = if double { F64 } else { F32 };
                let va = a.wrapping_add(imm);
                let value = if double {
                    self.read::<64>(bus, va)?
                } else {
                    self.read::<32>(bus, va)?
                };
                self.write_fp(rd, format, value);
            }
            Op::FpStore { double } => {
                let format = if double { F64 } else { F32 };
                let value = self.read_fp_raw(rs2, format);
                let va = a.wrapping_add(imm);
                if double {
                    self.write::<64>(bus, va, value)?;
                } else {
                    self.write::<32>(bus, va, value)?;
                }
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
                    FpOp::ToInteger { width, signed } => {
                        (fpu::to_integer(format, x, width.bits(), signed, mode), true)
                    }
                    FpOp::FromInteger { width, signed } => (
                        fpu::from_integer(format, self.regs[rs1], width.bits(), signed, mode),
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
                let a = self.translate(bus, a, Access::Store)?;
                match width {
                    // A pair beginning at x0 reads as zero at both halves, and one
                    // named as the destination discards the result entirely rather
                    // than writing only its odd register.
                    CasWidth::Quad => {
                        let (low, high) = (bus.load(a, 64)?, bus.load(a + 8, 64)?);
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
                            bus.store(a, 64, swap.0)?;
                            bus.store(a + 8, 64, swap.1)?;
                        }
                        if rd != 0 {
                            self.regs[rd] = low;
                            self.regs[rd + 1] = high;
                        }
                    }
                    CasWidth::Narrow(width) => {
                        let bits = width.bits();
                        let loaded = bus.load(a, bits)?;
                        // Anything narrower looks at the low bits of rd only, and
                        // stores the low bits of rs2: what is above the width is not
                        // part of either operand.
                        let mask = u64::MAX >> (64 - bits);
                        if loaded == self.regs[rd] & mask {
                            bus.store(a, bits, b)?;
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
                let a = self.translate(bus, a, access)?;
                match op {
                    Op::Lr { .. } => {
                        self.regs[rd] = Self::sext(bus.load(a, bits)?, bits);
                        bus.reserve(self.hart, a, bits);
                    }
                    Op::Sc { .. } => {
                        // The store happens only if the reservation still covers these
                        // bytes, and rd reports zero for success, nonzero for failure.
                        self.regs[rd] = if bus.take_reservation(self.hart, a, bits) {
                            bus.store(a, bits, b)?;
                            0
                        } else {
                            1
                        };
                    }
                    Op::Amo { op, .. } => {
                        let data = bus.load(a, bits)?;
                        let value = Self::amo(op, width, data, b);
                        bus.store(a, bits, value)?;
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

/// What a value of `miselect` or `siselect` names. It is what decides both whether an
/// access to the matching `mireg` or `sireg` is legal at all and what it reaches, which
/// is why the two window registers cannot be checked by their own address alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Indirect {
    /// A register of the interrupt file at that level, including the numbers inside
    /// its range that name nothing and so read as zero.
    File,
    /// The array of major-interrupt priorities, every byte of which is read-only zero.
    Priorities,
    /// A number this machine has no register space for, which is an illegal
    /// instruction rather than a register reading as zero.
    Reserved,
}

/// Which select register a window register is paired with.
fn selects(window: usize) -> usize {
    match window {
        MIREG => MISELECT,
        _ => SISELECT,
    }
}

/// The RISC-V Advanced Interrupt Architecture, 2.1, 3.7 and 3.8.3.
fn indirect(select: u64) -> Indirect {
    match select {
        _ if IPRIO.contains(&select) => Indirect::Priorities,
        // With `XLEN` of sixty-four each register of the pending and enable arrays
        // holds twice as many identities, so the odd-numbered halves do not exist and
        // naming one is refused rather than read as zero.
        _ if (imsic::EIP0..imsic::SELECT.end).contains(&select) && !select.is_multiple_of(2) => {
            Indirect::Reserved
        }
        _ if imsic::SELECT.contains(&select) => Indirect::File,
        _ => Indirect::Reserved,
    }
}
