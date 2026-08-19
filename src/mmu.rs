//! Sv39: turning a virtual address into a physical one by walking a three-level page
//! table, and refusing when the table says to.
//!
//! The walk is the manual's algorithm rather than a paraphrase of it, because every
//! step of it is a case something eventually depends on.
//!
//! The RISC-V Instruction Set Manual Volume II, 12.3.2 and 12.4.

use crate::{
    cpu::Cpu,
    csr::{
        MSTATUS, MSTATUS_MPP, MSTATUS_MPP_SHIFT, MSTATUS_MPRV, MSTATUS_MXR, MSTATUS_SUM, Mode, SATP,
    },
    trap::Exception,
};

/// What an access is for. It decides which permission bit the page must carry and
/// which of the three page faults a refusal raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Fetch,
    Load,
    Store,
}

impl Access {
    fn fault(self, addr: u64) -> Exception {
        match self {
            Self::Fetch => Exception::InstructionPageFault(addr),
            Self::Load => Exception::LoadPageFault(addr),
            Self::Store => Exception::StoreAmoPageFault(addr),
        }
    }
}

/// `satp`'s mode field. Zero is bare, where a virtual address is a physical one, and
/// eight is Sv39. The wider schemes are the same walk with more levels and rysk does
/// not implement them, so it refuses to enter them rather than pretending.
/// The RISC-V Instruction Set Manual Volume II, 12.1.11, table 26.
const MODE: u64 = 0xf << 60;
const SV39: u64 = 8 << 60;

const LEVELS: u64 = 3;
const PAGE_BITS: u64 = 12;
const PAGE_SIZE: u64 = 1 << PAGE_BITS;

/// The bits of a page table entry. `RSW` at 9:8 is software's to use and hardware
/// never reads it. The RISC-V Instruction Set Manual Volume II, 12.3.1.
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;
/// Everything above the physical page number: seven reserved bits, `PBMT`, and the
/// `N` of Svnapot. rysk implements neither extension, so a page table that sets any of
/// them is asking for something this machine does not do.
const PTE_UNSUPPORTED: u64 = 0x7fc0_0000_0000_0000;

impl Cpu {
    /// The mode an access is checked against. A load or store takes the mode `MPP`
    /// names while `MPRV` is set, which is how machine-mode software reaches memory
    /// the way its supervisor would see it. A fetch is never affected.
    /// The RISC-V Instruction Set Manual Volume II, 3.1.6.3.
    #[inline]
    fn effective_mode(&self, access: Access) -> Mode {
        let status = self.csrs[MSTATUS];
        if access != Access::Fetch && (status >> MSTATUS_MPRV) & 1 == 1 {
            return Mode::from_bits((status & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT);
        }
        self.mode
    }

    /// Whether an access of this kind goes through a page table at all. In machine
    /// mode, or with no table installed, an address is already physical.
    #[inline]
    pub fn translating(&self, access: Access) -> bool {
        self.effective_mode(access) != Mode::Machine && self.csrs[SATP] & MODE != 0
    }

    /// Translate `va` for an access of `access`, or raise the page fault that says why
    /// not.
    ///
    /// The answer is usually that there is nothing to do, and that has to stay cheap:
    /// this is called for every fetch and every load and store, so the question is
    /// asked far more often than a page table is walked.
    #[inline]
    pub fn translate(&mut self, va: u64, access: Access) -> Result<u64, Exception> {
        if !self.translating(access) {
            return Ok(va);
        }
        self.walk(va, access)
    }

    fn walk(&mut self, va: u64, access: Access) -> Result<u64, Exception> {
        let mode = self.effective_mode(access);
        if self.csrs[SATP] & MODE != SV39 {
            return Err(access.fault(va));
        }
        // The top twenty-five bits are not part of the address: they have to repeat
        // its sign, which is what lets a supervisor tell its own half of the space
        // from a user's by the top bit alone.
        if va as i64 >> 38 != 0 && va as i64 >> 38 != -1 {
            return Err(access.fault(va));
        }

        let mut a = (self.csrs[SATP] & 0xfff_ffff_ffff) * PAGE_SIZE;
        for level in (0..LEVELS).rev() {
            let vpn = (va >> (PAGE_BITS + 9 * level)) & 0x1ff;
            // A fault reading the table is an access fault about the table, not a page
            // fault about the address that led there.
            let pte = self.bus.load(a + vpn * 8, 64)?;

            if pte & PTE_V == 0
                || (pte & PTE_R == 0 && pte & PTE_W != 0)
                || pte & PTE_UNSUPPORTED != 0
            {
                return Err(access.fault(va));
            }

            let ppn = (pte >> 10) & 0xfff_ffff_ffff;
            if pte & (PTE_R | PTE_X) == 0 {
                // A pointer to the next level down, and there is no level below zero.
                if level == 0 {
                    return Err(access.fault(va));
                }
                a = ppn * PAGE_SIZE;
                continue;
            }

            // A leaf. A superpage has to be aligned to its own size, so the bits of
            // the page number that the address supplies must be clear in the entry.
            if ppn & ((1 << (9 * level)) - 1) != 0 {
                return Err(access.fault(va));
            }
            self.permitted(pte, mode, access)
                .map_err(|()| access.fault(va))?;

            // rysk implements Svade: the accessed and dirty bits are software's to
            // maintain, and hardware faults rather than setting them.
            // The RISC-V Instruction Set Manual Volume II, 12.3.2, step 9.
            if pte & PTE_A == 0 || (access == Access::Store && pte & PTE_D == 0) {
                return Err(access.fault(va));
            }

            // The levels the entry did not translate are taken from the address, which
            // is what makes a superpage one page rather than many.
            let translated =
                (ppn >> (9 * level)) << (9 * level) | (va >> PAGE_BITS) & ((1 << (9 * level)) - 1);
            return Ok((translated * PAGE_SIZE) | (va & (PAGE_SIZE - 1)));
        }
        unreachable!("the walk returns or faults at every level")
    }

    /// Whether the page a leaf entry describes may be reached this way.
    /// The RISC-V Instruction Set Manual Volume II, 12.3.2, steps 6 and 8.
    fn permitted(&self, pte: u64, mode: Mode, access: Access) -> Result<(), ()> {
        let status = self.csrs[MSTATUS];
        let user = pte & PTE_U != 0;
        match mode {
            // A supervisor reaching a user page is usually a bug, so it is refused
            // unless `SUM` says otherwise, and always refused for a fetch: there is no
            // reason for supervisor code to be executing out of a user page.
            Mode::Supervisor if user => {
                if access == Access::Fetch || (status >> MSTATUS_SUM) & 1 == 0 {
                    return Err(());
                }
            }
            Mode::User if !user => return Err(()),
            _ => {}
        }
        let allowed = match access {
            Access::Fetch => pte & PTE_X != 0,
            // `MXR` lets a load read a page that is only executable, which is how
            // software reads its own instructions.
            Access::Load => {
                pte & PTE_R != 0 || (pte & PTE_X != 0 && (status >> MSTATUS_MXR) & 1 == 1)
            }
            Access::Store => pte & PTE_W != 0,
        };
        if allowed { Ok(()) } else { Err(()) }
    }
}
