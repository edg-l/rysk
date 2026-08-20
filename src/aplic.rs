//! The advanced platform-level interrupt controller: what wires still run to on a
//! machine whose harts are interrupted by messages.
//!
//! It is the PLIC's replacement and not its descendant. Two things are new. A source
//! says what a wire means, so an edge is an edge and a level is a level rather than
//! everything being a level the controller holds until it is completed. And a domain
//! can turn a wire into a message: in MSI delivery mode nothing is claimed here at
//! all, the controller posts the identity its target register names and forgets it.
//!
//! A domain is one privilege level's view of the same sources. The machine-level
//! domain owns every source until it delegates one to its child, and a delegated
//! source is the child's alone: the two domains are two control regions over one set
//! of wires, which is why they are one model here seen at two addresses.
//!
//! The RISC-V Advanced Interrupt Architecture, chapter 4.

use std::sync::{Arc, Mutex};

use crate::{
    device::{Device, Level, Line, Msi, Pending},
    imsic::PAGE,
    trap::Exception,
};

/// Where each domain's control region is. The machine-level one is where a PLIC would
/// have been, since a machine has one external interrupt controller and these are the
/// alternatives.
pub const MACHINE: u64 = 0x0c00_0000;
pub const SUPERVISOR: u64 = 0x0d00_0000;
/// A control region is a multiple of four kibibytes and this one has room for the
/// registers plus a delivery structure for five hundred and twelve harts.
pub const SIZE: u64 = 0x8000;

/// How many sources this controller has, which is what the `virt` machine's has and
/// three times what the machine wires. Source zero does not exist: as in a PLIC, it is
/// what a claim reads as when there is nothing to claim. Anything above the last one is
/// not implemented, so its registers read as zero and take no write.
/// The RISC-V Advanced Interrupt Architecture, 4.1.
pub const SOURCES: usize = 96;
const WORDS: usize = SOURCES / 32;

/// The registers of a domain's control region.
/// The RISC-V Advanced Interrupt Architecture, 4.5, table 6.
const DOMAINCFG: u64 = 0x0000;
const SOURCECFG: u64 = 0x0004;
const SOURCECFG_END: u64 = 0x1000;
const MMSIADDRCFG: u64 = 0x1bc0;
const MMSIADDRCFG_END: u64 = 0x1bd0;
const SETIP: u64 = 0x1c00;
const SETIP_END: u64 = 0x1c80;
const SETIPNUM: u64 = 0x1cdc;
const IN_CLRIP: u64 = 0x1d00;
const IN_CLRIP_END: u64 = 0x1d80;
const CLRIPNUM: u64 = 0x1ddc;
const SETIE: u64 = 0x1e00;
const SETIE_END: u64 = 0x1e80;
const SETIENUM: u64 = 0x1edc;
const CLRIE: u64 = 0x1f00;
const CLRIE_END: u64 = 0x1f80;
const CLRIENUM: u64 = 0x1fdc;
const SETIPNUM_LE: u64 = 0x2000;
const SETIPNUM_BE: u64 = 0x2004;
const GENMSI: u64 = 0x3000;
const TARGET: u64 = 0x3004;
const TARGET_END: u64 = 0x4000;
/// The delivery structures, one per hart index, packed thirty-two bytes apart.
/// The RISC-V Advanced Interrupt Architecture, 4.8.1.
const IDC: u64 = 0x4000;
const IDC_STRIDE: u64 = 0x20;
const IDELIVERY: u64 = 0x00;
const IFORCE: u64 = 0x04;
const ITHRESHOLD: u64 = 0x08;
const TOPI: u64 = 0x18;
const CLAIMI: u64 = 0x1c;

/// `domaincfg`: the byte that says which end of it is which, the global enable, and
/// the delivery mode. The RISC-V Advanced Interrupt Architecture, 4.5.1.
const DOMAINCFG_READBACK: u32 = 0x8000_0000;
const DOMAINCFG_IE: u32 = 1 << 8;
const DOMAINCFG_DM: u32 = 1 << 2;

/// `sourcecfg`: the bit that delegates a source to the child domain, and the mode the
/// rest of it holds when it does not.
/// The RISC-V Advanced Interrupt Architecture, 4.5.2, table 7.
const SOURCECFG_DELEGATE: u32 = 1 << 10;
const SOURCECFG_MODE: u32 = 0b111;
const INACTIVE: u32 = 0;
const DETACHED: u32 = 1;
const EDGE1: u32 = 4;
const EDGE0: u32 = 5;
const LEVEL1: u32 = 6;
const LEVEL0: u32 = 7;

/// `target`, whose two halves mean different things in the two delivery modes: a hart
/// and a priority when the controller delivers, a hart and an identity to post when it
/// forwards. The RISC-V Advanced Interrupt Architecture, 4.5.16.
const TARGET_HART: u32 = 18;
const TARGET_PRIORITY: u32 = 0xff;
const TARGET_IDENTITY: u32 = 0x7ff;

/// What a read of `topi` or `claimi` puts the source number in.
/// The RISC-V Advanced Interrupt Architecture, 4.8.1.4.
const TOPI_IDENTITY: u32 = 16;

/// One hart's delivery structure: whether it is listening, whether it is being made to
/// listen, and how far down the priorities it cares.
#[derive(Debug, Default, Clone)]
struct Idc {
    delivery: bool,
    force: bool,
    threshold: u32,
}

/// One privilege level's control region: which sources it has and what it does with
/// them. Both domains have all of these; which sources are actually theirs is decided
/// by the machine-level domain's delegation bits.
#[derive(Debug)]
struct Domain {
    /// `domaincfg.IE` and `domaincfg.DM`.
    enabled: bool,
    forwards: bool,
    sourcecfg: Vec<u32>,
    pending: [u32; WORDS],
    enable: [u32; WORDS],
    target: Vec<u32>,
    idc: Vec<Idc>,
    /// Where this domain's messages go: one page per hart, which is the arrangement
    /// section 3.6 recommends and this controller has hardwired, so the registers that
    /// would configure it are read-only.
    /// The RISC-V Advanced Interrupt Architecture, 4.9.1.
    files: u64,
}

impl Domain {
    fn new(harts: usize, files: u64) -> Self {
        Self {
            enabled: false,
            forwards: false,
            sourcecfg: vec![0; SOURCES],
            pending: [0; WORDS],
            enable: [0; WORDS],
            target: vec![0; SOURCES],
            idc: vec![Idc::default(); harts],
            files,
        }
    }

    /// The source mode this domain has `source` in, which is what it says when the
    /// source is not delegated away.
    fn mode(&self, source: usize) -> u32 {
        match self.sourcecfg[source] & SOURCECFG_DELEGATE {
            0 => self.sourcecfg[source] & SOURCECFG_MODE,
            _ => INACTIVE,
        }
    }

    /// Do `f` for every source with a pending bit this domain has enabled. The two are
    /// bitmaps, so this looks at a word at a time rather than a source at a time.
    fn ready(&self, mut f: impl FnMut(usize)) {
        for word in 0..WORDS {
            let mut bits = self.pending[word] & self.enable[word];
            while bits != 0 {
                f(word * 32 + bits.trailing_zeros() as usize);
                bits &= bits - 1;
            }
        }
    }

    fn bit(&self, of: &[u32; WORDS], source: usize) -> bool {
        of[source / 32] >> (source % 32) & 1 == 1
    }

    fn set(of: &mut [u32; WORDS], source: usize, on: bool) {
        let mask = 1 << (source % 32);
        match on {
            true => of[source / 32] |= mask,
            false => of[source / 32] &= !mask,
        }
    }

    /// Where a message for the hart `target[source]` names goes.
    fn address(&self, source: usize) -> u64 {
        let hart = (self.target[source] >> TARGET_HART) as u64;
        self.files + hart * PAGE
    }
}

/// The whole controller: the wires, and the two domains that see them.
#[derive(Debug)]
struct Controller {
    /// The line each source is driven by, if anything drives it.
    lines: Vec<(usize, Line)>,
    /// What the rectified input of each source was when it was last looked at, which
    /// is the only way an edge can be told from a level that was already high.
    seen: [u32; WORDS],
    domains: [Domain; 2],
    /// Where a forwarded interrupt is posted.
    msi: Msi,
    /// What the two domains are asserting at each hart, said again whenever anything
    /// here changes. Working it out walks every source a domain has ready, once per
    /// hart per level, which is not a thing to do before every instruction.
    pending: Pending,
}

impl Controller {
    fn new(harts: usize, msi: Msi) -> Self {
        Self {
            lines: Vec::new(),
            seen: [0; WORDS],
            domains: [
                Domain::new(harts, crate::imsic::MACHINE),
                Domain::new(harts, crate::imsic::SUPERVISOR),
            ],
            msi,
            pending: Pending::new(harts),
        }
    }

    /// Say what both domains are asserting at every hart.
    fn publish(&self) {
        for hart in 0..self.domains[0].idc.len() {
            let mut bits = 0;
            for level in [Level::Machine, Level::Supervisor] {
                if self.signalling(level, hart) {
                    bits |= level.external();
                }
            }
            self.pending.set(hart, bits);
        }
    }

    fn domain(&self, level: Level) -> &Domain {
        &self.domains[level as usize]
    }

    fn domain_mut(&mut self, level: Level) -> &mut Domain {
        &mut self.domains[level as usize]
    }

    /// Which domain a source is active in, and in which mode. A source belongs to the
    /// machine-level domain unless that domain has delegated it, and to nobody if the
    /// domain that has it calls it inactive.
    /// The RISC-V Advanced Interrupt Architecture, 4.2 and 4.5.2.
    fn owner(&self, source: usize) -> Option<(Level, u32)> {
        // Source zero is not a source, and the register layout has room for far more
        // of them than this controller has.
        if source == 0 || source >= SOURCES {
            return None;
        }
        let level =
            match self.domains[Level::Machine as usize].sourcecfg[source] & SOURCECFG_DELEGATE {
                0 => Level::Machine,
                _ => Level::Supervisor,
            };
        match self.domain(level).mode(source) {
            INACTIVE => None,
            mode => Some((level, mode)),
        }
    }

    /// What the wire driving `source` reads as.
    fn wire(&self, source: usize) -> bool {
        self.lines
            .iter()
            .any(|(at, line)| *at == source && line.is_raised())
    }

    /// The wire as the source's mode reads it: inverted for the two modes that call a
    /// low signal an interrupt, and nothing at all for a source whose wire is ignored.
    /// The RISC-V Advanced Interrupt Architecture, 4.5.2.
    fn rectified(&self, source: usize) -> bool {
        match self.owner(source) {
            Some((_, EDGE1 | LEVEL1)) => self.wire(source),
            Some((_, EDGE0 | LEVEL0)) => !self.wire(source),
            _ => false,
        }
    }

    /// Whether a source is pending. For a level-sensitive source in a domain that
    /// delivers its own interrupts the pending bit is not storage at all: it is the
    /// rectified input, always, so it is answered from the wire rather than kept.
    /// The RISC-V Advanced Interrupt Architecture, 4.7.
    fn pending(&self, source: usize) -> bool {
        match self.owner(source) {
            None => false,
            Some((level, LEVEL1 | LEVEL0)) if !self.domain(level).forwards => {
                self.rectified(source)
            }
            Some((level, _)) => {
                let domain = self.domain(level);
                domain.bit(&domain.pending, source)
            }
        }
    }

    /// Set a source pending, if this is a source and a mode where that can happen.
    /// A level-sensitive source follows its wire and cannot be set by software: in a
    /// domain that forwards, only while the wire is already asserted; in one that
    /// delivers, never. The RISC-V Advanced Interrupt Architecture, 4.7.
    fn set_pending(&mut self, source: usize) {
        let Some((level, mode)) = self.owner(source) else {
            return;
        };
        let allowed = match mode {
            LEVEL1 | LEVEL0 => self.domain(level).forwards && self.rectified(source),
            _ => true,
        };
        if allowed {
            let domain = self.domain_mut(level);
            Domain::set(&mut domain.pending, source, true);
        }
    }

    /// Clear a source's pending bit, which a claim, a forwarded message and an
    /// explicit write all do. A level-sensitive source in a domain that delivers its
    /// own interrupts has no bit to clear.
    fn clear_pending(&mut self, source: usize) {
        let Some((level, mode)) = self.owner(source) else {
            return;
        };
        if matches!(mode, LEVEL1 | LEVEL0) && !self.domain(level).forwards {
            return;
        }
        let domain = self.domain_mut(level);
        Domain::set(&mut domain.pending, source, false);
    }

    /// Notice what the wires have done since the last look: a rising edge sets a
    /// source pending, and a level that has gone away takes a forwarding domain's
    /// pending bit with it.
    /// The RISC-V Advanced Interrupt Architecture, 4.7.
    fn sample(&mut self) {
        // Only a source with a wire has anything here to notice, and this machine
        // wires a handful of the sources the controller could have.
        let wired: Vec<usize> = self.lines.iter().map(|(source, _)| *source).collect();
        for source in wired {
            let Some((level, mode)) = self.owner(source) else {
                continue;
            };
            let now = self.rectified(source);
            let before = self.seen[source / 32] >> (source % 32) & 1 == 1;
            Domain::set(&mut self.seen, source, now);
            match mode {
                // A detached source ignores its wire entirely: only a write can make
                // one pending.
                DETACHED => {}
                EDGE1 | EDGE0 => {
                    if now && !before {
                        self.set_pending(source);
                    }
                }
                _ if self.domain(level).forwards => {
                    if now && !before {
                        let domain = self.domain_mut(level);
                        Domain::set(&mut domain.pending, source, true);
                    } else if !now {
                        let domain = self.domain_mut(level);
                        Domain::set(&mut domain.pending, source, false);
                    }
                }
                // A level-sensitive source in a domain that delivers is answered from
                // the wire, so there is nothing here to keep.
                _ => {}
            }
        }
    }

    /// Post a message for every source a forwarding domain has ready, and clear each
    /// one as it goes. A source that is still asserting its wire says so again only on
    /// its next rising edge, which is what stops one wire becoming a stream of
    /// messages. The RISC-V Advanced Interrupt Architecture, 4.9 and 4.9.2.
    fn forward(&mut self) {
        for level in [Level::Machine, Level::Supervisor] {
            if !self.domain(level).forwards || !self.domain(level).enabled {
                continue;
            }
            let mut sources = Vec::new();
            self.each_ready(level, &mut |source| sources.push(source));
            for source in sources {
                self.clear_pending(source);
                let domain = self.domain(level);
                let identity = domain.target[source] & TARGET_IDENTITY;
                let address = domain.address(source);
                self.msi.send(address, identity);
            }
        }
    }

    /// The sources of `level` that are pending, enabled and this domain's.
    ///
    /// Two things can make a source pending: a bit in the domain's own array, and, for
    /// a level-sensitive source in a domain that delivers, the wire itself, which has
    /// no bit to look at. So the sources worth asking about are the ones with a bit
    /// set and the ones with a wire, which on this machine is a handful either way.
    ///
    /// Keeping it to those is not a refinement. This is asked once per hart before
    /// every instruction, and walking the sources a domain could have rather than the
    /// ones it does made booting a kernel take longer than a kernel takes to boot.
    fn each_ready(&self, level: Level, f: &mut impl FnMut(usize)) {
        let domain = self.domain(level);
        let mut ask = |source: usize| {
            if self.owner(source).map(|(at, _)| at) == Some(level)
                && domain.bit(&domain.enable, source)
                && self.pending(source)
            {
                f(source);
            }
        };
        domain.ready(&mut ask);
        // A source with both a bit and a wire is asked about twice, which costs a
        // second look and answers the same.
        for (source, _) in &self.lines {
            ask(*source);
        }
    }

    /// The source `hart` should take from `level`: the highest-priority one that is
    /// pending, enabled, aimed at that hart and above its threshold. Ties go to the
    /// lowest source number, and a smaller priority number is the higher priority.
    /// The RISC-V Advanced Interrupt Architecture, 4.8.1.4.
    fn topi(&self, level: Level, hart: usize) -> u32 {
        let domain = self.domain(level);
        let Some(idc) = domain.idc.get(hart) else {
            return 0;
        };
        let mut best: Option<(u32, usize)> = None;
        self.each_ready(level, &mut |source| {
            let target = domain.target[source];
            let priority = target & TARGET_PRIORITY;
            // Ties go to the lowest source number, which is what comparing the pair
            // in this order gives.
            let candidate = (priority, source);
            if (target >> TARGET_HART) as usize == hart
                && (idc.threshold == 0 || priority < idc.threshold)
                && best.is_none_or(|current| candidate < current)
            {
                best = Some(candidate);
            }
        });
        match best {
            Some((priority, source)) => ((source as u32) << TOPI_IDENTITY) | priority,
            None => 0,
        }
    }

    /// Take the top interrupt, which clears its pending bit. A claim that finds
    /// nothing is the end of a forced interrupt rather than of a real one.
    /// The RISC-V Advanced Interrupt Architecture, 4.8.1.5.
    fn claim(&mut self, level: Level, hart: usize) -> u32 {
        let top = self.topi(level, hart);
        match top {
            0 => self.domain_mut(level).idc[hart].force = false,
            _ => self.clear_pending((top >> TOPI_IDENTITY) as usize),
        }
        top
    }

    /// Whether this level is asserting an external interrupt at `hart`. A domain that
    /// forwards asserts nothing: what it sends arrives at an interrupt file instead.
    /// The RISC-V Advanced Interrupt Architecture, 4.8.2.
    fn signalling(&self, level: Level, hart: usize) -> bool {
        let domain = self.domain(level);
        match domain.idc.get(hart) {
            Some(idc) => {
                !domain.forwards
                    && domain.enabled
                    && idc.delivery
                    && (idc.force || self.topi(level, hart) != 0)
            }
            None => false,
        }
    }

    /// Write one source's configuration. A source the machine-level domain has not
    /// delegated is not the child's to configure, and the child is a leaf, so it has
    /// no child of its own to delegate to and a write that says otherwise is refused
    /// by being taken as zero.
    /// The RISC-V Advanced Interrupt Architecture, 4.5.2.
    fn configure(&mut self, level: Level, source: usize, value: u32) {
        if source == 0 || source >= SOURCES {
            return;
        }
        match level {
            Level::Machine => {
                let delegated = value & SOURCECFG_DELEGATE != 0;
                let was = self.domains[Level::Machine as usize].sourcecfg[source]
                    & SOURCECFG_DELEGATE
                    != 0;
                self.domains[Level::Machine as usize].sourcecfg[source] = value & 0x7ff;
                // A source that has just become the child's arrives there inactive,
                // whatever the child last said about it.
                if delegated != was {
                    self.domains[Level::Supervisor as usize].sourcecfg[source] = 0;
                }
            }
            Level::Supervisor => {
                if self.domains[Level::Machine as usize].sourcecfg[source] & SOURCECFG_DELEGATE != 0
                {
                    self.domains[Level::Supervisor as usize].sourcecfg[source] =
                        match value & SOURCECFG_DELEGATE {
                            0 => value & SOURCECFG_MODE,
                            _ => 0,
                        };
                }
            }
        }
    }

    fn read(&mut self, level: Level, offset: u64) -> u32 {
        let word = |offset: u64, from: u64| ((offset - from) / 4) as usize * 32;
        let domain = self.domain(level);
        match offset {
            DOMAINCFG => {
                DOMAINCFG_READBACK
                    | if domain.enabled { DOMAINCFG_IE } else { 0 }
                    | if domain.forwards { DOMAINCFG_DM } else { 0 }
            }
            // A source this controller does not have, and one that is not this
            // domain's, both read as not being there.
            SOURCECFG..SOURCECFG_END => {
                let source = (offset / 4) as usize;
                match (level, source < SOURCES) {
                    (Level::Machine, true) => domain.sourcecfg[source],
                    (Level::Supervisor, true) => match self.owner(source) {
                        Some((Level::Supervisor, _)) => domain.sourcecfg[source],
                        _ => 0,
                    },
                    _ => 0,
                }
            }
            // The registers that would say where messages go. They are hardwired here,
            // which the specification allows and recommends, so they are locked and
            // read as nothing else. RISC-V Advanced Interrupt Architecture, 4.5.3.
            MMSIADDRCFG..MMSIADDRCFG_END => match (level, offset) {
                (Level::Machine, MMSIADDRCFG_LOCKED) => LOCKED,
                _ => 0,
            },
            SETIP..SETIP_END => self.bits(word(offset, SETIP), |c, source| c.pending(source)),
            IN_CLRIP..IN_CLRIP_END => {
                self.bits(word(offset, IN_CLRIP), |c, source| c.rectified(source))
            }
            SETIE..SETIE_END => {
                let base = word(offset, SETIE);
                self.bits(base, |c, source| {
                    let domain = c.domain(level);
                    c.owner(source).map(|(at, _)| at) == Some(level)
                        && domain.bit(&domain.enable, source)
                })
            }
            TARGET..TARGET_END => {
                let source = ((offset - TARGET) / 4) as usize + 1;
                match self.owner(source) {
                    Some((at, _)) if at == level => domain.target[source],
                    _ => 0,
                }
            }
            // Reading one of the write-by-number ports, or `genmsi`, which is never
            // busy here because a message is posted before the write completes.
            SETIPNUM | CLRIPNUM | SETIENUM | CLRIENUM | SETIPNUM_LE | SETIPNUM_BE | GENMSI => 0,
            _ if offset >= IDC => {
                let hart = ((offset - IDC) / IDC_STRIDE) as usize;
                let Some(idc) = domain.idc.get(hart) else {
                    return 0;
                };
                match (offset - IDC) % IDC_STRIDE {
                    IDELIVERY => idc.delivery as u32,
                    IFORCE => idc.force as u32,
                    ITHRESHOLD => idc.threshold,
                    TOPI => self.topi(level, hart),
                    CLAIMI => self.claim(level, hart),
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    /// One word of a per-source bitmap, built from whatever answers for a source.
    fn bits(&self, base: usize, of: impl Fn(&Self, usize) -> bool) -> u32 {
        (0..32)
            .filter(|bit| base + bit != 0 && of(self, base + bit))
            .fold(0, |bits, bit| bits | 1 << bit)
    }

    fn write(&mut self, level: Level, offset: u64, value: u32) {
        let each = |value: u32, base: usize| {
            (0..32).filter_map(move |bit| (value >> bit & 1 == 1).then_some(base + bit))
        };
        let word = |offset: u64, from: u64| ((offset - from) / 4) as usize * 32;
        match offset {
            DOMAINCFG => {
                let domain = self.domain_mut(level);
                domain.enabled = value & DOMAINCFG_IE != 0;
                domain.forwards = value & DOMAINCFG_DM != 0;
            }
            SOURCECFG..SOURCECFG_END => self.configure(level, (offset / 4) as usize, value),
            SETIP..SETIP_END => {
                for source in each(value, word(offset, SETIP)) {
                    self.set_pending(source);
                }
            }
            SETIPNUM | SETIPNUM_LE => self.set_pending(value as usize),
            // The big-endian write port, which a machine that is little-endian only
            // may leave as the read-only zeros the rest of the region is.
            SETIPNUM_BE => {}
            IN_CLRIP..IN_CLRIP_END => {
                for source in each(value, word(offset, IN_CLRIP)) {
                    self.clear_pending(source);
                }
            }
            CLRIPNUM => self.clear_pending(value as usize),
            SETIE..SETIE_END => self.enables(level, each(value, word(offset, SETIE)), true),
            SETIENUM => self.enables(level, [value as usize], true),
            CLRIE..CLRIE_END => self.enables(level, each(value, word(offset, CLRIE)), false),
            CLRIENUM => self.enables(level, [value as usize], false),
            // An extempore message, which goes out whatever the domain's enable says.
            // The RISC-V Advanced Interrupt Architecture, 4.5.15.
            GENMSI => {
                if self.domain(level).forwards {
                    let hart = (value >> TARGET_HART) as u64;
                    let domain = self.domain(level);
                    self.msi
                        .send(domain.files + hart * PAGE, value & TARGET_IDENTITY);
                }
            }
            TARGET..TARGET_END => {
                let source = ((offset - TARGET) / 4) as usize + 1;
                if self.owner(source).map(|(at, _)| at) == Some(level) {
                    let forwards = self.domain(level).forwards;
                    self.domain_mut(level).target[source] = target(value, forwards);
                }
            }
            _ if offset >= IDC => {
                let hart = ((offset - IDC) / IDC_STRIDE) as usize;
                let register = (offset - IDC) % IDC_STRIDE;
                let Some(idc) = self.domain_mut(level).idc.get_mut(hart) else {
                    return;
                };
                match register {
                    IDELIVERY => idc.delivery = value & 1 == 1,
                    IFORCE => idc.force = value & 1 == 1,
                    ITHRESHOLD => idc.threshold = value & TARGET_PRIORITY,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Turn a domain's enable bit on or off for each of `sources`, which is only a
    /// source's own domain's business.
    fn enables(&mut self, level: Level, sources: impl IntoIterator<Item = usize>, on: bool) {
        for source in sources {
            if source > 0 && source < SOURCES && self.owner(source).map(|(at, _)| at) == Some(level)
            {
                let domain = self.domain_mut(level);
                Domain::set(&mut domain.enable, source, on);
            }
        }
    }
}

/// The fields a write to a `target` register keeps, which are not the same two in the
/// two delivery modes. A priority of zero is not one, and a write that asks for it
/// gets the highest there is instead.
/// The RISC-V Advanced Interrupt Architecture, 4.5.16.
fn target(value: u32, forwards: bool) -> u32 {
    let hart = value & !0 << TARGET_HART;
    match forwards {
        true => hart | (value & TARGET_IDENTITY),
        false => hart | (value & TARGET_PRIORITY).max(1),
    }
}

/// `mmsiaddrcfgh`, whose lock bit is set and whose other fields therefore read as
/// nothing: this controller knows where the interrupt files are and software cannot
/// move them. The RISC-V Advanced Interrupt Architecture, 4.5.3.
const MMSIADDRCFG_LOCKED: u64 = MMSIADDRCFG + 4;
const LOCKED: u32 = 0x8000_0000;

/// The controller, shared between the two control regions that look at it.
#[derive(Debug, Clone)]
pub struct Aplic(Arc<Mutex<Controller>>);

impl Aplic {
    /// A controller for `harts` harts, forwarding whatever it forwards through `msi`.
    pub fn new(harts: usize, msi: Msi) -> Self {
        Self(Arc::new(Mutex::new(Controller::new(harts, msi))))
    }

    /// Wire `line` to interrupt source `source`.
    pub fn connect(&self, source: usize, line: Line) {
        assert!(
            source > 0 && source < SOURCES,
            "source {source} does not exist"
        );
        self.0.lock().unwrap().lines.push((source, line));
    }

    /// One privilege level's control region, as a device on the bus.
    pub fn domain(&self, level: Level) -> Region {
        Region {
            aplic: self.clone(),
            level,
        }
    }

    /// Where `level`'s control region goes.
    pub fn base(level: Level) -> u64 {
        match level {
            Level::Machine => MACHINE,
            Level::Supervisor => SUPERVISOR,
        }
    }
}

/// One domain's control region.
#[derive(Debug)]
pub struct Region {
    aplic: Aplic,
    level: Level,
}

impl Device for Region {
    /// Only aligned words are an access to this region. Anything else is reported as a
    /// fault, which is what the specification asks an implementation to prefer over
    /// ignoring it. The RISC-V Advanced Interrupt Architecture, 4.5.
    ///
    /// A read is not always a question here either: reading a claim register takes the
    /// interrupt it reports, so this ends by saying again what the domains assert.
    fn load(&mut self, offset: u64, size: u64) -> Result<u64, Exception> {
        if size != 32 || !offset.is_multiple_of(4) {
            return Err(Exception::LoadAccessFault(offset));
        }
        let mut controller = self.aplic.0.lock().unwrap();
        let value = controller.read(self.level, offset) as u64;
        controller.publish();
        Ok(value)
    }

    fn store(&mut self, offset: u64, size: u64, value: u64) -> Result<(), Exception> {
        if size != 32 || !offset.is_multiple_of(4) {
            return Err(Exception::StoreAmoAccessFault(offset));
        }
        let mut controller = self.aplic.0.lock().unwrap();
        controller.write(self.level, offset, value as u32);
        // A write can be what makes a source ready, and a domain that forwards sends
        // its messages as soon as one is rather than waiting to be asked again.
        controller.forward();
        controller.publish();
        Ok(())
    }

    /// Both domains publish into one word per hart, so either region names the same
    /// one.
    fn pending(&self) -> Option<Pending> {
        Some(self.aplic.0.lock().unwrap().pending.clone())
    }

    /// Look at the wires. Both regions are the same controller, so the second look of
    /// a round finds the edges of the first already taken.
    fn poll(&mut self) {
        let mut controller = self.aplic.0.lock().unwrap();
        controller.sample();
        controller.forward();
        controller.publish();
    }
}
