//! The incoming MSI controller: the part of a hart that receives messages.
//!
//! Where the PLIC is wires all the way to the hart, this is a mailbox. A device posts
//! a write, the write is an interrupt identity, and the identity sets a bit in the
//! interrupt file of the hart the address belongs to. Nothing is claimed from a shared
//! controller and nothing is completed back to one, so two harts taking interrupts do
//! not contend for anything.
//!
//! An interrupt file is reached two ways and they see the same state: through the one
//! register a page of address space exposes, which is where messages arrive, and
//! through the `miselect`/`mireg` window, which is how the hart that owns the file
//! enables and reads it. Only the first is on the bus.
//!
//! The RISC-V Advanced Interrupt Architecture, chapter 3.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use crate::{
    csr::{MEIP, SEIP},
    device::{Device, Level, Pending, Report, Value, field},
    trap::Exception,
};

/// Where the machine-level interrupt files are, and where the supervisor-level ones
/// are. The two levels are kept apart so that a hart can grant supervisor access to
/// every supervisor file with one protection entry rather than one per hart.
/// The RISC-V Advanced Interrupt Architecture, 3.6.
pub const MACHINE: u64 = 0x2400_0000;
pub const SUPERVISOR: u64 = 0x2800_0000;

/// One page per interrupt file, which is what makes the address of a hart's file its
/// number times this.
pub const PAGE: u64 = 0x1000;

/// The largest interrupt identity a file implements, which is what `riscv,num-ids`
/// publishes. Identity zero is never implemented: it is what "no interrupt" reads as.
pub const IDENTITIES: u32 = 255;

/// The identities in one indirectly accessed register, which is `XLEN` because the
/// window is as wide as the machine.
const PER_REGISTER: u32 = 64;
/// How many of those it takes to hold every identity.
const REGISTERS: usize = (IDENTITIES as usize + 1).div_ceil(PER_REGISTER as usize);

/// The only registers a file's page has. A read of either is zero, and everything else
/// in the page is read-only zero.
/// The RISC-V Advanced Interrupt Architecture, 3.5.
const SETEIPNUM_LE: u64 = 0x000;
const SETEIPNUM_BE: u64 = 0x004;

/// What a value of `miselect` in the external-interrupt range names.
/// The RISC-V Advanced Interrupt Architecture, 3.7.
pub const EIDELIVERY: u64 = 0x70;
pub const EITHRESHOLD: u64 = 0x72;
pub const EIP0: u64 = 0x80;
pub const EIE0: u64 = 0xc0;
/// One past the last of each array, which is sixty-four registers apart.
const EIP_END: u64 = EIP0 + 0x40;
const EIE_END: u64 = EIE0 + 0x40;
/// The whole range `miselect` hands to an interrupt file.
pub const SELECT: Range<u64> = EIDELIVERY..EIE_END;

/// What a read of a `*topei` register puts an identity in, twice: once as the identity
/// and once as its priority, which for an interrupt file are the same number.
/// The RISC-V Advanced Interrupt Architecture, 3.9.
const TOPEI_IDENTITY: u64 = 16;

/// One interrupt file: which identities have arrived, which the hart wants, whether it
/// wants any of them at all, and how far down the priorities it is listening.
#[derive(Debug, Default, Clone)]
struct File {
    /// `eidelivery`. Only the two ordinary values are supported, so an APLIC never
    /// delivers to a hart that has one of these; on this machine a hart either has an
    /// IMSIC and is reached by messages, or has neither and is reached by wires.
    /// The RISC-V Advanced Interrupt Architecture, 3.8.1.
    delivery: bool,
    /// `eithreshold`. Nonzero means identities from here up do not signal, whatever
    /// their enable bit says. The RISC-V Advanced Interrupt Architecture, 3.8.2.
    threshold: u32,
    pending: [u64; REGISTERS],
    enabled: [u64; REGISTERS],
}

impl File {
    /// Whether identity `id` exists here. Zero never does, which is what lets a read
    /// of `mtopei` use it to mean that nothing is waiting.
    fn implemented(id: u32) -> bool {
        id > 0 && id <= IDENTITIES
    }

    /// Set the pending bit for `id`, which is what an arriving message does. One that
    /// names an identity this file does not implement is ignored rather than refused:
    /// there is nothing on the other end of a posted write to refuse to.
    fn set_pending(&mut self, id: u32) {
        if Self::implemented(id) {
            self.pending[(id / PER_REGISTER) as usize] |= 1 << (id % PER_REGISTER);
        }
    }

    fn clear_pending(&mut self, id: u32) {
        if Self::implemented(id) {
            self.pending[(id / PER_REGISTER) as usize] &= !(1 << (id % PER_REGISTER));
        }
    }

    /// The identity to take: the lowest-numbered one that is pending, enabled, and
    /// below the threshold if there is one. Lower identities are higher priority.
    /// The RISC-V Advanced Interrupt Architecture, 3.9.
    fn best(&self) -> Option<u32> {
        (0..REGISTERS)
            .find_map(|word| {
                let ready = self.pending[word] & self.enabled[word];
                (ready != 0).then(|| word as u32 * PER_REGISTER + ready.trailing_zeros())
            })
            .filter(|&id| self.threshold == 0 || id < self.threshold)
    }

    /// What a read of `mtopei` or `stopei` gives: the identity and, in the low bits,
    /// the same number again as its priority.
    fn topei(&self) -> u64 {
        match self.best() {
            Some(id) => ((id as u64) << TOPEI_IDENTITY) | id as u64,
            None => 0,
        }
    }

    /// Whether this file is asserting its external interrupt at the hart.
    /// The RISC-V Advanced Interrupt Architecture, 3.10.
    fn signalling(&self) -> bool {
        self.delivery && self.best().is_some()
    }

    /// Read one of the registers `miselect` reaches. A number in the range with no
    /// register at it reads as zero.
    fn read(&self, select: u64) -> u64 {
        match select {
            EIDELIVERY => self.delivery as u64,
            EITHRESHOLD => self.threshold as u64,
            EIP0..EIP_END => self.array(&self.pending, select - EIP0),
            EIE0..EIE_END => self.array(&self.enabled, select - EIE0),
            _ => 0,
        }
    }

    /// One register of the pending or enable array. With `XLEN` of sixty-four each
    /// register holds twice what its number suggests, so only the even ones exist and
    /// register `2n` is the `n`th sixty-four identities.
    /// The RISC-V Advanced Interrupt Architecture, 3.8.3.
    fn array(&self, of: &[u64; REGISTERS], register: u64) -> u64 {
        match of.get(register as usize / 2) {
            // Identity zero does not exist, so the bit that would be its is zero.
            Some(bits) if register == 0 => *bits & !1,
            Some(bits) => *bits,
            None => 0,
        }
    }

    fn write(&mut self, select: u64, value: u64) {
        match select {
            // WARL over the values that exist, and the one that hands delivery to an
            // APLIC is not one of them here.
            EIDELIVERY => self.delivery = value == 1,
            EITHRESHOLD => self.threshold = (value as u32).min(IDENTITIES),
            EIP0..EIP_END => Self::write_array(&mut self.pending, select - EIP0, value),
            EIE0..EIE_END => Self::write_array(&mut self.enabled, select - EIE0, value),
            _ => {}
        }
    }

    fn write_array(of: &mut [u64; REGISTERS], register: u64, value: u64) {
        if let Some(bits) = of.get_mut(register as usize / 2) {
            // The bit for identity zero stays zero however it is written.
            *bits = if register == 0 { value & !1 } else { value };
        }
    }
}

/// A hart's interrupt files, one per privilege level.
///
/// Held behind a lock because two things reach them: the hart, through its CSRs, and
/// the page a message is written to, which is a device on the bus. They are the same
/// registers seen from both sides, so they cannot be two copies.
#[derive(Debug)]
struct State {
    files: Vec<[File; 2]>,
    /// What the files are asserting at their harts, kept up to date by every path that
    /// changes one. Reading it is what a hart does before an instruction; taking the
    /// lock and asking each file was what it used to do, and that cost more than the
    /// instruction.
    ///
    /// It is in here rather than beside the lock because the machine hands the handle
    /// out once, to whichever clone the bus was given, and every other clone has to see
    /// it: the hart reaching its own files through CSRs is one of them.
    pending: Pending,
}

#[derive(Debug, Clone)]
pub struct Imsic(Arc<Mutex<State>>);

impl Imsic {
    /// An IMSIC for each of `harts` harts.
    pub fn new(harts: usize) -> Self {
        Self(Arc::new(Mutex::new(State {
            files: vec![Default::default(); harts],
            pending: Pending::default(),
        })))
    }

    /// Take the word this machine's controllers drive `mip` through. Where a hart has
    /// interrupt files, they are what reaches it, so both external interrupts are
    /// theirs and the APLIC in front of them drives no wires.
    fn wire(&self, pending: &Pending) {
        let mut inner = self.0.lock().unwrap();
        inner.pending = pending.owning(MEIP | SEIP);
        // A handle taken says what the files are asserting already, which is not
        // always nothing: what a machine is built holding is as real as what arrives.
        for hart in 0..inner.files.len() {
            let bits = [Level::Machine, Level::Supervisor]
                .into_iter()
                .filter(|at| inner.files[hart][*at as usize].signalling())
                .fold(0, |bits, at| bits | at.external());
            inner.pending.set(hart, bits);
        }
    }

    /// The pages one level's interrupt files answer at, as a device on the bus.
    pub fn files(&self, level: Level) -> Files {
        Files {
            imsic: self.clone(),
            level,
        }
    }

    /// Where `level`'s interrupt files start, which is what an address decodes
    /// against. The RISC-V Advanced Interrupt Architecture, 3.6.
    pub fn base(level: Level) -> u64 {
        match level {
            Level::Machine => MACHINE,
            Level::Supervisor => SUPERVISOR,
        }
    }

    /// How much address space `harts` files take, which is a page each.
    pub fn size(harts: usize) -> u64 {
        harts as u64 * PAGE
    }

    /// Look at one file without changing it.
    fn peek<T>(&self, hart: usize, level: Level, f: impl FnOnce(&File) -> T) -> Option<T> {
        let inner = self.0.lock().unwrap();
        Some(f(&inner.files.get(hart)?[level as usize]))
    }

    /// Change one file, and say afterwards what that hart's two files are asserting.
    /// Every path that can change what a hart should see goes through here, which is
    /// what keeps the published word true.
    fn with<T>(&self, hart: usize, level: Level, f: impl FnOnce(&mut File) -> T) -> Option<T> {
        let mut inner = self.0.lock().unwrap();
        let file = inner.files.get_mut(hart)?;
        let answer = f(&mut file[level as usize]);
        let mut bits = 0;
        for at in [Level::Machine, Level::Supervisor] {
            if file[at as usize].signalling() {
                bits |= at.external();
            }
        }
        inner.pending.set(hart, bits);
        Some(answer)
    }

    /// Deliver a message: the identity `id` has arrived for `hart` at `level`.
    pub fn deliver(&self, hart: usize, level: Level, id: u32) {
        self.with(hart, level, |file| file.set_pending(id));
    }

    /// Read the register `select` names in `hart`'s file at `level`, which is what
    /// `mireg` and `sireg` answer with.
    pub fn read(&self, hart: usize, level: Level, select: u64) -> u64 {
        self.peek(hart, level, |file| file.read(select))
            .unwrap_or(0)
    }

    pub fn write(&self, hart: usize, level: Level, select: u64, value: u64) {
        self.with(hart, level, |file| file.write(select, value));
    }

    /// What a read of `mtopei` or `stopei` gives.
    pub fn topei(&self, hart: usize, level: Level) -> u64 {
        self.peek(hart, level, |file| file.topei()).unwrap_or(0)
    }

    /// Claim whatever `mtopei` reports, which clears its pending bit. The value
    /// written is ignored: what is claimed is what the register currently reads as.
    /// The RISC-V Advanced Interrupt Architecture, 3.9.
    pub fn claim(&self, hart: usize, level: Level) {
        self.with(hart, level, |file| {
            if let Some(id) = file.best() {
                file.clear_pending(id);
            }
        });
    }

    /// Whether `hart`'s file at `level` is asserting its external interrupt.
    pub fn signalling(&self, hart: usize, level: Level) -> bool {
        self.peek(hart, level, |file| file.signalling())
            .unwrap_or(false)
    }
}

/// One level's interrupt files, as the pages messages are written to.
///
/// This is the whole of the IMSIC that is on the bus. Everything else about an
/// interrupt file is reached by the hart that owns it, through CSRs, and a hart has no
/// address for another hart's file.
#[derive(Debug)]
pub struct Files {
    imsic: Imsic,
    level: Level,
}

impl Device for Files {
    fn describe(&self) -> Report {
        let inner = self.imsic.0.lock().unwrap_or_else(|held| held.into_inner());
        let mut fields = Vec::new();
        for (hart, files) in inner.files.iter().enumerate() {
            let file = &files[self.level as usize];
            fields.push(field(
                format!("hart {hart} delivery"),
                Value::Flag(file.delivery),
            ));
            fields.push(field(
                format!("hart {hart} threshold"),
                Value::Count(u64::from(file.threshold)),
            ));
            fields.push(field(
                format!("hart {hart} top identity"),
                Value::Count(u64::from(file.best().unwrap_or(0))),
            ));
            fields.push(field(
                format!("hart {hart} signalling"),
                Value::Flag(file.signalling()),
            ));
        }
        Report::new(
            match self.level {
                Level::Machine => "imsic (machine)",
                Level::Supervisor => "imsic (supervisor)",
            },
            fields,
        )
    }

    /// Every readable byte of a file's page reads as zero, including the write port
    /// itself. The RISC-V Advanced Interrupt Architecture, 3.5.
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        match (size, offset % 4) {
            (32, 0) => Ok(0),
            _ => Err(Exception::LoadAccessFault(offset)),
        }
    }

    /// A message: the identity written lands in the file whose page it was written to.
    ///
    /// Only aligned words are a write to this device at all. Anything else is reported
    /// as an access fault rather than quietly dropped, which is what the specification
    /// asks an implementation to prefer.
    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if size != 32 || !offset.is_multiple_of(4) {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        let hart = (offset / PAGE) as usize;
        match offset % PAGE {
            SETEIPNUM_LE => self.imsic.deliver(hart, self.level, value as u32),
            // The big-endian write port. This machine has no big-endian anything, and
            // a system that is little-endian only may ignore every write to it.
            SETEIPNUM_BE => {}
            _ => {}
        }
        Ok(())
    }

    fn wire(&mut self, pending: &Pending) -> bool {
        self.imsic.wire(pending);
        true
    }
}
