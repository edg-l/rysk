//! Why control left the program: the synchronous exceptions and the asynchronous
//! interrupts, and the values each leaves in `mcause` and `mtval`.

use crate::csr::Mode;

/// A synchronous exception, carrying the value the trap handler is owed in `mtval`:
/// the faulting address for a fault, the instruction encoding for an illegal
/// instruction, and nothing for an environment call.
///
/// The RISC-V Instruction Set Manual Volume II, table 15, gives the cause numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    InstructionAddressMisaligned(u64),
    InstructionAccessFault(u64),
    IllegalInstruction(u64),
    Breakpoint(u64),
    LoadAddressMisaligned(u64),
    LoadAccessFault(u64),
    StoreAmoAddressMisaligned(u64),
    StoreAmoAccessFault(u64),
    EnvironmentCall(Mode),
    InstructionPageFault(u64),
    LoadPageFault(u64),
    StoreAmoPageFault(u64),
}

impl Exception {
    /// The value written to `mcause`.
    pub fn cause(&self) -> u64 {
        match self {
            Self::InstructionAddressMisaligned(_) => 0,
            Self::InstructionAccessFault(_) => 1,
            Self::IllegalInstruction(_) => 2,
            Self::Breakpoint(_) => 3,
            Self::LoadAddressMisaligned(_) => 4,
            Self::LoadAccessFault(_) => 5,
            Self::StoreAmoAddressMisaligned(_) => 6,
            Self::StoreAmoAccessFault(_) => 7,
            Self::EnvironmentCall(Mode::User) => 8,
            Self::EnvironmentCall(Mode::Supervisor) => 9,
            Self::EnvironmentCall(Mode::Machine) => 11,
            Self::InstructionPageFault(_) => 12,
            Self::LoadPageFault(_) => 13,
            Self::StoreAmoPageFault(_) => 15,
        }
    }

    /// The value written to `mtval`.
    pub fn value(&self) -> u64 {
        match self {
            Self::InstructionAddressMisaligned(v)
            | Self::InstructionAccessFault(v)
            | Self::IllegalInstruction(v)
            | Self::Breakpoint(v)
            | Self::LoadAddressMisaligned(v)
            | Self::LoadAccessFault(v)
            | Self::StoreAmoAddressMisaligned(v)
            | Self::StoreAmoAccessFault(v)
            | Self::InstructionPageFault(v)
            | Self::LoadPageFault(v)
            | Self::StoreAmoPageFault(v) => *v,
            Self::EnvironmentCall(_) => 0,
        }
    }
}

impl Exception {
    /// The same cause, reported at `addr`. A device is handed an offset and never
    /// learns its own base, so turning that offset back into the address `mtval` is
    /// owed is the bus's job.
    pub fn at(self, addr: u64) -> Self {
        match self {
            Self::InstructionAddressMisaligned(_) => Self::InstructionAddressMisaligned(addr),
            Self::InstructionPageFault(_) => Self::InstructionPageFault(addr),
            Self::LoadPageFault(_) => Self::LoadPageFault(addr),
            Self::StoreAmoPageFault(_) => Self::StoreAmoPageFault(addr),
            Self::InstructionAccessFault(_) => Self::InstructionAccessFault(addr),
            Self::Breakpoint(_) => Self::Breakpoint(addr),
            Self::LoadAddressMisaligned(_) => Self::LoadAddressMisaligned(addr),
            Self::LoadAccessFault(_) => Self::LoadAccessFault(addr),
            Self::StoreAmoAddressMisaligned(_) => Self::StoreAmoAddressMisaligned(addr),
            Self::StoreAmoAccessFault(_) => Self::StoreAmoAccessFault(addr),
            Self::IllegalInstruction(_) | Self::EnvironmentCall(_) => self,
        }
    }
}

impl std::fmt::Display for Exception {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::InstructionAddressMisaligned(_) => "instruction address misaligned",
            Self::InstructionAccessFault(_) => "instruction access fault",
            Self::IllegalInstruction(_) => "illegal instruction",
            Self::Breakpoint(_) => "breakpoint",
            Self::LoadAddressMisaligned(_) => "load address misaligned",
            Self::LoadAccessFault(_) => "load access fault",
            Self::StoreAmoAddressMisaligned(_) => "store/amo address misaligned",
            Self::StoreAmoAccessFault(_) => "store/amo access fault",
            Self::InstructionPageFault(_) => "instruction page fault",
            Self::LoadPageFault(_) => "load page fault",
            Self::StoreAmoPageFault(_) => "store/amo page fault",
            Self::EnvironmentCall(_) => "environment call",
        };
        match self {
            Self::EnvironmentCall(mode) => write!(f, "{name} from {mode}"),
            Self::IllegalInstruction(v) => write!(f, "{name} {v:#010x}"),
            _ => write!(f, "{name} at {:#x}", self.value()),
        }
    }
}

/// An asynchronous trap. A wire is up, `mie` allows it through, and the mode it
/// targets is not holding it off. Only six exist, three per privilege level, so which
/// device asked is a question for the controller rather than for the cause.
///
/// The RISC-V Instruction Set Manual Volume II, table 14.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interrupt {
    SupervisorSoftware = 1,
    MachineSoftware = 3,
    SupervisorTimer = 5,
    MachineTimer = 7,
    SupervisorExternal = 9,
    MachineExternal = 11,
}

impl Interrupt {
    /// Decreasing priority, which is the order to offer them in when more than one is
    /// ready. Every machine-level interrupt outranks every supervisor-level one.
    /// The RISC-V Instruction Set Manual Volume II, 3.1.15.
    pub const PRIORITY: [Self; 6] = [
        Self::MachineExternal,
        Self::MachineSoftware,
        Self::MachineTimer,
        Self::SupervisorExternal,
        Self::SupervisorSoftware,
        Self::SupervisorTimer,
    ];
}

impl std::fmt::Display for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SupervisorSoftware => "supervisor software interrupt",
            Self::MachineSoftware => "machine software interrupt",
            Self::SupervisorTimer => "supervisor timer interrupt",
            Self::MachineTimer => "machine timer interrupt",
            Self::SupervisorExternal => "supervisor external interrupt",
            Self::MachineExternal => "machine external interrupt",
        })
    }
}

/// The top bit of `mcause`, which is what tells the two kinds of trap apart. They
/// share the register and the handler, so this is the only thing that distinguishes
/// them. The RISC-V Instruction Set Manual Volume II, 3.1.15.
pub const INTERRUPT: u64 = 1 << 63;

/// Either kind of trap, which is what a handler is entered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    Exception(Exception),
    Interrupt(Interrupt),
}

impl Trap {
    /// The cause number on its own. This is what indexes `medeleg` and `mideleg`, and
    /// what a vectored handler is spread out by.
    pub fn code(self) -> u64 {
        match self {
            Self::Exception(exception) => exception.cause(),
            Self::Interrupt(interrupt) => interrupt as u64,
        }
    }

    /// What `mcause` is written with: the code, and the top bit for an interrupt.
    pub fn cause(self) -> u64 {
        match self {
            Self::Exception(_) => self.code(),
            Self::Interrupt(_) => INTERRUPT | self.code(),
        }
    }

    /// What `mtval` is owed. An interrupt owes nothing: nothing about it is specific
    /// to an address or an instruction.
    pub fn value(self) -> u64 {
        match self {
            Self::Exception(exception) => exception.value(),
            Self::Interrupt(_) => 0,
        }
    }

    pub fn is_interrupt(self) -> bool {
        matches!(self, Self::Interrupt(_))
    }
}

impl From<Exception> for Trap {
    fn from(exception: Exception) -> Self {
        Self::Exception(exception)
    }
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exception(exception) => exception.fmt(f),
            Self::Interrupt(interrupt) => interrupt.fmt(f),
        }
    }
}

/// A trap, as it was actually taken: what happened, where, and where control went.
///
/// The interesting part is the pair of modes. `mcause` says why a hart trapped and
/// `mepc` says from where, but neither says whether `medeleg` sent it to a supervisor
/// or kept it, and a guest whose handler never runs usually has a delegation that does
/// not say what its author thought it said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken {
    /// The instruction it happened on, which is what the epc was written with.
    pub pc: u64,
    pub trap: Trap,
    /// The privilege it happened in.
    pub from: Mode,
    /// And the one that took it, which is what delegation decided.
    pub to: Mode,
    /// Where control went: the vector entry rather than the vector, so a vectored
    /// interrupt says which entry of the table it reached.
    pub handler: u64,
    /// Which trap this was, counted from the machine's reset, and assigned by the log
    /// rather than by whatever is recording the trap. Two entries a hundred apart came
    /// a hundred traps apart even when nothing between them was kept.
    pub seq: u64,
}

impl std::fmt::Display for Taken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:#x}: {}, {} to {}, handler {:#x}",
            self.pc, self.trap, self.from, self.to, self.handler
        )
    }
}

/// How many traps a hart remembers.
///
/// Enough to hold the run-up to whatever went wrong rather than a history: a guest that
/// is trapping in a loop fills this in microseconds, and the last few dozen are what say
/// which loop it is.
const DEPTH: usize = 64;

/// The traps a hart has taken lately, newest last, oldest forgotten.
///
/// A ring rather than a growing list, because the failure this exists for is a machine
/// taking traps far faster than anything is reading them, and a log that grows without
/// bound under exactly that load is a leak rather than a diagnostic.
#[derive(Debug, Clone)]
pub struct Log {
    entries: [Option<Taken>; DEPTH],
    /// Where the next one goes, which is also the oldest once the ring has wrapped.
    next: usize,
    /// How many have ever been taken, which is what numbers them and what says how
    /// many were dropped.
    taken: u64,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            entries: [None; DEPTH],
            next: 0,
            taken: 0,
        }
    }
}

impl Log {
    /// Remember a trap, forgetting the oldest if there is no room. Called from
    /// `take_trap` and nowhere else, so what is in here is what a handler was actually
    /// entered for: a trap with no handler installed is not taken and is not logged.
    pub fn push(&mut self, mut entry: Taken) {
        entry.seq = self.taken;
        self.entries[self.next] = Some(entry);
        self.next = (self.next + 1) % DEPTH;
        self.taken += 1;
    }

    /// How many traps this hart has ever taken, whether or not they are still here.
    pub fn taken(&self) -> u64 {
        self.taken
    }

    /// The traps still remembered, newest first, which is the order anything reading
    /// them wants: what just happened is what is being asked about.
    pub fn recent(&self) -> impl Iterator<Item = &Taken> {
        (1..=DEPTH)
            .map(move |back| self.entries[(self.next + DEPTH - back) % DEPTH].as_ref())
            .take_while(Option::is_some)
            .flatten()
    }

    /// The most recent one, which is what a panel with one line for it shows.
    pub fn last(&self) -> Option<&Taken> {
        self.recent().next()
    }
}
