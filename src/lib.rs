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

/// Record a decoded field on the current span.
macro_rules! trace_field {
    ($name:literal, $value:expr) => {{
        #[cfg(feature = "trace")]
        ::tracing::Span::current().record($name, $value);
    }};
}

pub mod bus;
pub mod cpu;
pub mod dram;
pub mod exception;
