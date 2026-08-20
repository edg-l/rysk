//! What a machine is made of. One table says what exists, where it sits and which
//! interrupt line it drives, and the bus is built from it, so nothing else has to
//! agree separately about an address.
//!
//! `virt` is the QEMU machine of the same name, which is where `DRAM_BASE` and every
//! other address here comes from.

use std::io;

use crate::{
    aplic::{self, Aplic},
    bus::{Bus, DRAM_BASE},
    clint::{self, Clint},
    cpu::Cpu,
    device::{Level, Line, Msi},
    dram::Dram,
    elf::{Error as ElfError, Image},
    fdt::Fdt,
    imsic::{self, Imsic},
    pci::{self, HostBridge, Ports, Root},
    plic::{self, Plic},
    trap::Trap,
    uart::{self, Keyboard, Uart},
};

/// Which interrupt architecture a machine is built with, in the spelling QEMU's `virt`
/// machine uses for the same choice, so that the same words describe the same machine
/// whichever of the two is running the image.
///
/// It is one choice rather than several because the parts only go together one way: a
/// hart is interrupted by a wire or by a message and not by both, and what converts
/// device wires into whichever it is follows from that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Aia {
    /// Wires all the way to the hart, through the controller the architecture had
    /// before this one.
    #[default]
    None,
    /// An APLIC delivering interrupts itself, still by wire, but with a domain per
    /// privilege level and sources that say what their wire means.
    Aplic,
    /// An APLIC that forwards to an interrupt file per hart, so what arrives at a hart
    /// is a message. This is the whole architecture.
    AplicImsic,
}

impl std::str::FromStr for Aia {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "none" => Ok(Self::None),
            "aplic" => Ok(Self::Aplic),
            "aplic-imsic" => Ok(Self::AplicImsic),
            _ => Err(format!("{name}: not none, aplic or aplic-imsic")),
        }
    }
}

impl Aia {
    /// What the harts of such a machine implement beyond the base, which a guest reads
    /// out of `riscv,isa` rather than out of `misa`: neither extension has a letter.
    pub fn isa(self) -> &'static str {
        match self {
            Self::AplicImsic => "_smaia_ssaia",
            _ => "",
        }
    }
}

/// The interrupt source the first serial port drives, as the `virt` machine wires it.
const UART_IRQ: usize = 10;
/// The first of the four the root complex's functions are swizzled onto.
const PCI_IRQ: usize = 32;
/// How many interrupt sources the controller answers for, which is as many as the
/// machine wires.
const SOURCES: u32 = (PCI_IRQ + pci::PINS - 1) as u32;

/// Which device number the host bridge is, which is where enumeration looks first.
const HOST_BRIDGE: usize = 0;

/// The phandles the tree refers to its interrupt controllers by. A device says which
/// controller its line runs to, so the controllers need names, and each hart has a
/// controller of its own. Zero is not a phandle, so they start at one and the platform
/// controllers take the numbers after the last hart's.
fn hart_intc(hart: usize) -> u32 {
    1 + hart as u32
}

/// The platform controllers, each with a number of its own whether or not the machine
/// has one, so that a node and everything pointing at it cannot disagree.
#[derive(Debug, Clone, Copy)]
enum Controller {
    Plic,
    Imsic(Level),
    Aplic(Level),
}

fn phandle(harts: usize, controller: Controller) -> u32 {
    let which = match controller {
        Controller::Plic => 0,
        Controller::Imsic(Level::Machine) => 1,
        Controller::Imsic(Level::Supervisor) => 2,
        Controller::Aplic(Level::Machine) => 3,
        Controller::Aplic(Level::Supervisor) => 4,
    };
    hart_intc(harts) + which
}

/// The cause each level's external interrupt is, which is what a controller says it
/// drives on a hart. The RISC-V Instruction Set Manual Volume II, table 14.
fn external(level: Level) -> u32 {
    match level {
        Level::Machine => 11,
        Level::Supervisor => 9,
    }
}

/// A controller's `interrupts-extended`: which cause it drives on which hart, as a
/// phandle and a cause number per entry, for every hart in turn. The order is what
/// numbers the controller's contexts, so it is not free to vary.
fn contexts(harts: usize, causes: &[u32]) -> Vec<u32> {
    (0..harts)
        .flat_map(|hart| {
            causes
                .iter()
                .flat_map(move |cause| [hart_intc(hart), *cause])
        })
        .collect()
}

/// What a device names as its interrupt parent, which is the controller the level a
/// guest runs at can reach: an APLIC has a domain per level and a PLIC a context per
/// level, so the two answers are different nodes and not different numbers.
fn parent(harts: usize, aia: Aia) -> u32 {
    match aia {
        Aia::None => phandle(harts, Controller::Plic),
        _ => phandle(harts, Controller::Aplic(Level::Supervisor)),
    }
}

/// How a device names one of that controller's sources. A PLIC takes a number; an
/// APLIC takes a number and what the wire under it means, since a source there is not
/// level-sensitive until it is told to be.
fn interrupt(aia: Aia, source: usize) -> Vec<u32> {
    /// The encoding a devicetree uses for a wire that means "interrupt while high",
    /// which is what everything on this machine drives.
    const LEVEL_HIGH: u32 = 4;
    match aia {
        Aia::None => vec![source as u32],
        _ => vec![source as u32, LEVEL_HIGH],
    }
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
pub fn boot(machine: &mut Machine, isa: &str, options: &Boot, aia: Aia) -> Keyboard {
    let harts = machine.harts.len();
    let keyboard = virt(machine, aia);
    let memory = machine.bus.dram.size();
    let at = fdt_base(memory);
    let tree = describe(isa, memory, harts, options, aia);
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
        let mut left = steps;
        while left > 0 {
            match tick(cpu, bus, left) {
                // A round that retired nothing still took one of the quantum: it
                // entered a handler, and a hart that did nothing else would otherwise
                // never give the others their turn.
                Ok(retired) => left -= retired.max(1),
                Err(trap) => return Some(Halt { hart, trap }),
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
        self.advance(hart, 1).err()
    }

    /// Run one hart to the end of the run of instructions at its `pc`, or for `max` of
    /// them, whichever comes first. Answers how many retired, or the trap nothing was
    /// installed to take.
    pub fn advance(&mut self, hart: usize, max: u64) -> Result<u64, Trap> {
        self.bus.poll();
        if !ready(&mut self.harts[hart], &self.bus) {
            return Ok(0);
        }
        tick(&mut self.harts[hart], &mut self.bus, max)
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

/// Run the instructions of one block on one hart, up to `max` of them: offer an
/// interrupt before each, then execute. Answers how many retired, or the trap that
/// nothing was installed to take, which is where a run ends.
///
/// The interrupt is offered per instruction rather than per block, so that one is
/// takeable between any two instructions and its latency is what it was. What a block
/// saves is the fetch: translating, finding the decoded instruction and deciding where
/// the run ends happen once for the whole of it.
///
/// Left to the compiler to inline or not, and it does not: the body is large, and
/// forcing it into the loop that runs a quantum is 3.6% slower on a Linux boot even
/// though it removes a call frame. What the frame saves and restores is registers the
/// body needs, so taking the boundary away does not remove that work, it spreads the
/// spills through the loop instead.
#[inline]
fn tick(cpu: &mut Cpu, bus: &mut Bus, max: u64) -> Result<u64, Trap> {
    if let Some(interrupt) = cpu.interrupt(bus) {
        let trap = Trap::Interrupt(interrupt);
        if !cpu.take_trap(trap) {
            return Err(trap);
        }
        return Ok(0);
    }
    let block = match cpu.block(bus) {
        Ok(block) => block,
        Err(exception) => {
            let trap = exception.into();
            if !cpu.take_trap(trap) {
                return Err(trap);
            }
            return Ok(0);
        }
    };
    let length = block.len.min(max as usize);
    let mut retired = 0;
    for index in 0..length {
        // The instructions of a block are consecutive, so the one at `pc` is the one
        // after the last, without asking where `pc` is.
        let decoded = cpu.blocks.at(block.start + index);
        if let Err(exception) = cpu.retire(bus, decoded) {
            let trap = exception.into();
            if !cpu.take_trap(trap) {
                return Err(trap);
            }
            // Whatever raised did not retire, and `pc` is the handler's now.
            return Ok(retired);
        }
        // Turns of this loop, not instructions: a fused pair is two instructions and
        // one turn. The quantum is how long a hart holds the host, and reading what a
        // pair is worth out of the instruction here would keep the whole of it in a
        // register across the instruction being executed, which costs 10% of the host
        // instructions a benchmark retires.
        retired += 1;
        // A park and a trap both leave the rest of the block for another round, since
        // neither one continues at the instruction after this.
        if cpu.waiting {
            break;
        }
        if index + 1 < length
            && let Some(interrupt) = cpu.interrupt(bus)
        {
            let trap = Trap::Interrupt(interrupt);
            if !cpu.take_trap(trap) {
                return Err(trap);
            }
            break;
        }
    }
    Ok(retired)
}

/// Attach what a `virt` machine has, and hand back the end of the serial port that
/// faces the world, so whatever is doing the typing can reach it.
///
/// This takes the whole machine rather than its bus because one of the things it
/// attaches is not only on the bus: an interrupt file is a hart's own, reached by that
/// hart's CSRs, and the page a message arrives at is the same registers seen from the
/// other side.
pub fn virt(machine: &mut Machine, aia: Aia) -> Keyboard {
    let harts = machine.harts.len();
    let bus = &mut machine.bus;
    let serial = Line::default();
    let keyboard = Keyboard::default();

    // The four wires the root complex swizzles its functions onto, in the order the
    // tree's `interrupt-map` lists them.
    let pins: [Line; pci::PINS] = std::array::from_fn(|_| Line::default());
    let wires = || {
        std::iter::once((UART_IRQ, serial.clone())).chain(
            pins.iter()
                .enumerate()
                .map(|(pin, line)| (PCI_IRQ + pin, line.clone())),
        )
    };

    // Where a message goes, if this machine has anywhere to put one: the interrupt
    // files decode the address, since they are the only thing here that answers to one.
    let msi = match aia {
        Aia::AplicImsic => {
            let imsic = Imsic::new(harts);
            for hart in &mut machine.harts {
                hart.imsic = Some(imsic.clone());
            }
            for level in [Level::Machine, Level::Supervisor] {
                bus.attach(
                    Imsic::base(level),
                    Imsic::size(harts),
                    Box::new(imsic.files(level)),
                );
            }
            Msi::new(move |addr, identity| {
                for level in [Level::Machine, Level::Supervisor] {
                    let files = Imsic::base(level)..Imsic::base(level) + Imsic::size(harts);
                    if files.contains(&addr) {
                        let hart = ((addr - files.start) / imsic::PAGE) as usize;
                        imsic.deliver(hart, level, identity);
                    }
                }
            })
        }
        _ => Msi::default(),
    };

    match aia {
        Aia::None => {
            let mut plic = Plic::new(harts);
            for (source, line) in wires() {
                plic.connect(source, line);
            }
            bus.attach(plic::BASE, plic::SIZE, Box::new(plic));
        }
        _ => {
            let aplic = Aplic::new(harts, msi.clone());
            for (source, line) in wires() {
                aplic.connect(source, line);
            }
            for level in [Level::Machine, Level::Supervisor] {
                bus.attach(
                    Aplic::base(level),
                    aplic::SIZE,
                    Box::new(aplic.domain(level)),
                );
            }
        }
    }

    let root = Root::new(pins, msi);
    root.plug(HOST_BRIDGE, Box::new(HostBridge));

    bus.attach(clint::BASE, clint::SIZE, Box::new(Clint::new(harts)));
    bus.attach(
        uart::BASE,
        uart::SIZE,
        Box::new(Uart::new(serial, keyboard.clone(), Box::new(io::stdout()))),
    );
    bus.attach(pci::ECAM, pci::ECAM_SIZE, Box::new(root.config()));
    bus.attach(pci::MMIO, pci::MMIO_SIZE, Box::new(root.window(pci::MMIO)));
    bus.attach(
        pci::MMIO64,
        pci::MMIO64_SIZE,
        Box::new(root.window(pci::MMIO64)),
    );
    bus.attach(pci::PIO, pci::PIO_SIZE, Box::new(Ports));
    keyboard
}

/// The `interrupt-map` of the root complex: for each of the four device numbers the
/// mask below keeps, and each of the four pins, which controller and which of its
/// sources that combination reaches.
///
/// Three cells for the child address, of which only the device number matters, one
/// for the pin, one for the controller, and however many that controller takes to name
/// a source. It has to say exactly what `pci::swizzle` computes, or an interrupt
/// arrives as another one.
fn interrupt_map(harts: usize, aia: Aia) -> Vec<u32> {
    (0..pci::PINS)
        .flat_map(|device| {
            (1..=pci::PINS as u32).flat_map(move |pin| {
                let source = PCI_IRQ + pci::swizzle(device, pin as u8);
                [(device << 11) as u32, 0, 0, pin, parent(harts, aia)]
                    .into_iter()
                    .chain(interrupt(aia, source))
            })
        })
        .collect()
}

/// One interrupt controller node. The two levels of an APLIC are the same node twice,
/// and so are the two levels of an IMSIC, since a domain and an interrupt file differ
/// only in what they say they drive and in what points at them.
fn describe_aia(fdt: &mut Fdt, harts: usize, aia: Aia) {
    for level in [Level::Machine, Level::Supervisor] {
        if aia == Aia::AplicImsic {
            fdt.begin_node(&format!("interrupt-controller@{:x}", Imsic::base(level)));
            fdt.strings("compatible", &["riscv,imsics"]);
            fdt.reg(Imsic::base(level), Imsic::size(harts));
            fdt.flag("interrupt-controller");
            fdt.flag("msi-controller");
            // Nothing names one of these by number: a message carries its own
            // identity, so a device says which controller and nothing else.
            fdt.cells("#interrupt-cells", &[0]);
            // Which cause each hart sees a message from this level as.
            fdt.cells("interrupts-extended", &contexts(harts, &[external(level)]));
            fdt.cells("riscv,num-ids", &[imsic::IDENTITIES]);
            fdt.cells("phandle", &[phandle(harts, Controller::Imsic(level))]);
            fdt.end_node();
        }

        fdt.begin_node(&format!("interrupt-controller@{:x}", Aplic::base(level)));
        fdt.strings("compatible", &["riscv,aplic"]);
        fdt.reg(Aplic::base(level), aplic::SIZE);
        fdt.flag("interrupt-controller");
        // A source and what its wire means, which is what an APLIC needs told and a
        // PLIC decides for itself.
        fdt.cells("#interrupt-cells", &[2]);
        fdt.cells("#address-cells", &[0]);
        fdt.cells("riscv,num-sources", &[SOURCES]);
        match aia {
            // Forwarding, so it says where it sends rather than what it drives.
            Aia::AplicImsic => {
                fdt.cells("msi-parent", &[phandle(harts, Controller::Imsic(level))]);
            }
            _ => {
                fdt.cells("interrupts-extended", &contexts(harts, &[external(level)]));
            }
        }
        // The machine-level domain owns every source and hands all of them to its
        // child, which is what leaves a supervisor able to configure them. The
        // property has two spellings and a consumer reads one or the other.
        if level == Level::Machine {
            let child = phandle(harts, Controller::Aplic(Level::Supervisor));
            let delegation = [child, 1, SOURCES];
            fdt.cells("riscv,children", &[child]);
            fdt.cells("riscv,delegate", &delegation);
            fdt.cells("riscv,delegation", &delegation);
        }
        fdt.cells("phandle", &[phandle(harts, Controller::Aplic(level))]);
        fdt.end_node();
    }
}

/// The same machine, described. Firmware and a kernel read this to find what `virt`
/// attached above, so the two are written next to each other on purpose.
pub fn describe(isa: &str, memory: u64, harts: usize, options: &Boot, aia: Aia) -> Vec<u8> {
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
    fdt.cells("interrupt-parent", &[parent(harts, aia)]);
    fdt.cells("interrupts", &interrupt(aia, UART_IRQ));
    // The rate a real part would divide down from. Nothing here measures time, but a
    // driver will not configure a port whose clock it does not know.
    fdt.cells("clock-frequency", &[3_686_400]);
    fdt.end_node();

    if aia == Aia::None {
        fdt.begin_node(&format!("plic@{:x}", plic::BASE));
        fdt.strings("compatible", &["riscv,plic0", "sifive,plic-1.0.0"]);
        fdt.reg(plic::BASE, plic::SIZE);
        fdt.flag("interrupt-controller");
        fdt.cells("#interrupt-cells", &[1]);
        fdt.cells("#address-cells", &[0]);
        // The contexts it drives, in the order the specification numbers them: every
        // hart's external interrupt at machine level and at supervisor level, causes
        // eleven and nine.
        fdt.cells(
            "interrupts-extended",
            &contexts(
                harts,
                &[external(Level::Machine), external(Level::Supervisor)],
            ),
        );
        // How many sources it has, which is the highest one anything is wired to.
        fdt.cells("riscv,ndev", &[SOURCES]);
        fdt.cells("phandle", &[phandle(harts, Controller::Plic)]);
        fdt.end_node();
    } else {
        describe_aia(&mut fdt, harts, aia);
    }

    // What the guest enumerates rather than what it is told: the tree says where
    // config space is and which addresses the windows hand out, and everything below
    // that the guest finds for itself.
    fdt.begin_node(&format!("pci@{:x}", pci::ECAM));
    fdt.strings("compatible", &["pci-host-ecam-generic"]);
    fdt.string("device_type", "pci");
    fdt.reg(pci::ECAM, pci::ECAM_SIZE);
    // Three cells to name an address below here, because the first of them holds
    // which space it is in and which function it belongs to rather than any of the
    // address. PCI Bus Binding to Open Firmware, 2.2.1.
    fdt.cells("#address-cells", &[3]);
    fdt.cells("#size-cells", &[2]);
    fdt.cells("#interrupt-cells", &[1]);
    fdt.cells("bus-range", &[0, 0xff]);
    fdt.cells("linux,pci-domain", &[0]);
    // Nothing on this machine caches, so a device reads what the hart wrote.
    fdt.flag("dma-coherent");
    // Each window as the space it is in, the address it starts at down there, the
    // address it starts at up here, and how big it is. The first cell's top two bits
    // are the space: one is ports, two is a 32-bit window and three a 64-bit one.
    fdt.cells(
        "ranges",
        &[
            0x0100_0000,
            0,
            0,
            (pci::PIO >> 32) as u32,
            pci::PIO as u32,
            (pci::PIO_SIZE >> 32) as u32,
            pci::PIO_SIZE as u32,
            0x0200_0000,
            (pci::MMIO >> 32) as u32,
            pci::MMIO as u32,
            (pci::MMIO >> 32) as u32,
            pci::MMIO as u32,
            (pci::MMIO_SIZE >> 32) as u32,
            pci::MMIO_SIZE as u32,
            0x0300_0000,
            (pci::MMIO64 >> 32) as u32,
            pci::MMIO64 as u32,
            (pci::MMIO64 >> 32) as u32,
            pci::MMIO64 as u32,
            (pci::MMIO64_SIZE >> 32) as u32,
            pci::MMIO64_SIZE as u32,
        ],
    );
    // Only the low two bits of the device number and the pin decide which wire an
    // interrupt lands on, which is what makes sixteen entries enough for a bus of
    // thirty-two devices.
    fdt.cells("interrupt-map-mask", &[0x1800, 0, 0, 7]);
    fdt.cells("interrupt-map", &interrupt_map(harts, aia));
    // Where a function sends a message, for one that has been given the capability to
    // send one rather than a wire to pull.
    if aia == Aia::AplicImsic {
        fdt.cells(
            "msi-parent",
            &[phandle(harts, Controller::Imsic(Level::Supervisor))],
        );
    }
    fdt.end_node();

    fdt.begin_node(&format!("clint@{:x}", clint::BASE));
    fdt.strings("compatible", &["riscv,clint0", "sifive,clint0"]);
    fdt.reg(clint::BASE, clint::SIZE);
    // Its two per hart: the machine software and machine timer interrupts, three and
    // seven.
    fdt.cells("interrupts-extended", &contexts(harts, &[3, 7]));
    fdt.end_node();

    fdt.end_node();
    fdt.end_node();
    fdt.finish(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine has to be able to cross a thread boundary, because phase 10 puts the
    /// window on the main thread and the harts on another. Nothing does that yet, so
    /// this is what notices the day something on the bus stops allowing it.
    #[test]
    fn a_machine_can_be_handed_to_another_thread() {
        fn assert_send<T: Send>() {}
        assert_send::<Machine>();
    }
}
