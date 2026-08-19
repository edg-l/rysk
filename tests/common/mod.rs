//! Running a short program on a fresh machine, and looking at what it left behind.

#![allow(dead_code)]

mod asm;

pub use asm::*;

use rysk::{bus::DRAM_BASE, cpu::Cpu, csr::Mode, device::Device, dram::DRAM_SIZE, trap::Trap};

/// An address in dram that no test program occupies, for tests that need memory.
pub const SCRATCH: u64 = DRAM_BASE + 0x1000;

// ---------------------------------------------------------------- harness

/// A program to run, plus the register state it starts from.
pub struct Program {
    code: Vec<u8>,
    regs: Vec<(u32, u64)>,
    csrs: Vec<(usize, u64)>,
    mode: Mode,
    devices: Vec<(u64, u64, Box<dyn Device>)>,
}

/// Assemble `code` into a program starting from a zeroed register file.
pub fn prog(code: &[u32]) -> Program {
    image(code.iter().flat_map(|inst| inst.to_le_bytes()).collect())
}

/// The same for compressed instructions, which are half as wide. A program that mixes
/// the two writes the 32-bit ones as their two halves, low first.
pub fn halves(code: &[u16]) -> Program {
    image(code.iter().flat_map(|inst| inst.to_le_bytes()).collect())
}

fn image(code: Vec<u8>) -> Program {
    Program {
        code,
        regs: Vec::new(),
        csrs: Vec::new(),
        mode: Mode::Machine,
        devices: Vec::new(),
    }
}

impl Program {
    /// Preload a register, so a test does not have to build its inputs in assembly.
    pub fn reg(mut self, reg: u32, value: u64) -> Self {
        self.regs.push((reg, value));
        self
    }

    /// Preload a csr, for the same reason `reg` exists: a test that needs a handler
    /// installed should not have to write one in assembly first.
    pub fn csr(mut self, csr: usize, value: u64) -> Self {
        self.csrs.push((csr, value));
        self
    }

    /// Put a device on the bus, answering for `size` bytes from `base`.
    pub fn device(mut self, base: u64, size: u64, device: Box<dyn Device>) -> Self {
        self.devices.push((base, size, device));
        self
    }

    /// Start below machine mode. A real machine only gets there through an `xRET`,
    /// which the tests for that instruction go through; every other test just wants to
    /// be somewhere.
    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// Run until a trap nothing handles, and check it was the expected one.
    pub fn expect(self, expected: impl Into<Trap>) -> Cpu {
        let (cpu, stopped) = self.run_to_trap();
        assert_eq!(
            stopped,
            expected.into(),
            "the machine stopped for the wrong reason"
        );
        cpu
    }

    /// Run to completion. Execution ends on the zeroed word past the last instruction,
    /// which decodes as an illegal instruction with nothing installed to take it.
    pub fn run(self) -> Cpu {
        self.run_to_trap().0
    }

    fn run_to_trap(self) -> (Cpu, Trap) {
        let mut cpu = Cpu::new(self.code);
        for (reg, value) in self.regs {
            cpu.regs[reg as usize] = value;
        }
        for (csr, value) in self.csrs {
            cpu.csrs[csr] = value;
        }
        cpu.mode = self.mode;
        for (base, size, device) in self.devices {
            cpu.bus.attach(base, size, device);
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
        // Straight at dram, because inspecting a machine should not disturb it and a
        // device read can be an action.
        self.bus.dram.load(addr, bytes * 8)
    }
}
