//! One API over the machine, for everything that has to look at it rather than run it.
//!
//! The window's panels, the control channel and a debugger are three encodings of what
//! is here, which is why it exists before any of them: a surface written underneath the
//! first panel is written once, and a surface written after it is written twice.
//!
//! **Values, not renderings.** Nothing here formats anything. A register is a number, a
//! device says what it is doing as named values, and how any of it looks on a screen or
//! goes down a socket is the caller's business. `Inst` prints itself, which is the one
//! place that line is crossed, and it is crossed because the disassembler was already
//! there.
//!
//! **Looking at a machine may not change it.** That is not a style: two of the reads a
//! frontend would obviously want are actions rather than questions on this hardware. A
//! serial port's receive register consumes the byte it answers with and an interrupt
//! file's top register claims the interrupt it names, so a panel polling either of them
//! would eat the guest's input and its interrupts. So an inspector reads memory and
//! never a device's registers, and reads a CSR out of storage rather than through the
//! path an instruction takes. What a device is doing it says through `Device::describe`,
//! which is a question by construction.
//!
//! **Exact, because the machine is stopped.** Everything here is read while no hart is
//! executing, which is what a run returning gives: the harts stop between instructions
//! and the run comes back, and a caller holding the session then holds every one of
//! them at a boundary. There is no second, approximate way to read the same value.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::{
    csr::{self, Mode},
    device::Report,
    inst::{self, Inst, Op, decode},
    machine::{Halt, Machine, Running, State},
    mmu::Access,
    rvc,
    trap::{Exception, Taken, Trap},
};

/// Where an address is in: the space the bus decodes, or the space a hart's page table
/// gives it.
///
/// The two are the same thing on a machine with no table installed, which is what a
/// hart before its kernel is looking at, and quite different afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Address {
    /// What the bus decodes. Where a device sits and where memory is, and what a page
    /// table walk ends at.
    Physical(u64),
    /// What the guest's own pointers hold, read as the hart currently translates them,
    /// which means through its mode, its `satp` and the permissions in its tables.
    Virtual(u64),
}

impl Address {
    /// The number itself, whichever space it is in.
    pub fn addr(self) -> u64 {
        match self {
            Self::Physical(addr) | Self::Virtual(addr) => addr,
        }
    }
}

/// Why a run came back.
///
/// Every one of these leaves the machine between instructions, so what a session
/// answers about it afterwards is exact rather than nearly current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// A trap nothing was installed to take, which is where a program ends. Nothing
    /// carries on from this: the machine is halted.
    Halted(Halt),
    /// Something asked the machine to pause, and it did. A resume carries on.
    Paused,
    /// A hart reached an address a breakpoint was set on, and has not executed the
    /// instruction there.
    Breakpoint { hart: usize, pc: u64 },
    /// A watched address changed, and the hart that changed it has finished the
    /// instruction that did.
    Watchpoint {
        hart: usize,
        at: Address,
        was: u64,
        now: u64,
    },
    /// The instructions asked for have retired.
    Stepped { hart: usize, retired: u64 },
    /// A hart entered a trap handler, which is what `step_to_trap` was waiting for.
    Trapped { hart: usize, taken: Taken },
}

impl Stop {
    /// Whether the machine can be asked to carry on. A halt cannot; everything else is
    /// a machine sitting still with somewhere to go.
    pub fn resumable(&self) -> bool {
        !matches!(self, Self::Halted(_))
    }
}

/// A hart's registers, as one answer rather than thirty-two.
///
/// `pc` is the instruction that will execute next, since a session reads a machine
/// between instructions and there is no instruction part way through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registers {
    pub hart: usize,
    pub pc: u64,
    pub mode: Mode,
    /// The integer registers, `x0` first, which is always zero.
    pub x: [u64; 32],
    /// The floating-point registers, in the boxed form the register file keeps: a
    /// single is held with every bit above it set.
    pub f: [u64; 32],
    /// Whether a `wfi` has parked it, in which case it is executing nothing until an
    /// interrupt it enabled arrives.
    pub waiting: bool,
    /// How many instructions it has retired, which is what says whether a machine that
    /// looks stuck is stuck or merely slow.
    pub instret: u64,
}

/// A control register: the number an instruction names it by, the name a person does,
/// and what it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Csr {
    pub addr: usize,
    pub name: &'static str,
    pub value: u64,
}

/// One instruction, disassembled.
///
/// It is decoded out of memory rather than taken from the hart's decoded-instruction
/// cache, and that is not an accident: a run in that cache may hold a fused pair, whose
/// instruction is one this machine invented and which never appeared in the image. A
/// disassembler has to show what is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Where it is, in whichever space the disassembly was asked for.
    pub at: u64,
    /// The bytes it is made of, which is two for a compressed instruction and four
    /// otherwise.
    pub encoding: u32,
    pub length: u8,
    /// What it decodes to, or nothing if it decodes to no instruction at all. Printing
    /// it is what disassembles it.
    pub inst: Option<Inst>,
    /// Where it goes, for the instructions that go somewhere known: a branch or a
    /// `jal`, whose target is an offset from here rather than an address. A `jalr`
    /// names a register and so has no target until it executes.
    pub target: Option<u64>,
    /// The symbol it falls in, and how far in.
    pub symbol: Option<(String, u64)>,
}

/// A window of memory, and what answered for each byte of it.
///
/// A byte is missing where nothing answered: an address outside memory, a page the
/// hart cannot translate, or a device. **A device is never read.** A read of a
/// device's registers is an action rather than a question on this machine, so an
/// inspector reads memory alone and asks a device what it is doing instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub at: Address,
    pub bytes: Vec<Option<u8>>,
}

impl Window {
    /// The window as a number of `size` bytes at `offset` into it, little end first,
    /// or nothing if any byte of it is missing.
    pub fn word(&self, offset: usize, size: usize) -> Option<u64> {
        let mut value = 0u64;
        for index in 0..size {
            value |= u64::from((*self.bytes.get(offset + index)?)?) << (8 * index);
        }
        Some(value)
    }
}

/// A watched address: what to compare, and what it last held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Watch {
    at: Address,
    size: usize,
    /// What it held when it was last looked at, so that a change is a comparison rather
    /// than a hook in the store path. Nothing if it did not answer then.
    was: Option<u64>,
}

/// The symbols an image was loaded with, in both directions.
///
/// An ELF gives names to addresses and nothing else: no sizes, no kinds. So the span a
/// name covers is taken to reach the next name, which is what every tool that resolves
/// an address this way does, and the last name is given a bounded span rather than the
/// rest of the address space.
#[derive(Debug, Clone, Default)]
pub struct Symbols {
    by_name: BTreeMap<String, u64>,
    /// The same names sorted by address, which is what an address is resolved through.
    by_addr: Vec<(u64, String)>,
}

/// How far past the last symbol still counts as being in it. Generous for one function
/// and far short of naming the whole of memory after whatever happened to be last.
const LAST_SPAN: u64 = 64 * 1024;

impl Symbols {
    /// Take a symbol table, which is names to addresses and nothing more.
    ///
    /// Every name is kept, so a breakpoint can be set on any of them. Only one name per
    /// *address* is kept for resolving one, since an address that answers with two names
    /// answers with neither: the last entry given for an address is the one it resolves
    /// to. That is what makes a table given by hand beat the image's own, so long as the
    /// image's are handed over first.
    pub fn new(symbols: impl IntoIterator<Item = (String, u64)>) -> Self {
        let mut by_name = BTreeMap::new();
        // Sorted by address for free, and one entry per address by construction.
        let mut naming: BTreeMap<u64, String> = BTreeMap::new();
        for (name, addr) in symbols {
            naming.insert(addr, name.clone());
            by_name.insert(name, addr);
        }
        Self {
            by_name,
            by_addr: naming.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_addr.is_empty()
    }

    /// Where a name is, which is what a breakpoint set by name needs.
    pub fn address(&self, name: &str) -> Option<u64> {
        self.by_name.get(name).copied()
    }

    /// The symbol an address falls in, and how far into it, or nothing if it falls past
    /// the end of the last one or before the start of the first.
    pub fn nearest(&self, addr: u64) -> Option<(&str, u64)> {
        let index = match self.by_addr.binary_search_by_key(&addr, |(at, _)| *at) {
            Ok(exact) => exact,
            // Nothing at or below it, so it is before everything named.
            Err(0) => return None,
            Err(after) => after - 1,
        };
        let (at, name) = &self.by_addr[index];
        let end = match self.by_addr.get(index + 1) {
            Some((next, _)) => *next,
            None => at.saturating_add(LAST_SPAN),
        };
        (addr < end).then(|| (name.as_str(), addr - at))
    }

    /// Every name, sorted by address, for a frontend that wants to offer them.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u64)> {
        self.by_addr.iter().map(|(at, name)| (name.as_str(), *at))
    }
}

/// A machine, and everything wanted of it by something that is not the machine.
///
/// It owns the machine because all three callers do: the window moves it to a thread of
/// its own, and the control channel and a debugger have nothing else to hold. What
/// hands it back is the run returning, which is what makes every read here exact.
#[derive(Debug)]
pub struct Session {
    machine: Machine,
    /// Shared rather than owned, because they never change once an image is loaded and
    /// a frontend wants to name an address whether or not it can reach the machine.
    symbols: Arc<Symbols>,
    /// Addresses to stop before executing. A virtual address, since that is what a
    /// symbol is and what a person reading a disassembly has.
    breakpoints: BTreeSet<u64>,
    watchpoints: Vec<Watch>,
    /// Why the machine last stopped, so that something which did not ask can still find
    /// out. The thread running the machine is not always the one showing what happened
    /// to it.
    stop: Option<Stop>,
}

impl Session {
    /// A session over a machine, with no symbols for it.
    pub fn new(machine: Machine) -> Self {
        Self {
            machine,
            symbols: Arc::default(),
            breakpoints: BTreeSet::new(),
            watchpoints: Vec::new(),
            stop: None,
        }
    }

    /// And one that can name what it is looking at.
    pub fn with_symbols(machine: Machine, symbols: Symbols) -> Self {
        Self {
            symbols: Arc::new(symbols),
            ..Self::new(machine)
        }
    }

    /// The machine itself, for the few things that are not inspection: attaching a
    /// frontend's devices, and reading what the machine was built with.
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    pub fn machine_mut(&mut self) -> &mut Machine {
        &mut self.machine
    }

    /// The flag the harts read between quanta, which is what a frontend on another
    /// thread pauses and resumes the machine through.
    pub fn running(&self) -> Running {
        self.machine.running.clone()
    }

    /// The names the image gave, which a frontend may hold on to: they do not change,
    /// and naming an address is not a question about the machine's state.
    pub fn symbols(&self) -> &Arc<Symbols> {
        &self.symbols
    }

    /// Why it last stopped, which is not always known to whatever is asking: the thread
    /// running the machine and the thread showing what became of it are not the same.
    pub fn stop(&self) -> Option<&Stop> {
        self.stop.as_ref()
    }

    /// Remember a stop and hand it back, so that every way out of a run records itself.
    fn stopped(&mut self, stop: Stop) -> Stop {
        self.stop = Some(stop.clone());
        stop
    }

    pub fn harts(&self) -> usize {
        self.machine.harts.len()
    }

    // ---------------------------------------------------------------- running

    /// Carry on until something stops it: a trap nothing takes, a breakpoint, a
    /// watched address changing, or a pause.
    ///
    /// A machine sitting on a breakpoint carries on past it rather than stopping on it
    /// again, which is what makes a breakpoint in a loop steppable.
    pub fn resume(&mut self) -> Stop {
        if self.machine.running.halted() {
            let halt = self.last_halt();
            return self.stopped(Stop::Halted(halt));
        }
        self.machine.running.resume();
        let stop = match self.watched() {
            // Nothing to watch for, so the harts run at full speed and the schedule
            // the machine was built with is the one they run under.
            false => match self.machine.run() {
                Some(halt) => Stop::Halted(halt),
                None => Stop::Paused,
            },
            // Something to watch for, so every instruction has to be looked at. This is
            // slow by construction and only ever as slow as debugging: a machine with
            // nothing set on it never reaches here.
            true => self.run_watching(),
        };
        self.stopped(stop)
    }

    /// Run one hart for one instruction, whatever else the machine is doing.
    pub fn step(&mut self, hart: usize) -> Stop {
        self.step_many(hart, 1)
    }

    /// Run one hart for `count` instructions, stopping early on a trap nothing takes,
    /// on a trap that something does take, or on the hart parking.
    ///
    /// Entering a handler is where a step stops rather than something it steps through,
    /// which is what makes the next instruction the one a person is looking for. It
    /// retires nothing, so a run of steps that ends in a trap says it retired fewer than
    /// it was asked for.
    pub fn step_many(&mut self, hart: usize, count: u64) -> Stop {
        let taken = self.machine.harts[hart].traps.taken();
        let mut retired = 0;
        while retired < count {
            match self.machine.advance(hart, count - retired) {
                Err(trap) => {
                    self.machine.running.halt();
                    return Stop::Halted(Halt { hart, trap });
                }
                Ok(ran) => {
                    retired += ran;
                    // Nothing retired is a hart that parked on a `wfi` or one that
                    // entered a handler. Neither is a step to simply repeat: the first
                    // waits on a device and the second has arrived somewhere.
                    if ran == 0 {
                        break;
                    }
                }
            }
            if self.machine.harts[hart].traps.taken() != taken {
                break;
            }
        }
        self.stopped(Stop::Stepped { hart, retired })
    }

    /// Run one hart until it enters a trap handler, or until `limit` instructions have
    /// retired without it doing so.
    ///
    /// The trap it stops on is the one that was taken, which is not always the one that
    /// was raised: a trap with no handler installed ends the run instead, and that is a
    /// halt rather than a trap to look at.
    pub fn step_to_trap(&mut self, hart: usize, limit: u64) -> Stop {
        let before = self.machine.harts[hart].traps.taken();
        let mut retired = 0;
        while retired < limit {
            let ran = match self.step_many(hart, 1) {
                Stop::Stepped { retired: ran, .. } => ran,
                halted => return halted,
            };
            // Before deciding that a step which retired nothing was a hart parking:
            // entering a handler retires nothing either, and it is the thing being
            // waited for.
            if self.machine.harts[hart].traps.taken() > before {
                let taken = *self.machine.harts[hart]
                    .traps
                    .last()
                    .expect("a trap was just counted");
                return self.stopped(Stop::Trapped { hart, taken });
            }
            if ran == 0 {
                break;
            }
            retired += ran;
        }
        self.stopped(Stop::Stepped { hart, retired })
    }

    /// Ask the machine to stop where it is. It stops at the end of the quantum each
    /// hart is in, which is what the run returning says.
    pub fn pause(&self) {
        self.machine.running.pause();
    }

    /// And to stop for good, which nothing carries on from.
    pub fn halt(&self) {
        self.machine.running.halt();
    }

    pub fn state(&self) -> State {
        self.machine.running.state()
    }

    /// Every instruction, on every hart, until something is hit. The price of having
    /// anything to watch for.
    fn run_watching(&mut self) -> Stop {
        // A breakpoint is checked before the instruction at it runs, which is what
        // makes it a breakpoint rather than a report that one was passed. The one it
        // may not stop on is the address a hart is already sitting at, since that is
        // where the last stop left it: without that, a breakpoint inside a loop is one
        // nothing can ever step out of. It is per hart, because the hart sitting on a
        // breakpoint is not the others.
        let mut moved = vec![false; self.machine.harts.len()];
        while self.machine.running.going() {
            let mut ran = false;
            for (hart, moved) in moved.iter_mut().enumerate() {
                let pc = self.machine.harts[hart].pc;
                if *moved && self.breakpoints.contains(&pc) {
                    self.machine.running.pause();
                    return Stop::Breakpoint { hart, pc };
                }
                let before = self.watch_values(hart);
                match self.step_many(hart, 1) {
                    Stop::Stepped { retired: 0, .. } => {}
                    Stop::Stepped { .. } => ran = true,
                    halted => return halted,
                }
                *moved = true;
                if let Some(stop) = self.changed(hart, &before) {
                    return stop;
                }
            }
            if !ran {
                std::hint::spin_loop();
            }
        }
        Stop::Paused
    }

    /// The halt a halted machine stopped on, which is the last trap any hart took with
    /// nothing installed to take it. A machine halted by a window closing has no trap
    /// at all, and says so as the illegal instruction it never executed would.
    fn last_halt(&self) -> Halt {
        let hart = 0;
        Halt {
            hart,
            trap: self.machine.harts[hart]
                .traps
                .last()
                .map(|taken| taken.trap)
                .unwrap_or(Trap::Exception(Exception::Breakpoint(
                    self.machine.harts[hart].pc,
                ))),
        }
    }

    // ------------------------------------------------------------ breakpoints

    fn watched(&self) -> bool {
        !self.breakpoints.is_empty() || !self.watchpoints.is_empty()
    }

    /// Stop before executing the instruction at `pc`, wherever any hart reaches it.
    pub fn set_breakpoint(&mut self, pc: u64) {
        self.breakpoints.insert(pc);
    }

    /// The same, at whatever address a symbol names. Answers where it went, or nothing
    /// if the image never named that.
    pub fn break_at(&mut self, name: &str) -> Option<u64> {
        let pc = self.symbols.address(name)?;
        self.set_breakpoint(pc);
        Some(pc)
    }

    pub fn clear_breakpoint(&mut self, pc: u64) -> bool {
        self.breakpoints.remove(&pc)
    }

    pub fn breakpoints(&self) -> impl Iterator<Item = u64> {
        self.breakpoints.iter().copied()
    }

    /// Stop when `size` bytes at `at` are found holding something else. It is a
    /// comparison made between instructions rather than a hook in the store path,
    /// because a hook there would cost every store on a machine that has no watchpoint
    /// set. A write of the value that was already there is not a change and does not
    /// stop anything, which is the one thing this cannot see.
    pub fn set_watchpoint(&mut self, at: Address, size: usize) {
        assert!(
            matches!(size, 1 | 2 | 4 | 8),
            "a watched value is one, two, four or eight bytes"
        );
        let was = self.read_word(0, at, size);
        self.watchpoints.push(Watch { at, size, was });
    }

    pub fn clear_watchpoint(&mut self, at: Address) -> bool {
        let before = self.watchpoints.len();
        self.watchpoints.retain(|watch| watch.at != at);
        self.watchpoints.len() != before
    }

    pub fn watchpoints(&self) -> impl Iterator<Item = (Address, usize)> {
        self.watchpoints.iter().map(|watch| (watch.at, watch.size))
    }

    /// What every watched address holds as this hart sees it.
    fn watch_values(&mut self, hart: usize) -> Vec<Option<u64>> {
        (0..self.watchpoints.len())
            .map(|index| {
                let watch = self.watchpoints[index];
                self.read_word(hart, watch.at, watch.size)
            })
            .collect()
    }

    /// Whether any of them moved, and the first that did.
    fn changed(&mut self, hart: usize, before: &[Option<u64>]) -> Option<Stop> {
        let now = self.watch_values(hart);
        for (index, (was, is)) in before.iter().zip(&now).enumerate() {
            if was != is {
                self.watchpoints[index].was = *is;
                self.machine.running.pause();
                return Some(Stop::Watchpoint {
                    hart,
                    at: self.watchpoints[index].at,
                    was: was.unwrap_or(0),
                    now: is.unwrap_or(0),
                });
            }
        }
        None
    }

    // ---------------------------------------------------------------- reading

    /// A hart's registers.
    pub fn registers(&self, hart: usize) -> Registers {
        let cpu = &self.machine.harts[hart];
        Registers {
            hart,
            pc: cpu.pc,
            mode: cpu.mode,
            x: cpu.regs,
            f: cpu.fregs,
            waiting: cpu.waiting,
            instret: cpu.csrs[csr::MINSTRET],
        }
    }

    /// What a control register holds.
    ///
    /// Out of storage rather than through the path an instruction takes, because that
    /// path has side effects: reading an interrupt file's top register claims the
    /// interrupt it names. The three registers that are views of another are resolved,
    /// since their storage holds nothing.
    pub fn csr(&self, hart: usize, addr: usize) -> u64 {
        let cpu = &self.machine.harts[hart];
        match csr::alias(addr, cpu.csrs[csr::MIDELEG]) {
            Some((backing, read, _)) => cpu.csrs[backing] & read,
            None => cpu.csrs[addr],
        }
    }

    /// The control registers worth showing, named. Every one this machine implements
    /// and gives a name to, in the order they are listed rather than by number, since
    /// what a person wants next to `mstatus` is `mepc` and not `misa`.
    pub fn csrs(&self, hart: usize) -> Vec<Csr> {
        NAMED
            .iter()
            .map(|(addr, name)| Csr {
                addr: *addr,
                name,
                value: self.csr(hart, *addr),
            })
            .collect()
    }

    /// Read `len` bytes, saying for each whether anything answered.
    ///
    /// Memory and nothing else: an address in a device's window reads as missing rather
    /// than being read, since reading a device is an action. A virtual address is
    /// translated as `hart` currently translates one, so a page it cannot reach reads
    /// as missing too.
    pub fn memory(&mut self, hart: usize, at: Address, len: usize) -> Window {
        let bytes = (0..len as u64)
            .map(|offset| {
                let addr = match at {
                    Address::Physical(base) => Some(base.wrapping_add(offset)),
                    Address::Virtual(base) => self.translate(hart, base.wrapping_add(offset)),
                }?;
                self.byte(addr)
            })
            .collect();
        Window { at, bytes }
    }

    /// One byte of memory, or nothing if memory is not what is there.
    fn byte(&self, addr: u64) -> Option<u8> {
        self.machine
            .bus
            .in_dram(addr, u8::BITS as u64)
            .then(|| self.machine.bus.dram.load(addr, u8::BITS as u64) as u8)
    }

    /// A word of memory as one number, for the places a size is known.
    fn read_word(&mut self, hart: usize, at: Address, size: usize) -> Option<u64> {
        self.memory(hart, at, size).word(0, size)
    }

    /// What a hart's page tables say a virtual address is, without disturbing anything
    /// a guest can see. A page it cannot read is no address at all.
    ///
    /// This fills the hart's translation cache, which is a cache of tables that have
    /// not moved, so it changes nothing the guest can observe. It never sets an
    /// accessed or dirty bit: this machine leaves both to software, so a page whose
    /// accessed bit is clear is one an inspector reports as unreachable rather than one
    /// it quietly marks as read.
    pub fn translate(&mut self, hart: usize, va: u64) -> Option<u64> {
        let (cpu, bus) = (&mut self.machine.harts[hart], &self.machine.bus);
        cpu.translate(bus, va, Access::Load).ok()
    }

    /// Disassemble `count` instructions from `at`, following their lengths so the
    /// stream stays aligned.
    ///
    /// A run of instructions has no way to be read backwards on an instruction set
    /// whose instructions differ in length, so this only goes forwards. What gives a
    /// caller the instructions *before* a `pc` is the symbol it is in: start at the
    /// symbol and walk to it.
    pub fn disassemble(&mut self, hart: usize, at: Address, count: usize) -> Vec<Line> {
        let mut lines = Vec::with_capacity(count);
        let mut addr = at.addr();
        for _ in 0..count {
            let here = match at {
                Address::Physical(_) => Address::Physical(addr),
                Address::Virtual(_) => Address::Virtual(addr),
            };
            let window = self.memory(hart, here, 4);
            // Two bytes decide the length, and a compressed instruction is all there is
            // to read; four are only needed once it is known that four are there.
            let Some(half) = window.word(0, 2) else {
                break;
            };
            let length = inst::length(half as u16) as u8;
            let encoding = match length {
                2 => half as u32,
                _ => match window.word(0, 4) {
                    Some(word) => word as u32,
                    None => break,
                },
            };
            let inst = match length {
                2 => rvc::decode(encoding as u16).ok(),
                _ => decode(encoding).ok(),
            };
            lines.push(Line {
                at: addr,
                encoding,
                length,
                inst,
                target: inst.and_then(|inst| target(inst, addr)),
                symbol: self
                    .symbols
                    .nearest(addr)
                    .map(|(name, offset)| (name.to_owned(), offset)),
            });
            addr = addr.wrapping_add(u64::from(length));
        }
        lines
    }

    /// The traps a hart has taken lately, newest first.
    pub fn traps(&self, hart: usize) -> Vec<Taken> {
        self.machine.harts[hart].traps.recent().copied().collect()
    }

    /// How many it has ever taken, which is what says how many the log dropped.
    pub fn traps_taken(&self, hart: usize) -> u64 {
        self.machine.harts[hart].traps.taken()
    }

    /// What every device on the bus is doing, in the order they sit at.
    pub fn devices(&self) -> Vec<Report> {
        self.machine.bus.describe()
    }

    /// Which interrupts are pending on a hart and which of them it has enabled, which
    /// together are why it is or is not in a handler.
    pub fn interrupts(&self, hart: usize) -> (u64, u64) {
        (
            self.machine.bus.interrupts(hart) | self.machine.harts[hart].csrs[csr::MIP],
            self.machine.harts[hart].csrs[csr::MIE],
        )
    }
}

/// Where an instruction goes, for the ones whose target is knowable without executing
/// them. A `jalr` computes its target out of a register and so has none until it runs,
/// and neither does a trap.
fn target(inst: Inst, at: u64) -> Option<u64> {
    match inst.op {
        Op::Jal | Op::Branch { .. } => Some(at.wrapping_add(inst.imm)),
        _ => None,
    }
}

/// The control registers this machine implements and a person reads, in the order they
/// are worth reading in: what a hart is doing, then what it would do about a trap, then
/// the counters and the rest.
///
/// It is a list rather than every non-zero register because a register reading zero is
/// often the answer being looked for, and because a `[u64; 4096]` has four thousand
/// numbers in it that name nothing.
const NAMED: &[(usize, &str)] = &[
    (csr::MSTATUS, "mstatus"),
    (csr::MISA, "misa"),
    (csr::MHARTID, "mhartid"),
    (csr::MEDELEG, "medeleg"),
    (csr::MIDELEG, "mideleg"),
    (csr::MIE, "mie"),
    (csr::MIP, "mip"),
    (csr::MTVEC, "mtvec"),
    (csr::MEPC, "mepc"),
    (csr::MCAUSE, "mcause"),
    (csr::MTVAL, "mtval"),
    (csr::MSCRATCH, "mscratch"),
    (csr::MCOUNTEREN, "mcounteren"),
    (csr::MENVCFG, "menvcfg"),
    (csr::SSTATUS, "sstatus"),
    (csr::SIE, "sie"),
    (csr::SIP, "sip"),
    (csr::STVEC, "stvec"),
    (csr::SEPC, "sepc"),
    (csr::SCAUSE, "scause"),
    (csr::STVAL, "stval"),
    (csr::SSCRATCH, "sscratch"),
    (csr::SATP, "satp"),
    (csr::SCOUNTEREN, "scounteren"),
    (csr::SENVCFG, "senvcfg"),
    (csr::MCYCLE, "mcycle"),
    (csr::MINSTRET, "minstret"),
    (csr::TIME, "time"),
    (csr::FCSR, "fcsr"),
];
