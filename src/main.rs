use std::{env, fs::File, io::Read};

use rysk::{dram::DRAM_SIZE, elf, htif, machine, machine::Machine};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// Long enough for any test in the corpus, short enough to notice a program that will
/// never finish.
const MAX_STEPS: u64 = 100_000_000;

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
    let harts = 1;
    let mut options = machine::Boot::default();
    let mut ramdisk = None;
    let mut at = 0;
    while at + 1 < args.len() {
        let value = args[at + 1].clone();
        match args[at].as_str() {
            "-m" => memory = value.parse::<u64>().expect("a size in mebibytes") * 1024 * 1024,
            "--initrd" => ramdisk = Some(value),
            "--append" => options.bootargs = Some(value),
            _ => break,
        }
        at += 2;
    }
    let images = &args[at..];
    let Some(first) = images.first() else {
        panic!(
            "Usage: rysk [-m <mebibytes>] [--initrd <file>] [--append <args>] \
             <image> [image@address ...]"
        );
    };
    let mut code = Vec::new();
    File::open(first)?.read_to_end(&mut code)?;

    // An image that carries a `tohost` symbol is a test: it signals its result there
    // and then spins, so watching that address is the only way the run ends.
    let (mut machine, tohost) = if elf::is_elf(&code) {
        let image = elf::parse(&code)?;
        (
            Machine::from_elf(&image, memory, harts)?,
            htif::tohost(&image),
        )
    } else {
        (Machine::new(code, memory, harts), None)
    };

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

    let keyboard = machine::boot(&mut machine, rysk::ISA, &options);
    handoff(&mut machine);

    // Whatever is typed reaches the port from its own thread, since the hart is busy
    // being a hart. The terminal is still in its usual line-buffered mode, so a line
    // arrives when it is finished rather than a key at a time: making it raw is the
    // frontend's job and the frontend is not written yet.
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        while std::io::stdin().read_exact(&mut byte).is_ok() {
            keyboard.typed(&byte);
        }
    });

    let stopped = match tohost {
        Some(tohost) => htif::run(&mut machine, tohost, MAX_STEPS).to_string(),
        None => {
            let halt = machine.run();
            format!("{halt}, pc {:#x}", machine.harts[halt.hart].pc)
        }
    };

    for hart in &machine.harts {
        hart.dump_registers();
        hart.dump_csr();
    }
    println!("stopped: {stopped}");

    Ok(())
}
