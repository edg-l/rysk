use crate::common::*;
use rysk::{bus::DRAM_BASE, csr::Mode, trap::Exception};

// ------------------------------------------------------------------ traps

const MSTATUS: u32 = 0x300;
const MTVEC: u32 = 0x305;
const MEPC: u32 = 0x341;
const MCAUSE: u32 = 0x342;
const MTVAL: u32 = 0x343;
const MEDELEG: usize = 0x302;
const STVEC: usize = 0x105;

#[test]
fn an_unknown_encoding_is_an_illegal_instruction() {
    prog(&[0xffff_ffff]).expect(Exception::IllegalInstruction(0xffff_ffff));
    // A program that runs off its own end meets the zeroed dram behind it.
    prog(&[addi(T0, ZERO, 1)]).expect(Exception::IllegalInstruction(0));
}

#[test]
fn ecall_and_ebreak_raise_rather_than_halt() {
    prog(&[ecall()]).expect(Exception::EnvironmentCall(Mode::Machine));
    prog(&[nop(), ebreak()]).expect(Exception::Breakpoint(DRAM_BASE + 4));
}

#[test]
fn running_off_dram_is_an_instruction_access_fault() {
    // A `ret` with a zero return address is how a bare program ends.
    prog(&[jalr(ZERO, ZERO, 0)]).expect(Exception::InstructionAccessFault(0));
}

#[test]
fn a_two_byte_aligned_jump_is_where_a_compressed_instruction_can_begin() {
    // With compressed instructions IALIGN is sixteen, so a target that is merely even
    // is a legal place for an instruction rather than a misaligned fetch.
    let machine = prog(&[jalr(ZERO, T0, 0), addi(A0, ZERO, 1)])
        .reg(T0, DRAM_BASE + 4)
        .run();
    assert_eq!(machine.reg(A0), 1, "it jumped there and carried on");
}

#[test]
fn a_misaligned_atomic_traps() {
    prog(&[lr_w(T2, ZERO, T0)])
        .reg(T0, SCRATCH + 1)
        .expect(Exception::StoreAmoAddressMisaligned(SCRATCH + 1));
    prog(&[amoadd_d(T2, T1, T0)])
        .reg(T0, SCRATCH + 4)
        .expect(Exception::StoreAmoAddressMisaligned(SCRATCH + 4));
}

#[test]
fn a_trap_records_where_and_why_it_happened() {
    const HANDLER: u64 = DRAM_BASE + 3 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        0xffff_ffff,
        nop(),
        // the handler uninstalls itself so the run ends at the zeroed word past it
        csrrw(ZERO, MTVEC, ZERO),
    ])
    .reg(T0, HANDLER)
    .expect(Exception::IllegalInstruction(0));

    assert_eq!(machine.csr(MCAUSE as usize), 2, "illegal instruction");
    assert_eq!(
        machine.csr(MEPC as usize),
        DRAM_BASE + 4,
        "the faulting address"
    );
    assert_eq!(
        machine.csr(MTVAL as usize),
        0xffff_ffff,
        "the faulting encoding"
    );
}

#[test]
fn a_trap_enters_the_handler_and_mret_resumes_after_the_ecall() {
    const HANDLER: u64 = DRAM_BASE + 5 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        ecall(),
        addi(A0, ZERO, 42),
        csrrw(ZERO, MTVEC, ZERO),
        jal(ZERO, 24),
        // handler
        addi(A1, ZERO, 7),
        csrrs(T1, MEPC, ZERO),
        addi(T1, T1, 4),
        csrrw(ZERO, MEPC, T1),
        mret(),
    ])
    .reg(T0, HANDLER)
    .expect(Exception::IllegalInstruction(0));

    assert_eq!(machine.reg(A1), 7, "the handler ran");
    assert_eq!(
        machine.reg(A0),
        42,
        "and mret resumed at the instruction after the ecall"
    );
}

#[test]
fn a_trap_stacks_the_interrupt_enable_bit_and_mret_unstacks_it() {
    const HANDLER: u64 = DRAM_BASE + 4 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        csrrs(ZERO, MSTATUS, T1), // set MIE
        ecall(),
        jal(ZERO, 28),
        // handler
        csrrs(A0, MSTATUS, ZERO), // mstatus as the handler sees it
        csrrs(T2, MEPC, ZERO),
        addi(T2, T2, 4),
        csrrw(ZERO, MEPC, T2),
        csrrw(ZERO, MTVEC, ZERO),
        mret(),
    ])
    .reg(T0, HANDLER)
    .reg(T1, 1 << 3)
    .expect(Exception::IllegalInstruction(0));

    assert_eq!((machine.reg(A0) >> 3) & 1, 0, "MIE is cleared on entry");
    assert_eq!(
        (machine.reg(A0) >> 7) & 1,
        1,
        "its old value moved into MPIE"
    );
    assert_eq!(
        (machine.reg(A0) >> 11) & 3,
        3,
        "MPP records the mode it trapped from"
    );
    assert_eq!(
        (machine.csr(MSTATUS as usize) >> 3) & 1,
        1,
        "mret restored MIE"
    );
}

#[test]
fn an_exception_enters_at_the_base_even_when_mtvec_is_vectored() {
    const HANDLER: u64 = DRAM_BASE + 3 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        ecall(),
        nop(),
        // the base of the handler, which is where an exception must land
        addi(A0, ZERO, 9),
        csrrw(ZERO, MTVEC, ZERO),
    ])
    .reg(T0, HANDLER | 1) // vectored mode
    .expect(Exception::IllegalInstruction(0));
    assert_eq!(machine.reg(A0), 9);
}

#[test]
fn a_trap_with_no_handler_installed_ends_the_run() {
    // mtvec is zero, so there is nowhere to deliver to and run() hands the trap back
    // rather than looping on a handler that does not exist.
    let machine =
        prog(&[ecall(), addi(A0, ZERO, 1)]).expect(Exception::EnvironmentCall(Mode::Machine));
    assert_eq!(machine.reg(A0), 0, "nothing after the trap ran");
    assert_eq!(
        machine.pc(),
        DRAM_BASE,
        "and pc still points at the faulting instruction"
    );
}

// ------------------------------------------------------------ the trap log

#[test]
fn a_trap_that_was_taken_is_remembered_with_where_it_went() {
    const HANDLER: u64 = DRAM_BASE + 5 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        ecall(),
        addi(A0, ZERO, 42),
        csrrw(ZERO, MTVEC, ZERO),
        jal(ZERO, 24),
        // handler
        addi(A1, ZERO, 7),
        csrrs(T1, MEPC, ZERO),
        addi(T1, T1, 4),
        csrrw(ZERO, MEPC, T1),
        mret(),
    ])
    .reg(T0, HANDLER)
    .expect(Exception::IllegalInstruction(0));

    let log = &machine.harts[0].traps;
    assert_eq!(log.taken(), 1, "one trap was taken and one was not");
    let taken = log.last().expect("it is remembered");
    assert_eq!(taken.trap, Exception::EnvironmentCall(Mode::Machine).into());
    assert_eq!(taken.pc, DRAM_BASE + 4, "the ecall it happened on");
    assert_eq!(taken.from, Mode::Machine);
    assert_eq!(taken.to, Mode::Machine, "nothing delegated it");
    assert_eq!(taken.handler, HANDLER, "and that is where control went");
    assert_eq!(taken.seq, 0, "it was the first");
}

#[test]
fn a_trap_nothing_was_installed_to_take_is_not_remembered_as_taken() {
    // It is the halt rather than an entry: the log says what a handler was entered
    // for, and nothing was entered. Reporting it here would be reporting a handler
    // that never ran.
    let machine = prog(&[ecall()]).expect(Exception::EnvironmentCall(Mode::Machine));
    let log = &machine.harts[0].traps;
    assert_eq!(log.taken(), 0);
    assert!(log.last().is_none());
}

#[test]
fn the_newest_traps_are_the_ones_kept() {
    // A handler that returns to the ecall that raised it, so the machine traps in a
    // loop until the counter in a1 runs out and the handler stops re-arming mtvec.
    const HANDLER: u64 = DRAM_BASE + 4 * 4;
    let machine = prog(&[
        csrrw(ZERO, MTVEC, T0),
        addi(A1, ZERO, 100),
        ecall(),
        jal(ZERO, 20),
        // handler: count down, uninstall at zero, and return to the ecall itself
        addi(A1, A1, -1),
        beq(A1, ZERO, 8),
        mret(),
        csrrw(ZERO, MTVEC, ZERO),
        mret(),
    ])
    .reg(T0, HANDLER)
    .expect(Exception::EnvironmentCall(Mode::Machine));

    let log = &machine.harts[0].traps;
    assert_eq!(log.taken(), 100, "every one of them was counted");
    let kept: Vec<_> = log.recent().collect();
    assert_eq!(kept.len(), 64, "and the last sixty-four of them kept");
    assert_eq!(kept[0].seq, 99, "newest first");
    assert_eq!(kept[63].seq, 36, "back to the oldest still here");
}

#[test]
fn the_log_says_which_mode_took_the_trap_and_which_raised_it() {
    // The one thing neither `mcause` nor `mepc` records: whether `medeleg` sent this
    // to a supervisor or the machine kept it.
    let machine = prog(&[ecall(), csrrw(ZERO, STVEC as u32, ZERO), addi(A0, ZERO, 1)])
        .mode(Mode::User)
        .csr(MEDELEG, 1 << 8)
        .csr(STVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));

    let taken = machine.harts[0].traps.last().expect("it was taken");
    assert_eq!(taken.trap, Exception::EnvironmentCall(Mode::User).into());
    assert_eq!(taken.from, Mode::User, "it was raised in u-mode");
    assert_eq!(taken.to, Mode::Supervisor, "and medeleg sent it to s-mode");
    assert_eq!(taken.handler, DRAM_BASE + 4, "which is stvec, not mtvec");
}
