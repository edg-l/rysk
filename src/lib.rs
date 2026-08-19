/// Log an instruction as it executes, at debug level.
macro_rules! trace_insn {
    ($($arg:tt)*) => {{
        #[cfg(feature = "trace")]
        ::tracing::debug!($($arg)*);
    }};
}

/// Log a bus access, at trace level.
macro_rules! trace_mem {
    ($($arg:tt)*) => {{
        #[cfg(feature = "trace")]
        ::tracing::trace!($($arg)*);
    }};
}

pub mod bus;
pub mod clint;
pub mod cpu;
pub mod csr;
pub mod device;
pub mod dram;
pub mod elf;
pub mod htif;
pub mod inst;
pub mod machine;
pub mod mmu;
pub mod plic;
pub mod rvc;
pub mod trap;
pub mod uart;
