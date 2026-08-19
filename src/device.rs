//! What the bus talks to, other than dram.
//!
//! A device is a model behind this trait and nothing else: it is told what was read or
//! written at an offset from wherever it was placed, and it says whether it is
//! asserting an interrupt. It never learns its own address, which controller its line
//! runs to, or what else exists.
//!
//! Interrupting comes in two shapes here, and neither of them names a controller: a
//! `Line` a device holds up, and an `Msi` it posts.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    csr::{MEIP, SEIP},
    trap::Exception,
};

/// The wire between a device and an interrupt controller. The device drives it and the
/// controller reads it, and neither knows anything else about the other: which line a
/// device got, and which controller it runs to, are the machine's to decide.
///
/// It is shared rather than polled so that swapping the controller a line runs to is a
/// change to how the machine is built and to nothing else.
#[derive(Debug, Clone, Default)]
pub struct Line(Arc<AtomicBool>);

impl Line {
    pub fn set(&self, raised: bool) {
        self.0.store(raised, Ordering::Relaxed);
    }

    pub fn is_raised(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Which privilege level a controller, or one half of one, belongs to. The two levels
/// have separate enables, separate pending bits and separate wires into a hart, so
/// every interrupt controller on this machine is really one of these per level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Machine,
    Supervisor,
}

impl Level {
    /// The bit this level's external interrupt asserts in `mip`.
    pub fn external(self) -> u64 {
        match self {
            Self::Machine => MEIP,
            Self::Supervisor => SEIP,
        }
    }
}

/// Where a message-signalled interrupt goes: a device posts an identity to an address
/// and learns nothing about what is there, which is the same bargain `Line` makes.
///
/// It is not a bus access. The things that post one are themselves on the bus and
/// cannot re-enter it, and an MSI is not ordinary traffic anyway: it is a posted write
/// the interconnect delivers to an interrupt file. What decodes the address is the
/// machine, which is the only thing that knows where it put anything.
#[derive(Clone, Default)]
pub struct Msi(Option<Arc<dyn Fn(u64, u32) + Send + Sync>>);

impl Msi {
    /// An `Msi` delivered by `deliver`, which is handed the address written and the
    /// identity written there.
    pub fn new(deliver: impl Fn(u64, u32) + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(deliver)))
    }

    /// Post `identity` to `addr`. A machine with nowhere to deliver one drops it,
    /// which is what a write to an address nothing answers for does.
    pub fn send(&self, addr: u64, identity: u32) {
        if let Some(deliver) = &self.0 {
            deliver(addr, identity);
        }
    }
}

impl fmt::Debug for Msi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Some(_) => "Msi(wired)",
            None => "Msi(nowhere)",
        })
    }
}

/// `Send`, because phase 10 puts the window on the main thread and the harts on
/// another, and a machine cannot cross a thread boundary if the things on its bus
/// cannot. Everything here already qualifies; saying so is what keeps it that way.
pub trait Device: std::fmt::Debug + Send {
    /// Read `size` bits at `offset` from the device's base.
    ///
    /// This takes `&mut self` because a read can be an action rather than a question:
    /// a uart's receive register consumes the byte it returns, and a plic's claim
    /// register hands over an interrupt.
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception>;

    /// Write `size` bits at `offset` from the device's base.
    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception>;

    /// The bits this device is asserting in `mip` for `hart`. Only an interrupt
    /// controller drives `mip` directly; everything else raises a line into one and
    /// answers zero here.
    ///
    /// It is asked per hart because the two controllers that answer it are per hart:
    /// a clint has a timer and a software interrupt for each, and a plic a context for
    /// each privilege level of each. Nothing else can tell them apart.
    fn interrupts(&self, _hart: usize) -> u64 {
        0
    }

    /// Notice anything that has changed without an access to notice it at: a byte
    /// typed at a serial port whose backend is not the hart, a timer that a frontend
    /// advances. Asked once every time round the harts rather than per hart, because
    /// it is not a question about a hart and there is no reason to ask it many times.
    fn poll(&mut self) {}
}
