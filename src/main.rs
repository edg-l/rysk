use std::{env, fs::File, io::Read};

use cpu::Cpu;

mod bus;
mod cpu;
mod dram;

fn main() -> Result<(), std::io::Error> {
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

    Ok(())
}
