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

/// What this machine implements, in the spelling a device tree wants. It is here
/// rather than in `csr` because `misa` has a bit per letter and no room for the rest.
pub const ISA: &str =
    "rv64imafdc_zicsr_zifencei_zicntr_zicond_zaamo_zalrsc_zacas_zabha_zawrs_svade";

pub mod aplic;
pub mod block;
pub mod bochs;
pub mod bus;
pub mod clint;
pub mod cpu;
pub mod csr;
pub mod device;
pub mod disk;
pub mod dram;
pub mod edid;
pub mod elf;
pub mod fdt;
pub mod fpu;
pub mod hid;
pub mod htif;
pub mod imsic;
pub mod inst;
pub mod machine;
pub mod mmu;
pub mod nvme;
pub mod pci;
pub mod plic;
pub mod rvc;
pub mod shared;
pub mod trap;
pub mod uart;
pub mod usb;
pub mod xhci;
