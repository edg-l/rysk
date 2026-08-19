use std::{env, fs::File, io::Read};

use rysk::{cpu::Cpu, dram::DRAM_SIZE, elf, htif, machine};
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
fn handoff(cpu: &mut Cpu) {
    /// "OSBI", and the layout version that carries a boot hart.
    const MAGIC: u64 = 0x4942_534f;
    const VERSION: u64 = 2;
    /// Supervisor mode, which is where a kernel expects to be started.
    const NEXT_MODE: u64 = 1;
    /// Where OpenSBI's own next stage begins, by its convention.
    const NEXT: u64 = 0x8020_0000;

    let at = machine::fdt_base(cpu.bus.dram.size()) - 0x1000;
    for (n, word) in [MAGIC, VERSION, NEXT, NEXT_MODE, 0, 0]
        .into_iter()
        .enumerate()
    {
        cpu.bus.dram.store(at + n as u64 * 8, 64, word);
    }
    cpu.regs[12] = at;
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
    let mut args = env::args().skip(1).peekable();
    let mut memory = DRAM_SIZE;
    if args.peek().is_some_and(|arg| arg == "-m") {
        args.next();
        let mib: u64 = args
            .next()
            .and_then(|arg| arg.parse().ok())
            .expect("-m takes a size in mebibytes");
        memory = mib * 1024 * 1024;
    }
    let Some(first) = args.next() else {
        panic!("Usage: rysk [-m <mebibytes>] <image> [image@address ...]");
    };
    let mut code = Vec::new();
    File::open(&first)?.read_to_end(&mut code)?;

    // An image that carries a `tohost` symbol is a test: it signals its result there
    // and then spins, so watching that address is the only way the run ends.
    let (mut cpu, tohost) = if elf::is_elf(&code) {
        let image = elf::parse(&code)?;
        (Cpu::from_elf(&image)?, htif::tohost(&image))
    } else {
        (Cpu::with_memory(code, memory), None)
    };

    for arg in args {
        let (path, at) = arg
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("{arg}: an image after the first needs an @address"));
        let at = u64::from_str_radix(at.trim_start_matches("0x"), 16)?;
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        let end = at + bytes.len() as u64;
        assert!(
            cpu.bus.dram.write(at, &bytes, 0),
            "{path} does not fit: it wants up to {end:#x}"
        );
        println!("loaded {path} at {at:#x}, {} KiB", bytes.len() / 1024);
    }

    machine::boot(&mut cpu, rysk::ISA);
    handoff(&mut cpu);

    let stopped = match tohost {
        Some(tohost) => htif::run(&mut cpu, tohost, MAX_STEPS).to_string(),
        None => {
            let trap = cpu.run();
            format!("{trap}, pc {:#x}", cpu.pc)
        }
    };

    cpu.dump_registers();
    cpu.dump_csr();
    println!("stopped: {stopped}");

    Ok(())
}
