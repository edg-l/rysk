//! What the bus talks to, other than dram.
//!
//! A device is a model behind this trait and nothing else: it is told what was read or
//! written at an offset from wherever it was placed, and it says whether it is
//! asserting an interrupt. It never learns its own address, which controller its line
//! runs to, or what else exists.
//!
//! Interrupting comes in two shapes here, and neither of them names a controller: a
//! `Line` a device holds up, and an `Msi` it posts. What comes out the far end is a
//! `Pending`, the word of `mip` bits a controller drives.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

/// The `mip` bits an interrupt controller is asserting, one word per hart.
///
/// A controller publishes into this whenever its own state changes, and the bus reads
/// it. That direction is the point: what a hart needs before every instruction is the
/// answer, and working the answer out is a walk over sources, enables, priorities and
/// thresholds that only a write, a message or a wire can change. Asking cost more than
/// interpreting the instruction the question was asked about.
///
/// What it costs is that a controller which forgets to publish keeps asserting what it
/// used to, so every path that changes what a hart should see ends in one of these.
#[derive(Debug, Clone, Default)]
pub struct Pending(Arc<[AtomicU64]>);

impl Pending {
    /// A word for each of `harts` harts, all clear.
    pub fn new(harts: usize) -> Self {
        Self((0..harts).map(|_| AtomicU64::new(0)).collect())
    }

    /// Assert exactly `bits` at `hart`, replacing whatever this controller asserted
    /// before. A controller owns its whole word, so it says what it is asserting
    /// rather than adding to what is there.
    pub fn set(&self, hart: usize, bits: u64) {
        if let Some(word) = self.0.get(hart) {
            word.store(bits, Ordering::Relaxed);
        }
    }

    pub fn get(&self, hart: usize) -> u64 {
        match self.0.get(hart) {
            Some(word) => word.load(Ordering::Relaxed),
            None => 0,
        }
    }

    /// Whether these are the same word rather than two words that agree. A controller
    /// seen at more than one address hands out the same one, and the bus has no use
    /// for the second copy.
    pub fn is(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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

    /// Whether this goes anywhere. A machine that wired one has somewhere to deliver
    /// a message to, which is what makes message delivery something a controller can
    /// be built for rather than switched into.
    pub fn wired(&self) -> bool {
        self.0.is_some()
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

    /// The word this device drives `mip` through, if it drives `mip` at all. Only an
    /// interrupt controller does; everything else raises a line into one and answers
    /// nothing here.
    ///
    /// Asked once, when the device is attached, rather than per hart per instruction.
    /// A controller seen at more than one address answers with the same word each
    /// time, and the bus keeps one of them.
    fn pending(&self) -> Option<Pending> {
        None
    }

    /// Notice anything that has changed without an access to notice it at: a byte
    /// typed at a serial port whose backend is not the hart, a timer that a frontend
    /// advances. Asked once every time round the harts rather than per hart, because
    /// it is not a question about a hart and there is no reason to ask it many times.
    fn poll(&mut self) {}
}
