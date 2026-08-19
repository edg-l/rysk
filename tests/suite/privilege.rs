use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    csr::{
        MCAUSE, MEDELEG, MEPC, MSTATUS, MSTATUS_MPIE, MSTATUS_MPP, MSTATUS_MPP_SHIFT, MSTATUS_SIE,
        MSTATUS_SPIE, MSTATUS_SPP, MTVAL, MTVEC, Mode, SCAUSE, SEPC, STVAL, STVEC,
    },
    exception::Exception,
};

// ------------------------------------------------------------- privilege

/// `mpp` holding `mode`, for preloading `mstatus`.
fn mpp(mode: Mode) -> u64 {
    (mode as u64) << MSTATUS_MPP_SHIFT
}

#[test]
fn mret_returns_to_the_mode_stacked_in_mpp() {
    for mode in [Mode::User, Mode::Supervisor, Mode::Machine] {
        let cpu = prog(&[mret(), addi(A0, ZERO, 1)])
            .csr(MSTATUS, mpp(mode) | (1 << MSTATUS_MPIE))
            .csr(MEPC, DRAM_BASE + 4)
            .run();
        assert_eq!(cpu.mode, mode, "mret entered the mode mpp named");
        assert_eq!(cpu.reg(A0), 1, "and carried on at mepc");
        assert_eq!(
            cpu.csrs[MSTATUS] & MSTATUS_MPP,
            0,
            "mpp is left holding the least privileged mode there is"
        );
    }
}

#[test]
fn sret_returns_to_the_mode_stacked_in_spp() {
    for (spp, mode) in [(0, Mode::User), (1, Mode::Supervisor)] {
        let cpu = prog(&[sret(), addi(A0, ZERO, 1)])
            .mode(Mode::Supervisor)
            .csr(MSTATUS, (spp << MSTATUS_SPP) | (1 << MSTATUS_SPIE))
            .csr(SEPC, DRAM_BASE + 4)
            .run();
        assert_eq!(cpu.mode, mode, "sret entered the mode spp named");
        assert_eq!(cpu.reg(A0), 1, "and carried on at sepc");
        assert_eq!(
            cpu.csrs[MSTATUS] & (1 << MSTATUS_SPP),
            0,
            "spp is left at user"
        );
        assert_ne!(
            cpu.csrs[MSTATUS] & (1 << MSTATUS_SIE),
            0,
            "spie moved into sie"
        );
        assert_ne!(
            cpu.csrs[MSTATUS] & (1 << MSTATUS_SPIE),
            0,
            "and spie is left set"
        );
    }
}

#[test]
fn an_xret_below_the_mode_it_returns_from_is_illegal() {
    for mode in [Mode::User, Mode::Supervisor] {
        prog(&[mret()])
            .mode(mode)
            .expect(Exception::IllegalInstruction(mret() as u64));
    }
    prog(&[sret()])
        .mode(Mode::User)
        .expect(Exception::IllegalInstruction(sret() as u64));
}

#[test]
fn ecall_raises_the_cause_that_names_the_mode_it_came_from() {
    for mode in [Mode::User, Mode::Supervisor, Mode::Machine] {
        let cpu = prog(&[ecall()])
            .mode(mode)
            .expect(Exception::EnvironmentCall(mode));
        assert_eq!(cpu.mode, mode, "nothing took the trap, so nothing moved");
    }
    assert_eq!(Exception::EnvironmentCall(Mode::User).cause(), 8);
    assert_eq!(Exception::EnvironmentCall(Mode::Supervisor).cause(), 9);
    assert_eq!(Exception::EnvironmentCall(Mode::Machine).cause(), 11);
}

#[test]
fn a_trap_records_the_mode_it_came_from_and_enters_machine_mode() {
    let cpu = prog(&[
        ecall(),
        // the handler uninstalls itself so the run ends at the zeroed word past it
        csrrw(ZERO, 0x305, ZERO),
        addi(A0, ZERO, 1),
    ])
    .mode(Mode::User)
    .csr(MTVEC, DRAM_BASE + 4)
    .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.mode, Mode::Machine, "the handler runs in machine mode");
    assert_eq!(
        cpu.csrs[MSTATUS] & MSTATUS_MPP,
        mpp(Mode::User),
        "mpp names the mode the ecall came from"
    );
    assert_eq!(cpu.csrs[MCAUSE], 8, "environment call from u-mode");
    assert_eq!(cpu.csrs[MEPC], DRAM_BASE, "at the ecall, not past it");
    assert_eq!(cpu.reg(A0), 1, "the handler ran");
}

#[test]
fn a_delegated_exception_enters_the_supervisor_handler() {
    // With mtvec left at zero, reaching the handler at all is proof stvec was used.
    let cpu = prog(&[ecall(), csrrw(ZERO, 0x105, ZERO), addi(A0, ZERO, 1)])
        .mode(Mode::User)
        .csr(MEDELEG, 1 << 8)
        .csr(STVEC, DRAM_BASE + 4)
        .csr(MSTATUS, 1 << MSTATUS_SIE)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.mode, Mode::Supervisor, "the handler runs in s-mode");
    assert_eq!(cpu.csrs[SCAUSE], 8, "scause took the cause");
    assert_eq!(cpu.csrs[SEPC], DRAM_BASE, "and sepc the address");
    assert_eq!(cpu.csrs[STVAL], 0, "an environment call owes no value");
    assert_eq!(cpu.csrs[MCAUSE], 0, "the machine registers are untouched");
    assert_eq!(cpu.csrs[MEPC], 0);
    assert_ne!(
        cpu.csrs[MSTATUS] & (1 << MSTATUS_SPIE),
        0,
        "sie moved to spie"
    );
    assert_eq!(cpu.csrs[MSTATUS] & (1 << MSTATUS_SIE), 0, "and sie cleared");
    assert_eq!(
        cpu.csrs[MSTATUS] & (1 << MSTATUS_SPP),
        0,
        "spp names u-mode"
    );
    assert_eq!(cpu.reg(A0), 1, "the supervisor handler ran");
}

#[test]
fn a_trap_in_machine_mode_is_never_delegated() {
    let cpu = prog(&[ecall(), csrrw(ZERO, 0x305, ZERO), addi(A0, ZERO, 1)])
        .csr(MEDELEG, !0)
        .csr(STVEC, DRAM_BASE + 0x800)
        .csr(MTVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.mode, Mode::Machine);
    assert_eq!(cpu.csrs[MCAUSE], 11, "environment call from m-mode");
    assert_eq!(cpu.csrs[SCAUSE], 0, "delegation cannot lower the mode");
}

#[test]
fn a_delegated_trap_with_no_supervisor_handler_ends_the_run() {
    // mtvec is installed, but the trap was delegated away from it.
    prog(&[ecall()])
        .mode(Mode::User)
        .csr(MEDELEG, 1 << 8)
        .csr(MTVEC, DRAM_BASE + 4)
        .expect(Exception::EnvironmentCall(Mode::User));
}

#[test]
fn a_csr_needing_more_privilege_than_the_hart_has_is_illegal() {
    let mstatus = csrrs(A0, 0x300, ZERO);
    for mode in [Mode::User, Mode::Supervisor] {
        prog(&[mstatus])
            .mode(mode)
            .expect(Exception::IllegalInstruction(mstatus as u64));
    }
    let sstatus = csrrs(A0, 0x100, ZERO);
    prog(&[sstatus])
        .mode(Mode::User)
        .expect(Exception::IllegalInstruction(sstatus as u64));
    // A supervisor may reach its own, and a machine may reach anything.
    prog(&[sstatus]).mode(Mode::Supervisor).run();
    prog(&[mstatus]).run();
}

#[test]
fn writing_a_read_only_csr_is_illegal_but_reading_one_is_not() {
    let write = csrrw(ZERO, 0xc00, T0);
    prog(&[write]).expect(Exception::IllegalInstruction(write as u64));
    let set = csrrs(A0, 0xc00, T0);
    prog(&[set])
        .reg(T0, 1)
        .expect(Exception::IllegalInstruction(set as u64));
    // Naming no bits to change is a read, and cycle is readable.
    let read = csrrs(A0, 0xc00, ZERO);
    assert_ne!(
        prog(&[read]).run().reg(A0),
        0,
        "the cycle counter is running"
    );
}

#[test]
fn mpp_cannot_be_left_holding_the_mode_that_does_not_exist() {
    let cpu = prog(&[csrrw(ZERO, 0x300, T0), csrrs(A0, 0x300, ZERO)])
        .reg(T0, 0b10 << MSTATUS_MPP_SHIFT)
        .csr(MSTATUS, mpp(Mode::Supervisor))
        .run();
    assert_eq!(
        cpu.reg(A0) & MSTATUS_MPP,
        mpp(Mode::Supervisor),
        "a write naming mode two leaves the field as it was"
    );
}

#[test]
fn a_fault_below_machine_mode_still_reports_its_value() {
    let cpu = prog(&[jalr(ZERO, T0, 0), csrrw(ZERO, 0x305, ZERO)])
        .mode(Mode::Supervisor)
        .reg(T0, 2)
        .csr(MTVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.csrs[MCAUSE], 0, "instruction address misaligned");
    assert_eq!(cpu.csrs[MTVAL], 2, "the address it tried to jump to");
    assert_eq!(cpu.csrs[MSTATUS] & MSTATUS_MPP, mpp(Mode::Supervisor));
}
