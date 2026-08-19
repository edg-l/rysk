//! What a machine is made of. One table says what exists, where it sits and which
//! interrupt line it drives, and the bus is built from it, so nothing else has to
//! agree separately about an address.
//!
//! `virt` is the QEMU machine of the same name, which is where `DRAM_BASE` and every
//! other address here comes from.

use std::io;

use crate::{
    bus::{Bus, DRAM_BASE},
    clint::{self, Clint},
    cpu::Cpu,
    device::Line,
    dram::Dram,
    elf::{Error as ElfError, Image},
    fdt::Fdt,
    plic::{self, Plic},
    trap::Trap,
    uart::{self, Keyboard, Uart},
};

/// The interrupt source the first serial port drives, as the `virt` machine wires it.
const UART_IRQ: usize = 10;
/// How many interrupt sources the controller answers for, which is as many as the
/// machine wires.
const SOURCES: u32 = UART_IRQ as u32;

/// The phandles the tree refers to its interrupt controllers by. A device says which
/// controller its line runs to, so the controllers need names, and each hart has a
/// controller of its own. Zero is not a phandle, so they start at one and the platform
/// controller takes the number after the last hart's.
fn hart_intc(hart: usize) -> u32 {
    1 + hart as u32
}

fn plic_phandle(harts: usize) -> u32 {
    hart_intc(harts)
}

/// A controller's `interrupts-extended`: which cause it drives on which hart, as a
/// phandle and a cause number per entry, for every hart in turn. The order is what
/// numbers the controller's contexts, so it is not free to vary.
fn contexts(harts: usize, causes: [u32; 2]) -> Vec<u32> {
    (0..harts)
        .flat_map(|hart| causes.map(|cause| [hart_intc(hart), cause]))
        .flatten()
        .collect()
}

/// Where the tree is left for the guest to find: high in dram, out of the way of an
/// image loaded at the bottom of it, and page aligned because a guest will map it.
pub fn fdt_base(memory: u64) -> u64 {
    DRAM_BASE + memory - 0x10_0000
}

/// What a guest is told that is not a device: the command line it was started with,
/// and where its initial ramdisk was left.
#[derive(Debug, Default)]
pub struct Boot {
    pub bootargs: Option<String>,
    pub initrd: Option<(u64, u64)>,
}

/// Attach the machine's devices, describe them, and leave the description where the
/// guest is told to look: `a0` is the hart reading it and `a1` is the tree, which is
/// the handover every RISC-V kernel expects from whatever ran before it.
///
/// Every hart is handed the same tree and its own id, because every hart comes out of
/// reset at the same address: firmware is what picks one to boot and parks the others.
pub fn boot(machine: &mut Machine, isa: &str, options: &Boot) -> Keyboard {
    let harts = machine.harts.len();
    let keyboard = virt(&mut machine.bus, harts);
    let memory = machine.bus.dram.size();
    let at = fdt_base(memory);
    let tree = describe(isa, memory, harts, options);
    assert!(
        machine.bus.dram.write(at, &tree, 0),
        "the device tree does not fit in dram"
    );
    for hart in &mut machine.harts {
        hart.regs[10] = hart.hart as u64;
        hart.regs[11] = at;
    }
    keyboard
}

/// How a run ended: a trap that nothing was installed to take, and the hart it
/// happened on. With no handler in the vector the trap would use there is nowhere for
/// it to go, so that is where a program ends, normally by running off its own code
/// into the zeroed dram behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Halt {
    pub hart: usize,
    pub trap: Trap,
}

impl std::fmt::Display for Halt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hart {}: {}", self.hart, self.trap)
    }
}

/// The harts, and the memory and devices they share.
///
/// A hart does not own the machine it is part of: the address space arrives at every
/// instruction as an argument, which is what lets several harts have the same one and,
/// later, what will let a device reach it without holding the bus that holds the
/// device.
#[derive(Debug)]
pub struct Machine {
    pub harts: Vec<Cpu>,
    pub bus: Bus,
    /// Instructions a hart runs before the next one gets a turn.
    ///
    /// Long enough that switching costs nothing measurable, short enough that a hart
    /// spinning on a word only another hart can write does not hold the machine for
    /// long. It is a property of the machine rather than of the loop so that a
    /// frontend which needs one run to repeat another can fix it.
    pub quantum: u64,
}

/// The default for `Machine::quantum`.
const QUANTUM: u64 = 4096;

impl Machine {
    /// A machine with `memory` bytes of dram and `harts` harts, running `code` placed
    /// at the bottom of memory.
    ///
    /// Every hart starts at the reset address with its stack pointer at the top of
    /// memory, which is a convenience for a flat binary and nothing a real machine
    /// does: firmware gives each hart a stack of its own before any of them needs one.
    pub fn new(code: Vec<u8>, memory: u64, harts: usize) -> Self {
        assert!(harts > 0, "a machine needs at least one hart");
        let mut machine = Self {
            harts: (0..harts).map(Cpu::new).collect(),
            bus: Bus::new(Dram::with_size(code, memory), harts),
            quantum: QUANTUM,
        };
        for hart in &mut machine.harts {
            hart.regs[2] = DRAM_BASE + memory;
        }
        machine
    }

    /// Place an image in memory and start every hart at its entry point.
    pub fn from_elf(image: &Image, memory: u64, harts: usize) -> Result<Self, ElfError> {
        let mut machine = Self::new(Vec::new(), memory, harts);
        for segment in &image.segments {
            if !machine
                .bus
                .dram
                .write(segment.addr, &segment.bytes, segment.zeroes)
            {
                return Err(ElfError::SegmentOutsideDram(segment.addr));
            }
        }
        for hart in &mut machine.harts {
            hart.pc = image.entry;
            hart.next_pc = image.entry;
        }
        Ok(machine)
    }

    /// Run until a trap that nothing is installed to handle, and say which hart raised
    /// it.
    ///
    /// The harts take turns a quantum at a time. One host thread runs all of them, so
    /// a switch only ever happens between whole instructions: an atomic is atomic
    /// because nothing can interleave with it, rather than because anything makes it
    /// so. The schedule itself is fixed, though a run is not yet reproducible, since
    /// the devices still advance with the wall clock.
    pub fn run(&mut self) -> Halt {
        loop {
            self.bus.poll();
            let mut ran = false;
            for hart in 0..self.harts.len() {
                if !ready(&mut self.harts[hart], &self.bus) {
                    continue;
                }
                ran = true;
                if let Some(halt) = self.run_hart(hart, self.quantum) {
                    return halt;
                }
            }
            // Every hart is parked, so nothing but a device can change what any of
            // them will do next, and the devices advance with the wall clock.
            if !ran {
                std::hint::spin_loop();
            }
        }
    }

    /// Run one hart, which has to be one that may run, for up to `steps` instructions,
    /// stopping early if it parks on a `wfi`. Answers the trap that nothing handled,
    /// if that is how it stopped.
    fn run_hart(&mut self, hart: usize, steps: u64) -> Option<Halt> {
        let cpu = &mut self.harts[hart];
        let bus = &mut self.bus;
        for _ in 0..steps {
            if let Some(trap) = tick(cpu, bus) {
                return Some(Halt { hart, trap });
            }
            if cpu.waiting {
                break;
            }
        }
        None
    }

    /// Run a single instruction on one hart, for a caller that needs to look at the
    /// machine between instructions rather than leave it running. A hart that is
    /// parked and has nothing to wake it runs nothing.
    pub fn step(&mut self, hart: usize) -> Option<Trap> {
        self.bus.poll();
        if !ready(&mut self.harts[hart], &self.bus) {
            return None;
        }
        tick(&mut self.harts[hart], &mut self.bus)
    }
}

/// Whether a hart may execute at all. A parked one may once something it has enabled
/// is pending, which is the only thing that can change while it is not executing.
#[inline]
fn ready(cpu: &mut Cpu, bus: &Bus) -> bool {
    if cpu.waiting {
        cpu.wake(bus);
    }
    !cpu.waiting
}

/// One instruction on one hart: offer it an interrupt, then execute. Answers the trap
/// that nothing was installed to take, which is where a run ends.
#[inline]
fn tick(cpu: &mut Cpu, bus: &mut Bus) -> Option<Trap> {
    if let Some(interrupt) = cpu.interrupt(bus) {
        let trap = Trap::Interrupt(interrupt);
        if !cpu.take_trap(trap) {
            return Some(trap);
        }
    }
    if let Err(exception) = cpu.step(bus)
        && !cpu.take_trap(exception.into())
    {
        return Some(exception.into());
    }
    None
}

/// Attach what a `virt` machine has, and hand back the end of the serial port that
/// faces the world, so whatever is doing the typing can reach it.
pub fn virt(bus: &mut Bus, harts: usize) -> Keyboard {
    let serial = Line::default();
    let keyboard = Keyboard::default();
    let mut plic = Plic::new(harts);
    plic.connect(UART_IRQ, serial.clone());

    bus.attach(clint::BASE, clint::SIZE, Box::new(Clint::new(harts)));
    bus.attach(plic::BASE, plic::SIZE, Box::new(plic));
    bus.attach(
        uart::BASE,
        uart::SIZE,
        Box::new(Uart::new(serial, keyboard.clone(), Box::new(io::stdout()))),
    );
    keyboard
}

/// The same machine, described. Firmware and a kernel read this to find what `virt`
/// attached above, so the two are written next to each other on purpose.
pub fn describe(isa: &str, memory: u64, harts: usize, options: &Boot) -> Vec<u8> {
    let mut fdt = Fdt::new();
    fdt.begin_node("");
    fdt.cells("#address-cells", &[2]);
    fdt.cells("#size-cells", &[2]);
    fdt.strings("compatible", &["riscv-virtio"]);
    fdt.string("model", "riscv-virtio,rysk");

    fdt.begin_node("chosen");
    fdt.string("stdout-path", "/soc/serial@10000000");
    if let Some(bootargs) = &options.bootargs {
        fdt.string("bootargs", bootargs);
    }
    // Where the ramdisk is, as two cells each, which is how a kernel finds the root
    // filesystem it was handed rather than one it has to go looking for.
    if let Some((start, end)) = options.initrd {
        fdt.cells("linux,initrd-start", &[(start >> 32) as u32, start as u32]);
        fdt.cells("linux,initrd-end", &[(end >> 32) as u32, end as u32]);
    }
    fdt.end_node();

    fdt.begin_node("cpus");
    fdt.cells("#address-cells", &[1]);
    fdt.cells("#size-cells", &[0]);
    // What `mtime` counts in, which is every timeout a guest will ever compute.
    fdt.cells("timebase-frequency", &[clint::FREQUENCY as u32]);
    // One node per hart, named and numbered by the `mhartid` it reports, which is how
    // everything else in the tree refers to a hart.
    for hart in 0..harts {
        fdt.begin_node(&format!("cpu@{hart}"));
        fdt.string("device_type", "cpu");
        fdt.cells("reg", &[hart as u32]);
        fdt.string("status", "okay");
        fdt.strings("compatible", &["riscv"]);
        fdt.string("riscv,isa", isa);
        fdt.string("mmu-type", "riscv,sv39");
        // The hart's own interrupt controller is the three bits of `mip` that reach it
        // directly, and it is what every other controller ultimately reports to.
        fdt.begin_node("interrupt-controller");
        fdt.cells("#interrupt-cells", &[1]);
        fdt.flag("interrupt-controller");
        fdt.strings("compatible", &["riscv,cpu-intc"]);
        fdt.cells("phandle", &[hart_intc(hart)]);
        fdt.end_node();
        fdt.end_node();
    }
    fdt.end_node();

    fdt.begin_node(&format!("memory@{DRAM_BASE:x}"));
    fdt.string("device_type", "memory");
    fdt.reg(DRAM_BASE, memory);
    fdt.end_node();

    fdt.begin_node("soc");
    fdt.cells("#address-cells", &[2]);
    fdt.cells("#size-cells", &[2]);
    fdt.strings("compatible", &["simple-bus"]);
    // An empty `ranges` means the addresses below are the ones the cpu uses.
    fdt.flag("ranges");

    fdt.begin_node(&format!("serial@{:x}", uart::BASE));
    fdt.strings("compatible", &["ns16550a"]);
    fdt.reg(uart::BASE, uart::SIZE);
    fdt.cells("interrupt-parent", &[plic_phandle(harts)]);
    fdt.cells("interrupts", &[UART_IRQ as u32]);
    // The rate a real part would divide down from. Nothing here measures time, but a
    // driver will not configure a port whose clock it does not know.
    fdt.cells("clock-frequency", &[3_686_400]);
    fdt.end_node();

    fdt.begin_node(&format!("plic@{:x}", plic::BASE));
    fdt.strings("compatible", &["riscv,plic0", "sifive,plic-1.0.0"]);
    fdt.reg(plic::BASE, plic::SIZE);
    fdt.flag("interrupt-controller");
    fdt.cells("#interrupt-cells", &[1]);
    fdt.cells("#address-cells", &[0]);
    // The contexts it drives, in the order the specification numbers them: every
    // hart's external interrupt at machine level and at supervisor level, causes
    // eleven and nine.
    fdt.cells("interrupts-extended", &contexts(harts, [11, 9]));
    // How many sources it has, which is the highest one anything is wired to.
    fdt.cells("riscv,ndev", &[SOURCES]);
    fdt.cells("phandle", &[plic_phandle(harts)]);
    fdt.end_node();

    fdt.begin_node(&format!("clint@{:x}", clint::BASE));
    fdt.strings("compatible", &["riscv,clint0", "sifive,clint0"]);
    fdt.reg(clint::BASE, clint::SIZE);
    // Its two per hart: the machine software and machine timer interrupts, three and
    // seven.
    fdt.cells("interrupts-extended", &contexts(harts, [3, 7]));
    fdt.end_node();

    fdt.end_node();
    fdt.end_node();
    fdt.finish(&[])
}
