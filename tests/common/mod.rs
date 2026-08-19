//! Running a short program on a fresh machine, and looking at what it left behind.

#![allow(dead_code)]

mod asm;

pub use asm::*;

use rysk::{
    bus::DRAM_BASE, csr::Mode, device::Device, dram::DRAM_SIZE, machine::Machine, trap::Trap,
};

/// An address in dram that no test program occupies, for tests that need memory.
pub const SCRATCH: u64 = DRAM_BASE + 0x1000;

// ---------------------------------------------------------------- harness

/// A program to run, plus the register state it starts from.
pub struct Program {
    code: Vec<u8>,
    regs: Vec<(u32, u64)>,
    csrs: Vec<(usize, u64)>,
    memory: Vec<(u64, u64)>,
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
        memory: Vec::new(),
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

    /// Place a doubleword in dram before the run, for the page tables a translation
    /// test needs and for anything else that has to be there rather than written.
    pub fn memory(mut self, addr: u64, value: u64) -> Self {
        self.memory.push((addr, value));
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
    pub fn expect(self, expected: impl Into<Trap>) -> Machine {
        let (machine, stopped) = self.run_to_trap();
        assert_eq!(
            stopped,
            expected.into(),
            "the machine stopped for the wrong reason"
        );
        machine
    }

    /// Run to completion. Execution ends on the zeroed word past the last instruction,
    /// which decodes as an illegal instruction with nothing installed to take it.
    pub fn run(self) -> Machine {
        self.run_to_trap().0
    }

    fn run_to_trap(self) -> (Machine, Trap) {
        let mut machine = Machine::new(self.code, DRAM_SIZE, 1);
        let cpu = &mut machine.harts[0];
        for (reg, value) in self.regs {
            cpu.regs[reg as usize] = value;
        }
        for (csr, value) in self.csrs {
            cpu.csrs[csr] = value;
        }
        cpu.mode = self.mode;
        for (addr, value) in self.memory {
            machine.bus.dram.store(addr, 64, value);
        }
        for (base, size, device) in self.devices {
            machine.bus.attach(base, size, device);
        }
        let halt = machine.run();
        (machine, halt.trap)
    }
}

/// Run `code` and return the resulting machine.
pub fn run(code: &[u32]) -> Machine {
    prog(code).run()
}

/// What a test looks at after a run. A harness machine has one hart, so everything
/// about a hart here means that one.
pub trait Inspect {
    fn reg(&self, reg: u32) -> u64;
    fn csr(&self, csr: usize) -> u64;
    fn mode(&self) -> Mode;
    fn pc(&self) -> u64;
    fn load(&self, addr: u64, bytes: u64) -> u64;
}

impl Inspect for Machine {
    fn reg(&self, reg: u32) -> u64 {
        self.harts[0].regs[reg as usize]
    }

    fn csr(&self, csr: usize) -> u64 {
        self.harts[0].csrs[csr]
    }

    fn mode(&self) -> Mode {
        self.harts[0].mode
    }

    fn pc(&self) -> u64 {
        self.harts[0].pc
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
