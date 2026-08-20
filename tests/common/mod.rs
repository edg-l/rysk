//! Running a short program on a fresh machine, and looking at what it left behind.

#![allow(dead_code)]

mod asm;

pub use asm::*;

use rysk::{
    bus::DRAM_BASE,
    csr::Mode,
    device::{Device, Level, Wires},
    dram::DRAM_SIZE,
    imsic::Imsic,
    machine::{Machine, Schedule},
    trap::Trap,
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
    harts: usize,
    hart_regs: Vec<(usize, u32, u64)>,
    imsic: Option<Imsic>,
    /// The wires of the machine this will build, made here rather than there because a
    /// test wires its devices together before it has a machine to put them in.
    wires: Wires,
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
        harts: 1,
        hart_regs: Vec::new(),
        imsic: None,
        wires: Wires::default(),
    }
}

impl Program {
    /// Preload a register, so a test does not have to build its inputs in assembly.
    pub fn reg(mut self, reg: u32, value: u64) -> Self {
        self.regs.push((reg, value));
        self
    }

    /// Run the same program on `harts` harts.
    ///
    /// They all start at the reset address with the same registers, which is what a
    /// real machine does: `mhartid` is the only thing that tells one from another, and
    /// what each does about that is the program's to decide.
    pub fn harts(mut self, harts: usize) -> Self {
        self.harts = harts;
        self
    }

    /// Preload a register on one hart alone, for a program that has to tell the harts
    /// apart somewhere `mhartid` cannot be read: it is a machine-mode register, and a
    /// supervisor learns which hart it is from whatever started it.
    pub fn hart_reg(mut self, hart: usize, reg: u32, value: u64) -> Self {
        self.hart_regs.push((hart, reg, value));
        self
    }

    /// Preload a csr, for the same reason `reg` exists: a test that needs a handler
    /// installed should not have to write one in assembly first.
    pub fn csr(mut self, csr: usize, value: u64) -> Self {
        self.csrs.push((csr, value));
        self
    }

    /// Give every hart an interrupt file at each level, which is what makes the
    /// registers Smaia and Ssaia add exist. The pages messages arrive at go on the bus
    /// at the same time, since they are the same registers from the other side.
    pub fn imsic(mut self, imsic: Imsic) -> Self {
        self.imsic = Some(imsic);
        self
    }

    /// Put a device on the bus, answering for `size` bytes from `base`.
    /// The wires of the machine this will build, to take a device's line from. A line
    /// from anywhere else is one no access would be seen to move, so it would only
    /// reach a controller at the next round's poll.
    pub fn wires(&self) -> Wires {
        self.wires.clone()
    }

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

    /// Run every hart, one instruction each in turn, until they have all parked on a
    /// `wfi`.
    ///
    /// That is how a program with more than one hart says it is finished: a hart with
    /// no work left waits rather than running off the end into a trap, which would
    /// stop the machine before the others had finished. A step each rather than a
    /// quantum each, because a test wants the harts interleaved as finely as the
    /// machine can interleave them.
    pub fn parked(self) -> Machine {
        let mut machine = self.build();
        loop {
            let mut parked = true;
            for hart in 0..machine.harts.len() {
                if let Some(trap) = machine.step(hart) {
                    panic!(
                        "hart {hart} stopped on {trap} at {:#x} instead of parking",
                        machine.harts[hart].pc
                    );
                }
                parked &= machine.harts[hart].waiting;
            }
            if parked {
                return machine;
            }
        }
    }

    /// The same, with every hart on a host thread of its own and running at once.
    ///
    /// `parked` interleaves the harts as finely as one thread can, which is between
    /// whole instructions and no finer. This is the only way a hart can be inside
    /// another's instruction, which is what anything claiming to be atomic has to
    /// survive, so a test of an atomic is only a test of it under this.
    pub fn contended(self) -> Machine {
        let mut machine = self.build();
        machine.schedule = Schedule::Threads;
        machine.run_until_parked();
        machine
    }

    fn run_to_trap(self) -> (Machine, Trap) {
        let mut machine = self.build();
        let halt = machine.run().expect("nothing asked this machine to stop");
        (machine, halt.trap)
    }

    /// The machine this program describes, before it has run.
    fn build(self) -> Machine {
        let mut machine = Machine::new(self.code, DRAM_SIZE, self.harts);
        machine.bus.wires = self.wires;
        for cpu in &mut machine.harts {
            for (reg, value) in &self.regs {
                cpu.regs[*reg as usize] = *value;
            }
            for (csr, value) in &self.csrs {
                cpu.csrs[*csr] = *value;
            }
            cpu.mode = self.mode;
        }
        for (hart, reg, value) in self.hart_regs {
            machine.harts[hart].regs[reg as usize] = value;
        }
        for (addr, value) in self.memory {
            machine.bus.dram.store(addr, 64, value);
        }
        if let Some(imsic) = &self.imsic {
            for cpu in &mut machine.harts {
                cpu.imsic = Some(imsic.clone());
            }
            for level in [Level::Machine, Level::Supervisor] {
                machine.bus.attach(
                    Imsic::base(level),
                    Imsic::size(self.harts),
                    Box::new(imsic.files(level)),
                );
            }
        }
        for (base, size, device) in self.devices {
            machine.bus.attach(base, size, device);
        }
        machine
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
