use std::{env, fs::File, io::Read};

use cpu::Cpu;
use tracing::Level;
use tracing_subscriber::{EnvFilter, FmtSubscriber};

mod bus;
mod cpu;
mod dram;

fn main() -> Result<(), std::io::Error> {
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_env_filter(EnvFilter::from_default_env())
            .with_max_level(Level::DEBUG)
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

    let mut cpu = Cpu::new(code);
    cpu.run()?;
    cpu.dump_registers();
    cpu.dump_csr();

    Ok(())
}
