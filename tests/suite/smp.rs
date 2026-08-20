//! More than one hart on one machine: what tells them apart, what one can do to
//! another, and what each keeps to itself.

use crate::common::*;
use rysk::{
    bus::DRAM_BASE,
    clint::{self, Clint},
    csr::{MCAUSE, MHARTID, MIE, MSIP, MSTATUS, MSTATUS_MIE, MTIP, MTVEC},
    machine::Machine,
    trap::{INTERRUPT, Interrupt},
};

/// `mtimecmp` for a hart, and `msip` for one, at the offsets from the clint's base
/// that the specification fixes.
/// RISC-V Advanced Core Local Interruptor Specification, 2.3 and 3.2.
fn mtimecmp(hart: u64) -> u64 {
    clint::BASE + 0x4000 + 8 * hart
}

fn msip(hart: u64) -> u64 {
    clint::BASE + 4 * hart
}

/// Far enough ahead that a hart really has to wait for it, close enough that waiting
/// is not noticeable: a millisecond.
const SOON: u64 = clint::FREQUENCY / 1000;

/// A program on `harts` harts with a clint they can all reach.
fn smp(code: &[u32], harts: usize) -> Program {
    prog(code)
        .harts(harts)
        .device(clint::BASE, clint::SIZE, Box::new(Clint::new(harts)))
}

#[test]
fn every_hart_reports_its_own_number() {
    // Each hart writes its own id where only it writes, so what is left in memory says
    // both that every hart ran and that no two of them thought they were the same one.
    // One is added so that hart zero's mark is not the zero the memory started as.
    let machine = prog(&[
        csrrs(T0, MHARTID as u32, ZERO),
        slli(T1, T0, 3),
        add(T1, T1, T2),
        addi(T3, T0, 1),
        sd(T3, T1, 0),
        wfi(),
    ])
    .harts(4)
    .reg(T2, SCRATCH)
    .parked();

    for hart in 0..4u64 {
        assert_eq!(
            machine.load(SCRATCH + hart * 8, 8),
            hart + 1,
            "hart {hart} left its mark"
        );
    }
}

#[test]
fn one_hart_interrupts_another_through_its_software_interrupt() {
    // The only way one hart reaches another, and what an operating system's inter
    // processor interrupt is underneath: hart zero writes hart one's msip, and hart
    // one is waiting for exactly that.
    let machine = smp(
        &[
            csrrs(T0, MHARTID as u32, ZERO),
            bne(T0, ZERO, 3 * 4),
            // hart 0: raise hart 1's software interrupt, then wait for one of its own
            // that nothing will ever send.
            sw(T2, T1, 0),
            wfi(),
            // hart 1: wait until hart 0 says something.
            wfi(),
            // the handler: clear the interrupt at its source, leave evidence, and stop
            // listening, so the wait below it is the last thing that happens.
            sw(ZERO, T1, 0),
            addi(A1, ZERO, 7),
            csrrw(ZERO, MIE as u32, ZERO),
            wfi(),
        ],
        2,
    )
    .reg(T1, msip(1))
    .reg(T2, 1)
    .csr(MIE, MSIP)
    .csr(MSTATUS, 1 << MSTATUS_MIE)
    .csr(MTVEC, DRAM_BASE + 5 * 4)
    .parked();

    assert_eq!(
        machine.harts[1].regs[A1 as usize], 7,
        "hart 1 woke and took the interrupt"
    );
    assert_eq!(
        machine.harts[1].csrs[MCAUSE],
        INTERRUPT | Interrupt::MachineSoftware as u64,
        "and it was a software interrupt that woke it"
    );
    assert_eq!(
        machine.harts[0].regs[A1 as usize], 0,
        "hart 0 raised it and never took one itself"
    );
}

#[test]
fn a_timer_belongs_to_the_hart_whose_deadline_it_compares() {
    // Hart zero arms hart one's deadline and none of its own. Only hart one has
    // anything to take, which is what having a `mtimecmp` each means.
    let machine = smp(
        &[
            csrrs(T0, MHARTID as u32, ZERO),
            bne(T0, ZERO, 3 * 4),
            // hart 0: arm the other hart's timer, then wait on a deadline it never set.
            sd(T2, T1, 0),
            wfi(),
            // hart 1: spin until the deadline arrives, since parking here would let the
            // run finish before the clock had moved.
            jal(ZERO, 0),
            // the handler, which is where hart 1 stops.
            addi(A1, A1, 1),
            csrrw(ZERO, MIE as u32, ZERO),
            wfi(),
        ],
        2,
    )
    .reg(T1, mtimecmp(1))
    .reg(T2, SOON)
    .csr(MIE, MTIP)
    .csr(MSTATUS, 1 << MSTATUS_MIE)
    .csr(MTVEC, DRAM_BASE + 5 * 4)
    .parked();

    assert_eq!(
        machine.harts[1].regs[A1 as usize], 1,
        "hart 1's deadline passed and hart 1 took the interrupt"
    );
    assert_eq!(
        machine.harts[0].regs[A1 as usize], 0,
        "hart 0 armed it and heard nothing, because the deadline was not its own"
    );
}

/// Hart zero reserves `SCRATCH` and stores to it a moment later; hart one stores to
/// `written` in between. Whether the store-conditional writes is the whole question.
fn contend(written: u64) -> Machine {
    prog(&[
        csrrs(T0, MHARTID as u32, ZERO),
        bne(T0, ZERO, 5 * 4),
        // hart 0
        lr_d(T4, ZERO, T1),
        nop(),
        sc_d(A1, T2, T1),
        wfi(),
        // hart 1
        nop(),
        sd(T3, T5, 0),
        wfi(),
    ])
    .harts(2)
    .reg(T1, SCRATCH)
    .reg(T2, 0xaaaa)
    .reg(T3, 0xbbbb)
    .reg(T5, written)
    .parked()
}

#[test]
fn a_store_from_another_hart_breaks_the_reservation() {
    let machine = contend(SCRATCH);
    assert_eq!(
        machine.harts[0].regs[A1 as usize], 1,
        "the store-conditional failed"
    );
    assert_eq!(
        machine.load(SCRATCH, 8),
        0xbbbb,
        "and did not write, so what is there is the other hart's"
    );
}

#[test]
fn a_store_from_another_hart_elsewhere_leaves_the_reservation_alone() {
    // The same program with the other hart writing the next doubleword instead, which
    // is the control: the failure above is the overlap and not the second hart.
    let machine = contend(SCRATCH + 8);
    assert_eq!(
        machine.harts[0].regs[A1 as usize], 0,
        "the store-conditional succeeded"
    );
    assert_eq!(machine.load(SCRATCH, 8), 0xaaaa, "and wrote what it held");
}

#[test]
fn each_hart_holds_a_reservation_of_its_own() {
    // Both harts reserve at once, each a doubleword nothing else touches, and both
    // store-conditionals write. With one reservation between them the second load
    // reserved would take the first hart's, and the first hart's store would fail
    // without anything having written what it reserved.
    let machine = prog(&[
        csrrs(T0, MHARTID as u32, ZERO),
        slli(T1, T0, 3),
        add(T1, T1, T5),
        lr_d(T4, ZERO, T1),
        nop(),
        sc_d(A1, T2, T1),
        wfi(),
    ])
    .harts(2)
    .reg(T2, 0xcccc)
    .reg(T5, SCRATCH)
    .parked();

    for hart in 0..2 {
        assert_eq!(
            machine.harts[hart].regs[A1 as usize], 0,
            "hart {hart}'s store-conditional wrote"
        );
        assert_eq!(machine.load(SCRATCH + hart as u64 * 8, 8), 0xcccc);
    }
}

/// How many times each hart goes round in the contended tests below. Large enough that
/// the harts really are inside one another rather than happening to take turns, small
/// enough that the test is not something anybody waits for.
const ROUNDS: u64 = 20_000;

/// The lock, and the counter it guards, in memory nothing else uses.
const LOCK: u64 = SCRATCH;
const COUNTER: u64 = SCRATCH + 8;

#[test]
fn two_harts_counting_behind_one_lock_lose_no_count() {
    // A spinlock built the way a guest builds one, out of `lr`/`sc`, with a read, an
    // add and a write inside it that are not atomic and do not need to be. Both harts
    // run the same program at the same time on host threads of their own.
    //
    // What this is really testing is the reservation. If a store from one hart can fail
    // to break the other's reservation, both leave the acquire believing they hold the
    // lock, both read the same count, and the total comes out short. Taking turns
    // cannot fail this way and so cannot test it.
    let machine = prog(&[
        // acquire:
        lr_d(T4, ZERO, T1),
        bne(T4, ZERO, -4),
        addi(T5, ZERO, 1),
        sc_d(T6, T5, T1),
        bne(T6, ZERO, -16),
        // the count, which the lock is what makes safe
        ld(T4, T2, 0),
        addi(T4, T4, 1),
        sd(T4, T2, 0),
        // release, and go round again
        sd(ZERO, T1, 0),
        addi(T3, T3, -1),
        bne(T3, ZERO, -40),
        wfi(),
    ])
    .harts(2)
    .reg(T1, LOCK)
    .reg(T2, COUNTER)
    .reg(T3, ROUNDS)
    .contended();

    assert_eq!(
        machine.load(COUNTER, 8),
        2 * ROUNDS,
        "every increment of both harts is in the total"
    );
    assert_eq!(machine.load(LOCK, 8), 0, "and the lock was given back");
}

#[test]
fn two_harts_adding_atomically_lose_no_count() {
    // The same total with no lock at all, which is the instruction on its own rather
    // than the reservation: an `amoadd` that is a load, an add and a store instead of
    // one indivisible operation loses every increment another hart lands inside.
    let machine = prog(&[
        amoadd_d(ZERO, T5, T2),
        addi(T3, T3, -1),
        bne(T3, ZERO, -8),
        wfi(),
    ])
    .harts(2)
    .reg(T2, COUNTER)
    .reg(T3, ROUNDS)
    .reg(T5, 1)
    .contended();

    assert_eq!(machine.load(COUNTER, 8), 2 * ROUNDS);
}

#[test]
fn a_hart_spinning_on_a_plain_load_sees_another_hart_write() {
    // The progress axiom, Volume I, 17.1: a hart spinning on an ordinary load has to
    // see a remote store eventually. Nothing in rysk asks the host for that, since
    // guest memory is read and written without `volatile`, so what holds it up is that
    // the address is worked out afresh from guest registers through calls that clobber
    // memory and the compiler cannot lift the load out of the interpreter's own loop.
    //
    // This test does not fail if that ever stops being true. It hangs.
    let machine = prog(&[
        csrrs(T0, MHARTID as u32, ZERO),
        bne(T0, ZERO, 16),
        // hart 0 waits for the word to change
        ld(T4, T1, 0),
        beq(T4, ZERO, -4),
        wfi(),
        // hart 1 changes it
        sd(T5, T1, 0),
        wfi(),
    ])
    .harts(2)
    .reg(T1, SCRATCH)
    .reg(T5, 0xd0d0)
    .contended();

    assert_eq!(machine.harts[0].regs[T4 as usize], 0xd0d0);
    assert_eq!(machine.load(SCRATCH, 8), 0xd0d0);
}
