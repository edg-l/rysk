use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    clint::{self, Clint},
    csr::{
        MCAUSE, MEPC, MIDELEG, MIE, MIP, MSTATUS, MSTATUS_MIE, MSTATUS_SIE, MTIE, MTIP, MTVEC,
        Mode, SCAUSE, SSIE, SSIP, STVEC,
    },
    trap::{Exception, INTERRUPT, Interrupt, Trap},
};

// ------------------------------------------------------------- interrupts

/// Far enough ahead that the machine really has to wait for it, close enough that a
/// test waiting does not notice: a millisecond.
const SOON: u64 = clint::FREQUENCY / 1000;

/// The offset of the register that arms the timer.
const MTIMECMP: u64 = 0x4000;

/// What a handler does to let the run end: stop the interrupts and uninstall itself,
/// so whatever follows falls off the program into a trap with nowhere to go.
fn quiesce(machine: bool) -> [u32; 2] {
    if machine {
        [
            csrrw(ZERO, MIE as u32, ZERO),
            csrrw(ZERO, MTVEC as u32, ZERO),
        ]
    } else {
        [
            csrrw(ZERO, rysk::csr::SIE as u32, ZERO),
            csrrw(ZERO, STVEC as u32, ZERO),
        ]
    }
}

/// A machine with a timer, and `t0` pointing at the register that arms it.
fn timed(code: &[u32]) -> Program {
    prog(code)
        .device(clint::BASE, clint::SIZE, Box::new(Clint::default()))
        .reg(T0, clint::BASE + MTIMECMP)
}

#[test]
fn an_interrupt_that_is_pending_but_not_enabled_is_not_taken() {
    // With no vector installed, an interrupt that was taken would end the run as
    // itself rather than as the trap the program runs into.
    let cpu = prog(&[addi(A0, ZERO, 1)])
        .csr(MIP, SSIP)
        .csr(MSTATUS, 1 << MSTATUS_MIE)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A0), 1, "the program ran to its own end");
}

#[test]
fn an_interrupt_that_is_enabled_and_pending_enters_the_handler() {
    let [off, uninstall] = quiesce(true);
    let cpu = prog(&[addi(A0, ZERO, 1), off, uninstall, addi(A1, ZERO, 1)])
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .csr(MSTATUS, 1 << MSTATUS_MIE)
        .csr(MTVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A0), 0, "taken before the first instruction ran");
    assert_eq!(cpu.reg(A1), 1, "and the handler ran instead");
    assert_eq!(
        cpu.csrs[MCAUSE],
        INTERRUPT | Interrupt::SupervisorSoftware as u64,
        "mcause says interrupt, and which one"
    );
    assert_eq!(
        cpu.csrs[MEPC], DRAM_BASE,
        "at the instruction it interrupted"
    );
}

#[test]
fn a_machine_interrupt_is_not_held_off_by_a_less_privileged_mode() {
    // mstatus.MIE is clear, which only holds machine interrupts off in machine mode.
    let [off, uninstall] = quiesce(true);
    let cpu = prog(&[addi(A0, ZERO, 1), off, uninstall, addi(A1, ZERO, 1)])
        .mode(Mode::Supervisor)
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .csr(MTVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A1), 1, "the machine handler ran anyway");
    assert_eq!(cpu.mode, Mode::Machine);
}

#[test]
fn a_vectored_handler_spreads_the_interrupts_out() {
    let [off, uninstall] = quiesce(true);
    // The base is where an exception would enter; cause 1 is one entry along.
    let cpu = prog(&[addi(A1, ZERO, 1), off, uninstall, addi(A0, ZERO, 1)])
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .csr(MSTATUS, 1 << MSTATUS_MIE)
        .csr(MTVEC, DRAM_BASE | 1)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A0), 1, "the handler ran");
    assert_eq!(cpu.reg(A1), 0, "and it was not entered at the base");
}

#[test]
fn the_timer_interrupt_follows_mtime_past_mtimecmp() {
    let cpu = timed(&[sd(T1, T0, 0), csrrs(A0, MIP as u32, ZERO)])
        .reg(T1, 0)
        .run();
    assert_ne!(cpu.reg(A0) & MTIP, 0, "a deadline in the past is pending");

    let cpu = timed(&[sd(T1, T0, 0), csrrs(A0, MIP as u32, ZERO)])
        .reg(T1, u64::MAX)
        .run();
    assert_eq!(cpu.reg(A0) & MTIP, 0, "and one that never arrives is not");
}

#[test]
fn software_cannot_clear_a_pending_bit_a_device_is_driving() {
    let cpu = timed(&[
        sd(T1, T0, 0),
        csrrw(ZERO, MIP as u32, ZERO),
        csrrs(A0, MIP as u32, ZERO),
    ])
    .reg(T1, 0)
    .run();
    assert_ne!(cpu.reg(A0) & MTIP, 0, "a write cannot argue with a wire");
}

#[test]
fn a_timer_that_is_armed_and_enabled_eventually_fires() {
    let [off, uninstall] = quiesce(true);
    let cpu = timed(&[
        sd(T1, T0, 0),
        wfi(),
        addi(A1, ZERO, 1),
        // the handler pushes the deadline out of reach, which is the only way to clear
        // a timer interrupt, and then lets the run end
        sd(T2, T0, 0),
        off,
        uninstall,
        addi(A0, ZERO, 1),
    ])
    .reg(T1, SOON)
    .reg(T2, u64::MAX)
    .csr(MIE, MTIE)
    .csr(MSTATUS, 1 << MSTATUS_MIE)
    .csr(MTVEC, DRAM_BASE + 12)
    .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A0), 1, "the timer handler ran");
    assert_eq!(cpu.reg(A1), 0, "which had not run when the trap was taken");
    assert_eq!(cpu.csrs[MCAUSE], INTERRUPT | Interrupt::MachineTimer as u64);
    assert_eq!(
        cpu.csrs[MEPC],
        DRAM_BASE + 8,
        "wfi retires and the trap lands on the instruction after it, so returning from \
         the handler resumes past the wait"
    );
}

#[test]
fn wfi_retires_at_once_when_something_is_already_pending() {
    let cpu = prog(&[wfi(), addi(A0, ZERO, 1)])
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .run();
    assert_eq!(cpu.reg(A0), 1, "it did not stall");
}

#[test]
fn wfi_waits_for_an_interrupt_that_has_not_arrived_yet() {
    // mstatus.MIE is clear, so nothing is taken and no handler runs: what is left is
    // the wait itself, which ends when the timer says so.
    let cpu = timed(&[sd(T1, T0, 0), wfi(), csrrs(A0, MIP as u32, ZERO)])
        .reg(T1, SOON)
        .csr(MIE, MTIE)
        .run();
    assert_ne!(
        cpu.reg(A0) & MTIP,
        0,
        "it waited until the timer was pending"
    );
}

#[test]
fn an_interrupt_delegated_to_supervisor_mode_enters_stvec() {
    let [off, uninstall] = quiesce(false);
    let cpu = prog(&[addi(A0, ZERO, 1), off, uninstall, addi(A1, ZERO, 1)])
        .mode(Mode::Supervisor)
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .csr(MIDELEG, SSIP)
        .csr(MSTATUS, 1 << MSTATUS_SIE)
        .csr(STVEC, DRAM_BASE + 4)
        .expect(Exception::IllegalInstruction(0));
    assert_eq!(cpu.reg(A1), 1, "the supervisor handler ran");
    assert_eq!(cpu.mode, Mode::Supervisor, "and stayed in supervisor mode");
    assert_eq!(
        cpu.csrs[SCAUSE],
        INTERRUPT | Interrupt::SupervisorSoftware as u64
    );
    assert_eq!(cpu.csrs[MCAUSE], 0, "machine mode was never told");
}

#[test]
fn an_interrupt_with_nowhere_to_go_ends_the_run() {
    prog(&[nop()])
        .csr(MIP, SSIP)
        .csr(MIE, SSIE)
        .csr(MSTATUS, 1 << MSTATUS_MIE)
        .expect(Trap::Interrupt(Interrupt::SupervisorSoftware));
}
