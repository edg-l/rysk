//! The platform-level interrupt controller: what turns many device lines into the one
//! external-interrupt bit each privilege level has.
//!
//! It is strictly wired. A source raises a line, the controller decides which context
//! should hear about it, and a hart claims it and says when it is done. There is no
//! message-signalled path here and there cannot be: a posted write needs something on
//! the receiving end, which is what the Advanced Interrupt Architecture adds and this
//! does not have.
//!
//! RISC-V Platform-Level Interrupt Controller Specification 1.0.0.

use crate::{
    csr::{MEIP, SEIP},
    device::{Device, Line},
    trap::Exception,
};

pub const BASE: u64 = 0x0c00_0000;
pub const SIZE: u64 = 0x400_0000;

/// Source zero does not exist: the specification reserves it to mean "no interrupt",
/// which is what a claim returns when there is nothing to claim.
pub const SOURCES: usize = 1024;

/// One context per privilege level that can take an external interrupt. A machine with
/// more harts has more, in the same order.
/// RISC-V Platform-Level Interrupt Controller Specification, 1.1.
const CONTEXTS: usize = 2;
const MACHINE: usize = 0;
const SUPERVISOR: usize = 1;

const PRIORITY: u64 = 0x0000_0000;
const PENDING: u64 = 0x0000_1000;
const ENABLE: u64 = 0x0000_2000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT: u64 = 0x0020_0000;
const CONTEXT_STRIDE: u64 = 0x1000;
const THRESHOLD: u64 = 0x0;
const CLAIM: u64 = 0x4;

#[derive(Debug, Default)]
pub struct Plic {
    /// The line each source drives, if anything drives it.
    lines: Vec<(usize, Line)>,
    priority: Vec<u32>,
    enable: [Vec<u32>; CONTEXTS],
    threshold: [u32; CONTEXTS],
    /// Sources a context has claimed and not yet completed. The gateway stops offering
    /// one while it is being serviced, whatever its line is doing.
    /// RISC-V Platform-Level Interrupt Controller Specification, 1.2 and 9.
    claimed: Vec<bool>,
}

impl Plic {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            priority: vec![0; SOURCES],
            enable: [vec![0; SOURCES / 32], vec![0; SOURCES / 32]],
            threshold: [0; CONTEXTS],
            claimed: vec![false; SOURCES],
        }
    }

    /// Wire `line` to interrupt source `source`.
    pub fn connect(&mut self, source: usize, line: Line) {
        assert!(
            source > 0 && source < SOURCES,
            "source {source} does not exist"
        );
        self.lines.push((source, line));
    }

    /// Whether source `source` is asking for attention: its line is up and no context
    /// is already servicing it.
    fn pending(&self, source: usize) -> bool {
        !self.claimed[source]
            && self
                .lines
                .iter()
                .any(|(at, line)| *at == source && line.is_raised())
    }

    /// The source `context` should take: the highest priority one that is pending,
    /// enabled and above the threshold. Ties go to the lowest source number.
    ///
    /// This walks the lines that exist rather than the thousand-odd sources that could,
    /// because it is asked before every instruction and almost always answers nothing.
    ///
    /// RISC-V Platform-Level Interrupt Controller Specification, 4.
    fn best(&self, context: usize) -> Option<usize> {
        self.lines
            .iter()
            .map(|(source, _)| *source)
            .filter(|&source| {
                self.enable[context][source / 32] >> (source % 32) & 1 == 1
                    && self.priority[source] > self.threshold[context]
                    && self.pending(source)
            })
            .max_by_key(|&source| (self.priority[source], std::cmp::Reverse(source)))
    }

    /// Take the best source for `context`, which stops it being offered until the
    /// context says it is finished with it.
    fn claim(&mut self, context: usize) -> u64 {
        match self.best(context) {
            Some(source) => {
                self.claimed[source] = true;
                source as u64
            }
            None => 0,
        }
    }

    fn complete(&mut self, source: usize) {
        if source > 0 && source < SOURCES {
            self.claimed[source] = false;
        }
    }
}

impl Device for Plic {
    fn load(&mut self, offset: u64, _size: u64) -> Result<u64, Exception> {
        let word = (offset / 4) as usize;
        Ok(match offset {
            PRIORITY..PENDING => self.priority[word] as u64,
            PENDING..ENABLE => {
                let base = (offset - PENDING) as usize / 4 * 32;
                (0..32)
                    .filter(|bit| base + bit < SOURCES && self.pending(base + bit))
                    .fold(0u64, |bits, bit| bits | 1 << bit)
            }
            ENABLE..CONTEXT => {
                let context = ((offset - ENABLE) / ENABLE_STRIDE) as usize;
                let word = ((offset - ENABLE) % ENABLE_STRIDE) as usize / 4;
                match self.enable.get(context) {
                    Some(enable) => enable[word] as u64,
                    None => 0,
                }
            }
            _ => {
                let context = ((offset - CONTEXT) / CONTEXT_STRIDE) as usize;
                let register = (offset - CONTEXT) % CONTEXT_STRIDE;
                match (self.threshold.get(context), register) {
                    (Some(threshold), THRESHOLD) => *threshold as u64,
                    (Some(_), CLAIM) => self.claim(context),
                    _ => return Err(Exception::LoadAccessFault(offset)),
                }
            }
        })
    }

    fn store(&mut self, offset: u64, _size: u64, value: u64) -> Result<(), Exception> {
        let value = value as u32;
        match offset {
            // Source zero has no priority to set: it is not a source.
            PRIORITY..PENDING => {
                let source = (offset / 4) as usize;
                if source > 0 {
                    self.priority[source] = value;
                }
            }
            // Pending is what the gateways say, not what software says.
            PENDING..ENABLE => {}
            ENABLE..CONTEXT => {
                let context = ((offset - ENABLE) / ENABLE_STRIDE) as usize;
                let word = ((offset - ENABLE) % ENABLE_STRIDE) as usize / 4;
                if let Some(enable) = self.enable.get_mut(context) {
                    enable[word] = value;
                }
            }
            _ => {
                let context = ((offset - CONTEXT) / CONTEXT_STRIDE) as usize;
                let register = (offset - CONTEXT) % CONTEXT_STRIDE;
                match register {
                    THRESHOLD if context < CONTEXTS => self.threshold[context] = value,
                    CLAIM if context < CONTEXTS => self.complete(value as usize),
                    _ => return Err(Exception::StoreAmoAccessFault(offset)),
                }
            }
        }
        Ok(())
    }

    /// The external-interrupt bit of each privilege level is one wire out of this
    /// controller, asserted for as long as that level has something to claim.
    fn interrupts(&self, _hart: usize) -> u64 {
        let mut bits = 0;
        if self.best(MACHINE).is_some() {
            bits |= MEIP;
        }
        if self.best(SUPERVISOR).is_some() {
            bits |= SEIP;
        }
        bits
    }
}
