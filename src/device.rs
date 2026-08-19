//! What the bus talks to, other than dram.
//!
//! A device is a model behind this trait and nothing else: it is told what was read or
//! written at an offset from wherever it was placed, and it says whether it is
//! asserting an interrupt. It never learns its own address, which controller its line
//! runs to, or what else exists.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::trap::Exception;

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
