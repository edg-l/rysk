use std::{env, fs::File, io::Read};

use rysk::{
    bochs,
    debug::{Session, Stop, Symbols},
    dram::DRAM_SIZE,
    elf, htif,
    inst::REG_NAMES,
    machine,
    machine::{Aia, Halt, Machine, Schedule, Storage, Usb, Video},
};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// Long enough for any test in the corpus, short enough to notice a program that will
/// never finish.
const MAX_STEPS: u64 = 100_000_000;

/// What this takes, for someone who asked or who gave it nothing to run.
const USAGE: &str = "\
rysk: a RISC-V emulator. Runs a flat binary at DRAM_BASE, or an ELF at its entry.

    rysk [options] <image> [image@address ...]

    -m <mebibytes>              how much memory the machine has
    -smp <harts>                how many harts it has
    --schedule turns|threads    how they take their turns
    --initrd <file>             a ramdisk, placed as high as it fits
    --append <args>             the kernel command line
    --aia none|aplic|aplic-imsic    which interrupt architecture
    --display none|bochs        a framebuffer and the monitor it answers for
    --usb none|hid              an xHCI controller, a keyboard and a mouse
    --disk none|<file>|<n>M     an NVMe controller with that behind it
    --symbols <file>            names for addresses, as an ELF or a System.map
    --gui                       open a window on the display

An image after the first says where it goes, which is how firmware and the kernel
it hands off to are both in memory at once:

    rysk -m 1024 --initrd initrd.gz --append \"console=ttyS0\" \
         fw_dynamic.bin vmlinux@0x80200000";

/// What a boot rom would leave for firmware that asks: where to go next, and in which
/// mode. rysk is the stage before OpenSBI, so it is the one that has to say.
///
/// A firmware built to jump to a fixed address ignores this; one built to be told
/// reads it out of `a2`. Saying so here is what keeps the device tree where this
/// machine put it, rather than being relocated into the middle of a kernel that
/// firmware does not know is there.
fn handoff(machine: &mut Machine) {
    /// "OSBI", and the layout version that carries a boot hart.
    const MAGIC: u64 = 0x4942_534f;
    const VERSION: u64 = 2;
    /// Supervisor mode, which is where a kernel expects to be started.
    const NEXT_MODE: u64 = 1;
    /// Where OpenSBI's own next stage begins, by its convention.
    const NEXT: u64 = 0x8020_0000;

    let at = machine::fdt_base(machine.bus.dram.size()) - 0x1000;
    for (n, word) in [MAGIC, VERSION, NEXT, NEXT_MODE, 0, 0]
        .into_iter()
        .enumerate()
    {
        machine.bus.dram.store(at + n as u64 * 8, 64, word);
    }
    for hart in &mut machine.harts {
        hart.regs[12] = at;
    }
}

/// How a run ended, for the line printed after it. A run that was asked to stop
/// reached no trap, so there is nothing to name but the asking.
fn describe(session: &Session, stop: &Stop) -> String {
    match stop {
        Stop::Halted(halt) => {
            let pc = session.registers(halt.hart).pc;
            format!("{halt}, pc {}", place(session, pc))
        }
        _ => "asked to stop".to_owned(),
    }
}

/// Names for addresses, out of an ELF's symbol table or a `System.map`.
///
/// Both, because the image a machine actually boots is usually neither. A Linux kernel
/// is a raw `Image` with no symbol table in it at all, and the names for it ship beside
/// it in a `System.map`; a firmware or a test binary is an ELF and carries its own.
fn symbols_from(path: &str) -> Result<Vec<(String, u64)>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if elf::is_elf(&bytes) {
        return Ok(elf::parse(&bytes)?.symbols.into_iter().collect());
    }
    // `System.map` is one symbol a line: an address in hex, the one-letter kind `nm`
    // gives it, and the name. Anything that is not that shape is not a symbol.
    let text = String::from_utf8(bytes)?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let addr = u64::from_str_radix(parts.next()?, 16).ok()?;
            let _kind = parts.next()?;
            Some((parts.next()?.to_owned(), addr))
        })
        .collect())
}

/// An address, and what it is called if the image said. A kernel that stopped
/// somewhere is far easier to place by the name of the function it stopped in than by
/// the number.
fn place(session: &Session, addr: u64) -> String {
    match session.symbols().nearest(addr) {
        Some((name, 0)) => format!("{addr:#x} <{name}>"),
        Some((name, offset)) => format!("{addr:#x} <{name}+{offset:#x}>"),
        None => format!("{addr:#x}"),
    }
}

/// Everything a hart was left holding, which is what a run prints after it.
///
/// Read through the inspection surface rather than out of the hart, so that what is
/// printed here and what a panel or a control channel would show are the same values
/// answered by the same call.
fn dump(session: &Session, hart: usize) {
    let regs = session.registers(hart);
    println!(
        "hart {hart}: pc {}, {:?} mode, {} retired{}",
        place(session, regs.pc),
        regs.mode,
        regs.instret,
        match regs.waiting {
            true => ", parked",
            false => "",
        }
    );
    for (which, value) in regs.x.iter().enumerate() {
        print!("x{which:02} ({:>4}) = {value:>#18x} | ", REG_NAMES[which]);
        if (which + 1) % 4 == 0 {
            println!();
        }
    }
    // The registers a person reads, named, and only the ones holding something: a
    // machine that never entered supervisor mode has nothing to say about `stvec`.
    let live: Vec<_> = session
        .csrs(hart)
        .into_iter()
        .filter(|csr| csr.value != 0)
        .collect();
    for (index, csr) in live.iter().enumerate() {
        print!("{:>10} = {:>#18x} | ", csr.name, csr.value);
        if (index + 1) % 4 == 0 {
            println!();
        }
    }
    if !live.len().is_multiple_of(4) {
        println!();
    }
    if let Some(taken) = session.traps(hart).first() {
        println!("last trap: {taken}");
    }
}

/// Run the machine behind a window, which is a thing only a build that was asked for
/// one can do.
#[cfg(feature = "gui")]
fn ends(frontend: &machine::Frontend) -> rysk::gui::Ends {
    rysk::gui::Ends {
        screen: frontend.screen.clone(),
        keys: frontend.keys.clone(),
        pointer: frontend.pointer.clone(),
    }
}

#[cfg(feature = "gui")]
fn window(
    session: Session,
    ends: rysk::gui::Ends,
) -> Result<(Session, Option<Halt>), Box<dyn std::error::Error>> {
    Ok(rysk::gui::run(session, ends)?)
}

#[cfg(not(feature = "gui"))]
fn ends(_frontend: &machine::Frontend) {}

#[cfg(not(feature = "gui"))]
fn window(
    _session: Session,
    _ends: (),
) -> Result<(Session, Option<Halt>), Box<dyn std::error::Error>> {
    Err(
        "--gui: this rysk was built without a window, which is what \
         `--no-default-features` leaves out"
            .into(),
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_max_level(Level::DEBUG)
            .with_env_filter(EnvFilter::from_default_env())
            .pretty()
            .finish(),
    )
    .unwrap();

    // The first image is what the machine starts on, at the bottom of dram. Any after
    // it are placed where they say, which is how firmware and the kernel it hands off
    // to are both in memory at once: `rysk fw_jump.bin vmlinux@0x80200000`.
    let args: Vec<String> = env::args().skip(1).collect();
    let mut memory = DRAM_SIZE;
    let mut harts = 1;
    let mut aia = Aia::default();
    let mut video = Video::default();
    let mut usb = Usb::default();
    let mut storage = Storage::default();
    let mut schedule = None;
    let mut options = machine::Boot::default();
    let mut ramdisk = None;
    let mut named: Option<String> = None;
    let mut gui = false;
    let mut at = 0;
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    while at < args.len() {
        // The only option that is not a pair. Everything else names a thing the
        // machine is built with; this one names who is driving it.
        if args[at] == "--gui" {
            gui = true;
            at += 1;
            continue;
        }
        if at + 1 >= args.len() {
            break;
        }
        let value = args[at + 1].clone();
        match args[at].as_str() {
            "-m" => memory = value.parse::<u64>().expect("a size in mebibytes") * 1024 * 1024,
            "-smp" => harts = value.parse::<usize>().expect("a number of harts"),
            "--initrd" => ramdisk = Some(value),
            "--symbols" => named = Some(value.clone()),
            "--append" => options.bootargs = Some(value),
            "--aia" => aia = value.parse().unwrap_or_else(|why| panic!("--aia {why}")),
            "--display" => {
                video = value
                    .parse()
                    .unwrap_or_else(|why| panic!("--display {why}"))
            }
            "--usb" => usb = value.parse().unwrap_or_else(|why| panic!("--usb {why}")),
            "--disk" => storage = value.parse().unwrap_or_else(|why| panic!("--disk {why}")),
            "--schedule" => {
                schedule = Some(
                    value
                        .parse()
                        .unwrap_or_else(|why| panic!("--schedule {why}")),
                )
            }
            _ => break,
        }
        at += 2;
    }
    let images = &args[at..];
    let Some(first) = images.first() else {
        // Asking for the usage is not a failure, and neither is running it with
        // nothing to run: both are told what to say and neither is a panic.
        println!("{USAGE}");
        return Ok(());
    };
    let mut code = Vec::new();
    File::open(first)?.read_to_end(&mut code)?;

    // An image that carries a `tohost` symbol is a test: it signals its result there
    // and then spins, so watching that address is the only way the run ends.
    let (mut machine, tohost, mut symbols) = if elf::is_elf(&code) {
        let image = elf::parse(&code)?;
        (
            Machine::from_elf(&image, memory, harts)?,
            htif::tohost(&image),
            {
                // Sorted, because an image's table is a map and two names for one
                // address would otherwise resolve to whichever it happened to yield.
                let mut symbols: Vec<_> = image.symbols.clone().into_iter().collect();
                symbols.sort();
                symbols
            },
        )
    } else {
        (Machine::new(code, memory, harts), None, Vec::new())
    };

    // Appended after the image's, and the last name given for an address is the one it
    // resolves to, so a file asked for by hand beats an image that was not. The usual
    // case is that the image has no names at all: a kernel is a raw `Image`, and what
    // names it ships beside it.
    if let Some(path) = &named {
        symbols.extend(symbols_from(path)?);
    }
    let symbols = Symbols::new(symbols);

    for arg in &images[1..] {
        let (path, at) = arg
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("{arg}: an image after the first needs an @address"));
        let at = u64::from_str_radix(at.trim_start_matches("0x"), 16)?;
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        let end = at + bytes.len() as u64;
        assert!(
            machine.bus.dram.write(at, &bytes, 0),
            "{path} does not fit: it wants up to {end:#x}"
        );
        println!("loaded {path} at {at:#x}, {} KiB", bytes.len() / 1024);
    }

    // The ramdisk goes as high as it fits, out of the way of a kernel that was loaded
    // at the bottom and of the device tree that says where this is.
    if let Some(path) = ramdisk {
        let mut bytes = Vec::new();
        File::open(&path)?.read_to_end(&mut bytes)?;
        let end = machine::fdt_base(memory) - 0x10_0000;
        let at = (end - bytes.len() as u64) & !0xfff;
        assert!(
            machine.bus.dram.write(at, &bytes, 0),
            "{path} does not fit in memory"
        );
        println!("loaded {path} at {at:#x}, {} KiB", bytes.len() / 1024);
        options.initrd = Some((at, at + bytes.len() as u64));
    }

    let frontend = machine::boot(
        &mut machine,
        &format!("{}{}", rysk::ISA, aia.isa()),
        &options,
        aia,
        video,
        usb,
        &storage,
    );
    handoff(&mut machine);

    // Whatever is typed reaches the port from its own thread, since the hart is busy
    // being a hart. The terminal is still in its usual line-buffered mode, so a line
    // arrives when it is finished rather than a key at a time: making it raw is the
    // frontend's job and the frontend is not written yet.
    // The window's ends are taken first, since the serial port's end is moved out of
    // the frontend just below and moving one field out ends the whole of it.
    let ends = ends(&frontend);
    let keyboard = frontend.keyboard;
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while std::io::stdin().read_exact(&mut byte).is_ok() {
            keyboard.typed(&byte);
        }
    });

    // A hart each on a host thread once there is more than one hart, which is the whole
    // of what threads are worth: one hart has nothing to run alongside. Keeping a single
    // hart taking turns also keeps the benchmarks the one shape, since they all run
    // `-smp 1` and would otherwise be timing a poller thread as well.
    machine.schedule = schedule.unwrap_or(match machine.harts.len() {
        1 => Schedule::RoundRobin,
        _ => Schedule::Threads,
    });

    // The machine goes into a session for the rest of its life. Nothing here sets a
    // breakpoint, so a run is the run it always was; what the session adds is that
    // what is printed afterwards is read through the same surface a window's panels
    // and a control channel read, rather than out of the harts directly.
    let mut session = Session::with_symbols(machine, symbols);

    let stopped = match (tohost, gui) {
        (Some(tohost), _) => htif::run(session.machine_mut(), tohost, MAX_STEPS).to_string(),
        (None, true) => {
            // The window takes the machine for as long as it is open, since the harts
            // run on a thread of their own behind it, and hands it back afterwards.
            let (returned, halt) = window(session, ends)?;
            session = returned;
            describe(&session, &halt.map_or(Stop::Paused, Stop::Halted))
        }
        (None, false) => {
            let stop = session.resume();
            describe(&session, &stop)
        }
    };

    for hart in 0..session.harts() {
        dump(&session, hart);
    }
    // What the display was showing when it stopped. Presenting it is the frontend's
    // job and the frontend is not written yet, so this says what there was to present:
    // the mode the guest asked for, and how much of video memory it had drawn into.
    if let Some(screen) = &frontend.screen {
        match screen.mode() {
            Some(mode) => {
                let drawn = screen.vram().take_dirty().pages().count() as u64 * bochs::PAGE;
                println!(
                    "display: {}x{} in {:?}, {} KiB of a {} KiB picture drawn",
                    mode.width,
                    mode.height,
                    mode.format,
                    drawn / 1024,
                    mode.size / 1024,
                );
            }
            None => println!("display: showing nothing"),
        }
    }
    println!("stopped: {stopped}");

    Ok(())
}
