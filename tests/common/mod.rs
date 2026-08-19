//! Running a short program on a fresh machine, and looking at what it left behind.

#![allow(dead_code)]

mod asm;

pub use asm::*;

use rysk::{bus::DRAM_BASE, cpu::Cpu, dram::DRAM_SIZE, exception::Exception};

/// An address in dram that no test program occupies, for tests that need memory.
pub const SCRATCH: u64 = DRAM_BASE + 0x1000;

// ---------------------------------------------------------------- harness

/// A program to run, plus the register state it starts from.
pub struct Program {
    code: Vec<u32>,
    regs: Vec<(u32, u64)>,
}

/// Assemble `code` into a program starting from a zeroed register file.
pub fn prog(code: &[u32]) -> Program {
    Program {
        code: code.to_vec(),
        regs: Vec::new(),
    }
}

impl Program {
    /// Preload a register, so a test does not have to build its inputs in assembly.
    pub fn reg(mut self, reg: u32, value: u64) -> Self {
        self.regs.push((reg, value));
        self
    }

    /// Run until a trap nothing handles, and check it was the expected one.
    pub fn expect(self, expected: Exception) -> Cpu {
        let (cpu, stopped) = self.run_to_trap();
        assert_eq!(
            stopped, expected,
            "the machine stopped for the wrong reason"
        );
        cpu
    }

    /// Run to completion. Execution ends on the zeroed word past the last instruction,
    /// which decodes as an illegal instruction with nothing installed to take it.
    pub fn run(self) -> Cpu {
        self.run_to_trap().0
    }

    fn run_to_trap(self) -> (Cpu, Exception) {
        let mut bytes = Vec::with_capacity(self.code.len() * 4);
        for inst in &self.code {
            bytes.extend_from_slice(&inst.to_le_bytes());
        }

        let mut cpu = Cpu::new(bytes);
        for (reg, value) in self.regs {
            cpu.regs[reg as usize] = value;
        }
        let stopped = cpu.run();
        (cpu, stopped)
    }
}

/// Run `code` and return the resulting machine.
pub fn run(code: &[u32]) -> Cpu {
    prog(code).run()
}

pub trait Inspect {
    fn reg(&self, reg: u32) -> u64;
    fn load(&self, addr: u64, bytes: u64) -> u64;
}

impl Inspect for Cpu {
    fn reg(&self, reg: u32) -> u64 {
        self.regs[reg as usize]
    }

    fn load(&self, addr: u64, bytes: u64) -> u64 {
        assert!(
            (DRAM_BASE..DRAM_BASE + DRAM_SIZE).contains(&addr),
            "address {addr:#x} is outside dram"
        );
        self.bus.load(addr, bytes * 8).expect("load failed")
    }
}
