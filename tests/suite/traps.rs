use crate::common::*;
use rysk::{bus::DRAM_BASE, csr::Mode, trap::Exception};

// ------------------------------------------------------------------ traps

const MSTATUS: u32 = 0x300;
const MTVEC: u32 = 0x305;
const MEPC: u32 = 0x341;
const MCAUSE: u32 = 0x342;
const MTVAL: u32 = 0x343;

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
fn a_jump_to_an_unaligned_address_traps() {
    // jalr clears bit 0 of its target but not bit 1.
    prog(&[jalr(ZERO, T0, 0)])
        .reg(T0, DRAM_BASE + 2)
        .expect(Exception::InstructionAddressMisaligned(DRAM_BASE + 2));
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
    let cpu = prog(&[
        csrrw(ZERO, MTVEC, T0),
        0xffff_ffff,
        nop(),
        // the handler uninstalls itself so the run ends at the zeroed word past it
        csrrw(ZERO, MTVEC, ZERO),
    ])
    .reg(T0, HANDLER)
    .expect(Exception::IllegalInstruction(0));

    assert_eq!(cpu.csrs[MCAUSE as usize], 2, "illegal instruction");
    assert_eq!(
        cpu.csrs[MEPC as usize],
        DRAM_BASE + 4,
        "the faulting address"
    );
    assert_eq!(
        cpu.csrs[MTVAL as usize], 0xffff_ffff,
        "the faulting encoding"
    );
}

#[test]
fn a_trap_enters_the_handler_and_mret_resumes_after_the_ecall() {
    const HANDLER: u64 = DRAM_BASE + 5 * 4;
    let cpu = prog(&[
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

    assert_eq!(cpu.reg(A1), 7, "the handler ran");
    assert_eq!(
        cpu.reg(A0),
        42,
        "and mret resumed at the instruction after the ecall"
    );
}

#[test]
fn a_trap_stacks_the_interrupt_enable_bit_and_mret_unstacks_it() {
    const HANDLER: u64 = DRAM_BASE + 4 * 4;
    let cpu = prog(&[
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

    assert_eq!((cpu.reg(A0) >> 3) & 1, 0, "MIE is cleared on entry");
    assert_eq!((cpu.reg(A0) >> 7) & 1, 1, "its old value moved into MPIE");
    assert_eq!(
        (cpu.reg(A0) >> 11) & 3,
        3,
        "MPP records the mode it trapped from"
    );
    assert_eq!(
        (cpu.csrs[MSTATUS as usize] >> 3) & 1,
        1,
        "mret restored MIE"
    );
}

#[test]
fn an_exception_enters_at_the_base_even_when_mtvec_is_vectored() {
    const HANDLER: u64 = DRAM_BASE + 3 * 4;
    let cpu = prog(&[
        csrrw(ZERO, MTVEC, T0),
        ecall(),
        nop(),
        // the base of the handler, which is where an exception must land
        addi(A0, ZERO, 9),
        csrrw(ZERO, MTVEC, ZERO),
    ])
    .reg(T0, HANDLER | 1) // vectored mode
    .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A0), 9);
}

#[test]
fn a_trap_with_no_handler_installed_ends_the_run() {
    // mtvec is zero, so there is nowhere to deliver to and run() hands the trap back
    // rather than looping on a handler that does not exist.
    let cpu = prog(&[ecall(), addi(A0, ZERO, 1)]).expect(Exception::EnvironmentCall(Mode::Machine));
    assert_eq!(cpu.reg(A0), 0, "nothing after the trap ran");
    assert_eq!(
        cpu.pc, DRAM_BASE,
        "and pc still points at the faulting instruction"
    );
}
