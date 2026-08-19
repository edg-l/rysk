//! Synchronous exceptions, and the values they leave in `mcause` and `mtval`.

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
    EnvironmentCallFromMMode,
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
            Self::EnvironmentCallFromMMode => 11,
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
            | Self::StoreAmoAccessFault(v) => *v,
            Self::EnvironmentCallFromMMode => 0,
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
            Self::EnvironmentCallFromMMode => "environment call from m-mode",
        };
        match self {
            Self::EnvironmentCallFromMMode => write!(f, "{name}"),
            Self::IllegalInstruction(v) => write!(f, "{name} {v:#010x}"),
            _ => write!(f, "{name} at {:#x}", self.value()),
        }
    }
}
