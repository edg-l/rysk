use std::{env, fs::File, io::Read};

use rysk::{cpu::Cpu, elf, htif};
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

/// Long enough for any test in the corpus, short enough to notice a program that will
/// never finish.
const MAX_STEPS: u64 = 100_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_max_level(Level::DEBUG)
            .with_env_filter(EnvFilter::from_default_env())
            .pretty()
            .finish(),
    )
    .unwrap();

    let args: Vec<String> = env::args().collect();

    if args.len() != 2 {
        panic!("Usage: rysk <filename>");
    }
    let mut file = File::open(&args[1])?;
    let mut code = Vec::new();
    file.read_to_end(&mut code)?;

    // An image that carries a `tohost` symbol is a test: it signals its result there
    // and then spins, so watching that address is the only way the run ends.
    let (mut cpu, tohost) = if elf::is_elf(&code) {
        let image = elf::parse(&code)?;
        (Cpu::from_elf(&image)?, htif::tohost(&image))
    } else {
        (Cpu::new(code), None)
    };

    let stopped = match tohost {
        Some(tohost) => htif::run(&mut cpu, tohost, MAX_STEPS).to_string(),
        None => {
            let exception = cpu.run();
            format!("{exception}, pc {:#x}", cpu.pc)
        }
    };

    cpu.dump_registers();
    cpu.dump_csr();
    println!("stopped: {stopped}");

    Ok(())
}
