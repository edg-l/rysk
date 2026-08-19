use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    csr::{MSTATUS, MSTATUS_MPP_SHIFT, MSTATUS_MPRV, MSTATUS_MXR, MSTATUS_SUM, Mode, SATP},
    trap::Exception,
};

// ---------------------------------------------------------------- sv39

/// Sv39 in `satp`'s mode field, over the page number of the root table.
fn sv39(root: u64) -> u64 {
    (8 << 60) | (root >> 12)
}

const V: u64 = 1 << 0;
const R: u64 = 1 << 1;
const W: u64 = 1 << 2;
const X: u64 = 1 << 3;
const U: u64 = 1 << 4;
const A: u64 = 1 << 6;
const D: u64 = 1 << 7;

/// A page table entry pointing at `pa` with `flags`.
fn pte(pa: u64, flags: u64) -> u64 {
    ((pa >> 12) << 10) | flags
}

/// Three pages of table, above anything a test program occupies.
const ROOT: u64 = DRAM_BASE + 0x2000;
const MID: u64 = DRAM_BASE + 0x3000;
const LEAF: u64 = DRAM_BASE + 0x4000;
/// The page the tests map, and the virtual address it is mapped at.
const FRAME: u64 = DRAM_BASE + 0x5000;
const VA: u64 = 0x1000;

/// Map the gigabyte that holds dram as itself, from one entry at the root. The
/// program, its stack and its own page tables are all in there, and a supervisor that
/// could not fetch its own instructions would fault before reaching the test.
fn identity(program: Program) -> Program {
    program.memory(ROOT + 2 * 8, pte(DRAM_BASE, V | R | W | X | A | D))
}

/// A machine in supervisor mode with `VA` mapped to `FRAME` through a three-level
/// walk, the leaf carrying `flags`. `t0` holds the virtual address.
fn mapped(code: &[u32], flags: u64) -> Program {
    identity(prog(code))
        .mode(Mode::Supervisor)
        .csr(SATP, sv39(ROOT))
        .memory(ROOT, pte(MID, V))
        .memory(MID, pte(LEAF, V))
        .memory(LEAF + 8, pte(FRAME, flags))
        .reg(T0, VA)
}

#[test]
fn a_mapped_page_is_reached_through_its_translation() {
    let cpu = mapped(&[sd(T1, T0, 0), ld(A0, T0, 0)], V | R | W | A | D)
        .reg(T1, 0xabcd)
        .run();
    assert_eq!(cpu.reg(A0), 0xabcd, "it round-tripped through the mapping");
    assert_eq!(
        cpu.load(FRAME, 8),
        0xabcd,
        "and landed in the physical page the table named, not at the virtual address"
    );
}

#[test]
fn the_page_offset_survives_translation() {
    let cpu = mapped(&[sd(T1, T0, 0x18), ld(A0, T0, 0x18)], V | R | W | A | D)
        .reg(T1, 7)
        .run();
    assert_eq!(cpu.reg(A0), 7);
    assert_eq!(
        cpu.load(FRAME + 0x18, 8),
        7,
        "at the same offset in the frame"
    );
}

#[test]
fn an_absent_page_faults_as_what_was_being_done_to_it() {
    mapped(&[ld(A0, T0, 0)], R | A).expect(Exception::LoadPageFault(VA));
    mapped(&[sd(T1, T0, 0)], R | A).expect(Exception::StoreAmoPageFault(VA));
    // A leaf that is not valid at all is the same answer.
    mapped(&[ld(A0, T0, 0)], R | W | A | D)
        .memory(LEAF + 8, 0)
        .expect(Exception::LoadPageFault(VA));
}

#[test]
fn a_page_without_the_permission_being_asked_for_faults() {
    mapped(&[sd(T1, T0, 0)], V | R | A | D).expect(Exception::StoreAmoPageFault(VA));
    mapped(&[ld(A0, T0, 0)], V | X | A).expect(Exception::LoadPageFault(VA));
}

#[test]
fn an_executable_page_is_readable_only_when_mxr_says_so() {
    mapped(&[ld(A0, T0, 0)], V | X | A).expect(Exception::LoadPageFault(VA));
    let cpu = mapped(&[ld(A0, T0, 0)], V | X | A)
        .csr(MSTATUS, 1 << MSTATUS_MXR)
        .run();
    assert_eq!(
        cpu.reg(A0),
        0,
        "it read the zeroed frame rather than faulting"
    );
}

#[test]
fn a_supervisor_reaches_a_user_page_only_when_sum_says_so() {
    mapped(&[ld(A0, T0, 0)], V | R | U | A).expect(Exception::LoadPageFault(VA));
    let cpu = mapped(&[ld(A0, T0, 0)], V | R | U | A)
        .csr(MSTATUS, 1 << MSTATUS_SUM)
        .run();
    assert_eq!(cpu.reg(A0), 0, "sum let it through");
}

#[test]
fn a_user_program_cannot_reach_a_supervisor_page() {
    // The program's own page has to be a user page for it to run at all; the page it
    // reaches for is not one.
    prog(&[ld(A0, T0, 0)])
        .mode(Mode::User)
        .csr(SATP, sv39(ROOT))
        .memory(ROOT + 2 * 8, pte(DRAM_BASE, V | R | W | X | U | A | D))
        .memory(ROOT, pte(MID, V))
        .memory(MID, pte(LEAF, V))
        .memory(LEAF + 8, pte(FRAME, V | R | A))
        .reg(T0, VA)
        .expect(Exception::LoadPageFault(VA));
}

#[test]
fn a_supervisor_never_executes_out_of_a_user_page_whatever_sum_says() {
    mapped(&[jalr(ZERO, T0, 0)], V | R | X | U | A)
        .csr(MSTATUS, 1 << MSTATUS_SUM)
        .memory(FRAME, addi(A0, ZERO, 1) as u64)
        .expect(Exception::InstructionPageFault(VA));
}

#[test]
fn the_accessed_and_dirty_bits_are_software_to_maintain() {
    // Reading a page that has never been touched faults so that software can record
    // that it now has been.
    mapped(&[ld(A0, T0, 0)], V | R | W).expect(Exception::LoadPageFault(VA));
    // Writing one that is accessed but clean faults for the same reason.
    mapped(&[sd(T1, T0, 0)], V | R | W | A).expect(Exception::StoreAmoPageFault(VA));
    // And reading it does not, since a read does not dirty anything.
    mapped(&[ld(A0, T0, 0)], V | R | W | A).run();
}

#[test]
fn a_virtual_address_has_to_be_sign_extended_into_the_bits_above_it() {
    // Bit 38 is clear here, so every bit above it has to be too.
    mapped(&[ld(A0, T0, 0)], V | R | W | A | D)
        .reg(T0, 1 << 40)
        .expect(Exception::LoadPageFault(1 << 40));
}

#[test]
fn a_superpage_maps_its_whole_range_from_one_entry() {
    // A leaf at the middle level covers two megabytes, so the address supplies the
    // bits the entry does not.
    let two_meg = DRAM_BASE + 0x20_0000;
    let cpu = identity(prog(&[sd(T1, T0, 0), ld(A0, T0, 0)]))
        .mode(Mode::Supervisor)
        .csr(SATP, sv39(ROOT))
        .memory(ROOT, pte(MID, V))
        .memory(MID + 8, pte(two_meg, V | R | W | A | D))
        .reg(T0, 0x20_1000)
        .reg(T1, 0x1234)
        .run();
    assert_eq!(cpu.reg(A0), 0x1234);
    assert_eq!(
        cpu.load(two_meg + 0x1000, 8),
        0x1234,
        "at the offset within it"
    );
}

#[test]
fn a_superpage_that_is_not_aligned_to_its_own_size_faults() {
    identity(prog(&[ld(A0, T0, 0)]))
        .mode(Mode::Supervisor)
        .csr(SATP, sv39(ROOT))
        .memory(ROOT, pte(MID, V))
        // a middle leaf whose frame is only four kilobytes aligned
        .memory(MID + 8, pte(DRAM_BASE + 0x20_1000, V | R | W | A | D))
        .reg(T0, 0x20_1000)
        .expect(Exception::LoadPageFault(0x20_1000));
}

#[test]
fn machine_mode_does_not_translate_unless_mprv_says_to() {
    // The same table, but the hart is in machine mode, so the address is physical and
    // reaches dram directly.
    let cpu = prog(&[sd(T1, T0, 0)])
        .csr(SATP, sv39(ROOT))
        .memory(ROOT, pte(MID, V))
        .reg(T0, SCRATCH)
        .reg(T1, 9)
        .run();
    assert_eq!(cpu.load(SCRATCH, 8), 9, "untranslated");

    // With MPRV set and MPP naming supervisor mode, the same store translates.
    prog(&[sd(T1, T0, 0)])
        .csr(SATP, sv39(ROOT))
        .csr(
            MSTATUS,
            (1 << MSTATUS_MPRV) | ((Mode::Supervisor as u64) << MSTATUS_MPP_SHIFT),
        )
        .memory(ROOT, 0)
        .reg(T0, SCRATCH)
        .expect(Exception::StoreAmoPageFault(SCRATCH));
}

#[test]
fn an_instruction_fetch_translates_too() {
    // The frame holds a single instruction, and the program counter reaches it through
    // the mapping rather than by its address.
    let cpu = mapped(&[jalr(ZERO, T0, 0)], V | R | X | A)
        .memory(FRAME, addi(A0, ZERO, 1) as u64)
        .run();
    assert_eq!(cpu.reg(A0), 1, "it fetched through the mapping");
}

#[test]
fn an_instruction_that_straddles_two_pages_is_fetched_through_both() {
    // The instruction begins in the last two bytes of one page and ends in the first
    // two of the next, and the two pages are mapped to frames that are not next to
    // each other. Taking the second half from beside the first would find the decoy.
    let next = FRAME + 0x2000;
    let decoy = FRAME + 0x1000;
    let cpu = mapped(&[jalr(ZERO, T0, 0)], V | R | X | A)
        .memory(LEAF + 16, pte(next, V | R | X | A))
        .memory(FRAME + 0xff8, (addi(A0, ZERO, 1) as u64 & 0xffff) << 48)
        .memory(next, (addi(A0, ZERO, 1) >> 16) as u64)
        .memory(decoy, (addi(A0, ZERO, 2) >> 16) as u64)
        .reg(T0, VA + 0xffe)
        .run();
    assert_eq!(
        cpu.reg(A0),
        1,
        "the half that is in the page it is mapped to"
    );
}

#[test]
fn a_page_that_may_not_be_executed_faults_on_the_fetch() {
    mapped(&[jalr(ZERO, T0, 0)], V | R | W | A | D)
        .memory(FRAME, addi(A0, ZERO, 1) as u64)
        .expect(Exception::InstructionPageFault(VA));
}

#[test]
fn satp_holds_only_the_translation_schemes_the_machine_has() {
    let cpu = prog(&[
        csrrw(ZERO, 0x180, T0),
        csrrs(A0, 0x180, ZERO),
        csrrw(ZERO, 0x180, T1),
        csrrs(A1, 0x180, ZERO),
    ])
    .reg(T0, 9 << 60) // sv48, which rysk does not have
    .reg(T1, sv39(ROOT))
    .run();
    assert_eq!(
        cpu.reg(A0),
        0,
        "the write of an unsupported mode was refused"
    );
    assert_eq!(cpu.reg(A1), sv39(ROOT), "and a supported one was taken");
}

#[test]
fn sfence_vma_is_a_supervisor_instruction() {
    let inst = sfence_vma(ZERO, ZERO);
    prog(&[inst])
        .mode(Mode::User)
        .expect(Exception::IllegalInstruction(inst as u64));
    prog(&[inst]).mode(Mode::Supervisor).run();
    prog(&[inst]).run();
}

#[test]
fn a_change_to_the_table_takes_effect_after_sfence_vma() {
    // The program remaps its own window from one frame to another and invalidates the
    // translation; the load after that has to see the new frame rather than whatever
    // the last walk found.
    let other = FRAME + 0x1000;
    let cpu = mapped(
        &[
            ld(A0, T0, 0),
            sd(T2, T1, 0),
            sfence_vma(ZERO, ZERO),
            ld(A1, T0, 0),
        ],
        V | R | W | A | D,
    )
    .reg(T1, LEAF + 8)
    .reg(T2, pte(other, V | R | W | A | D))
    .memory(FRAME, 1)
    .memory(other, 2)
    .run();
    assert_eq!(cpu.reg(A0), 1, "the frame it was mapped to");
    assert_eq!(cpu.reg(A1), 2, "and the one it was remapped to");
}
