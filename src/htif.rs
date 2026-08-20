//! The host/target interface the riscv-tests corpus signals its result through.
//!
//! A test ends by storing a non-zero doubleword to the address of its `tohost` symbol:
//! `1` means every case passed, and `(n << 1) | 1` that case `n` failed. Nothing else
//! about the protocol is needed to run the corpus.

use crate::{
    elf::Image,
    machine::Machine,
    trap::{Exception, Trap},
};

/// How a run against the corpus ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    /// The corpus numbers its cases from one, and reports the first that failed.
    Failed(u64),
    /// A trap with no handler installed, which for the corpus means its own handler
    /// never got the chance to write `tohost`.
    Trapped {
        trap: Trap,
        pc: u64,
    },
    /// Still running after `max_steps`, which for a corpus test means a loop it cannot
    /// leave.
    TimedOut,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passed => write!(f, "passed"),
            Self::Failed(n) => write!(f, "failed at test {n}"),
            Self::Trapped { trap, pc } => write!(f, "{trap}, pc {pc:#x}"),
            Self::TimedOut => write!(f, "timed out"),
        }
    }
}

/// The address an image signals through, if it has one.
pub fn tohost(image: &Image) -> Option<u64> {
    image.symbols.get("tohost").copied()
}

/// Run until the image writes to `tohost`, traps with nothing to handle it, or runs
/// longer than `max_steps`.
///
/// A block at a time rather than through the machine's own loop, because the address
/// has to be read back between them: the corpus reports by writing it and then spinning
/// on a jump to itself, which is where a block ends. The corpus is single-hart, so the
/// hart it runs is hart zero.
pub fn run(machine: &mut Machine, tohost: u64, max_steps: u64) -> Outcome {
    let mut left = max_steps;
    while left > 0 {
        match machine.advance(0, left) {
            // A round that retired nothing is a hart that took a trap or has parked,
            // and counts as one so that neither spins here forever.
            Ok(retired) => left -= retired.max(1),
            Err(trap) => {
                return Outcome::Trapped {
                    trap,
                    pc: machine.harts[0].pc,
                };
            }
        }

        // The store lands through the bus like any other, so reading it back is how
        // the write is noticed.
        match machine.bus.load(tohost, 64) {
            Ok(0) => {}
            Ok(1) => return Outcome::Passed,
            Ok(status) => return Outcome::Failed(status >> 1),
            Err(_) => {
                return Outcome::Trapped {
                    trap: Exception::LoadAccessFault(tohost).into(),
                    pc: machine.harts[0].pc,
                };
            }
        }
    }
    Outcome::TimedOut
}
