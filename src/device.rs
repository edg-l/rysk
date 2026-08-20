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
    bus::DRAM_BASE,
    csr::{MEIP, SEIP},
    dram::Dram,
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

/// The `mip` bits the interrupt controllers are asserting, one word per hart.
///
/// A controller publishes into this whenever its own state changes, and the bus reads
/// it. That direction is the point: what a hart needs before every instruction is the
/// answer, and working the answer out is a walk over sources, enables, priorities and
/// thresholds that only a write, a message or a wire can change. Asking cost more than
/// interpreting the instruction the question was asked about.
///
/// There is one word rather than one per controller, so the question is one load from
/// one cache line however many controllers a machine has. What keeps them out of each
/// other's way is that a handle carries the mask of bits its holder owns, and a machine
/// gives no two controllers the same bit: a clint owns the software and timer
/// interrupts, and whichever of the plic, the aplic and the imsic is the one reaching
/// its harts owns the external ones.
///
/// What it costs is that a controller which forgets to publish keeps asserting what it
/// used to, so every path that changes what a hart should see ends in one of these.
#[derive(Debug, Clone, Default)]
pub struct Pending {
    words: Arc<[AtomicU64]>,
    /// The bits this handle may write. The word the bus reads is every controller's
    /// bits together, so a handle can only ever say something about its own.
    mask: u64,
}

impl Pending {
    /// A word for each of `harts` harts, all clear, owned by nobody yet.
    pub fn new(harts: usize) -> Self {
        Self {
            words: (0..harts).map(|_| AtomicU64::new(0)).collect(),
            mask: 0,
        }
    }

    /// The same words, as the handle of a controller that owns `mask`.
    pub fn owning(&self, mask: u64) -> Self {
        Self {
            words: self.words.clone(),
            mask,
        }
    }

    /// Assert exactly `bits` of what this handle owns at `hart`, replacing whatever it
    /// asserted before and leaving every other controller's bits alone.
    pub fn set(&self, hart: usize, bits: u64) {
        if let Some(word) = self.words.get(hart) {
            let mine = bits & self.mask;
            let _ = word.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some((current & !self.mask) | mine)
            });
        }
    }

    /// Everything every controller is asserting at `hart`.
    #[inline]
    pub fn get(&self, hart: usize) -> u64 {
        match self.words.get(hart) {
            Some(word) => word.load(Ordering::Relaxed),
            None => 0,
        }
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

    /// Take a handle on the word this machine's controllers drive `mip` through, and
    /// say whether this device is one of them. A device that is not ignores the word
    /// and answers no; everything it has to say about interrupts it says by raising a
    /// line into a controller.
    ///
    /// Asked once, when the device is attached, rather than per hart per instruction.
    /// A controller seen at more than one address is asked once per address and takes
    /// a handle each time, which is the same word both times.
    fn wire(&mut self, _pending: &Pending) -> bool {
        false
    }

    /// Notice anything that has changed without an access to notice it at: a byte
    /// typed at a serial port whose backend is not the hart, a timer that a frontend
    /// advances. Asked once every time round the harts rather than per hart, because
    /// it is not a question about a hart and there is no reason to ask it many times.
    fn poll(&mut self) {}
}

/// The guest memory a bus-mastering device transfers to and from.
///
/// A device holding one reaches memory without going back through the bus that is
/// holding it, which is the same bargain `Msi` makes and for the same reason: the thing
/// doing the transfer is itself being accessed, and an access cannot wait on the access
/// it is inside.
///
/// What it reaches is memory and nothing else. A descriptor pointing at another
/// device's window reaches nothing rather than that device, which is a guest asking for
/// peer-to-peer traffic on a machine that has no path for it, and a transfer that runs
/// off the end of memory does not happen rather than happening somewhere else.
///
/// It does not break a hart's reservation, which real coherence would. What that leaves
/// is the case a store-conditional's data comparison cannot see either: a transfer that
/// wrote back the byte that was already there. Anything that changed a reserved word is
/// caught by the comparison in `Bus::store_conditional`.
#[derive(Debug, Clone, Default)]
pub struct Dma(Option<Arc<Dram>>);

impl Dma {
    pub fn new(memory: Arc<Dram>) -> Self {
        Self(Some(memory))
    }

    /// Whether all `len` bytes at `addr` are memory this can reach.
    fn holds(&self, addr: u64, len: usize) -> Option<&Dram> {
        let memory = self.0.as_deref()?;
        let end = addr.checked_add(len as u64)?;
        (DRAM_BASE <= addr && end <= DRAM_BASE + memory.size()).then_some(memory)
    }

    /// Fill `into` from `addr`. Answers whether the whole of it was memory; a transfer
    /// that was not leaves `into` as it found it.
    pub fn read(&self, addr: u64, into: &mut [u8]) -> bool {
        let Some(memory) = self.holds(addr, into.len()) else {
            return false;
        };
        let at = (addr - DRAM_BASE) as usize;
        into.copy_from_slice(&unsafe { memory.as_slice() }[at..at + into.len()]);
        true
    }

    /// Put `from` at `addr`, with the same answer.
    pub fn write(&self, addr: u64, from: &[u8]) -> bool {
        match self.holds(addr, from.len()) {
            Some(memory) => memory.write(addr, from, 0),
            None => false,
        }
    }

    /// The `size` bits at `addr`, which is how a descriptor's fields are read.
    pub fn load(&self, addr: u64, size: u64) -> Option<u64> {
        let mut bytes = [0u8; 8];
        let len = (size / 8) as usize;
        self.read(addr, &mut bytes[..len])
            .then(|| u64::from_le_bytes(bytes))
    }

    /// And how one is written back.
    pub fn store(&self, addr: u64, size: u64, value: u64) -> bool {
        self.write(addr, &value.to_le_bytes()[..(size / 8) as usize])
    }
}
