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
pub mod cpu;
pub mod csr;
pub mod dram;
pub mod elf;
pub mod exception;
pub mod htif;
pub mod inst;
