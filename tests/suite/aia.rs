//! The advanced interrupt architecture: an interrupt file per hart, the window a hart
//! reaches its own through, and the controller that turns the wires still on this
//! machine into what those files receive.

use crate::common::*;
use rysk::{
    aplic::{self, Aplic},
    csr::{
        MEIP, MIE, MIP, MIREG, MISELECT, MTOPEI, MTOPI, SEIP, SIREG, SISELECT, SSIE, SSIP, STIP,
    },
    device::{Level, Line, Msi},
    imsic::{self, Imsic},
    trap::Exception,
};

/// The CSR numbers as an assembled instruction wants them.
const MISELECT_N: u32 = MISELECT as u32;
const MIREG_N: u32 = MIREG as u32;
const MTOPEI_N: u32 = MTOPEI as u32;
const MTOPI_N: u32 = MTOPI as u32;
const SISELECT_N: u32 = SISELECT as u32;
const SIREG_N: u32 = SIREG as u32;

/// The offsets of a domain's control region, from the specification's own table rather
/// than from the model, so the two are held against each other.
/// The RISC-V Advanced Interrupt Architecture, 4.5, table 6 and 4.8.1.
const DOMAINCFG: i32 = 0x0000;
const SETIPNUM: u64 = 0x1cdc;
const SETIE0: u64 = 0x1e00;
const TARGETS: u64 = 0x3000;
const IDC0: u64 = 0x4000;
const IDELIVERY: i32 = 0x00;
const ITHRESHOLD: i32 = 0x08;
const TOPI: i32 = 0x18;
const CLAIMI: i32 = 0x1c;

/// `domaincfg`: the global enable on its own, and with the bit that says a domain
/// forwards rather than delivers.
const DELIVERS: u64 = 1 << 8;
const FORWARDS: u64 = (1 << 8) | (1 << 2);

/// The source modes a test uses, and the bit that hands a source to the child domain.
const DETACHED: u64 = 1;
const LEVEL1: u64 = 6;
const DELEGATE: u64 = 1 << 10;

/// A wire nothing else on this machine uses.
const SOURCE: usize = 10;

// ------------------------------------------------------------ interrupt files

/// A machine whose hart has interrupt files, with `t0` at its machine-level page.
fn filed(code: &[u32]) -> Program {
    prog(code).imsic(Imsic::new(1)).reg(T0, imsic::MACHINE)
}

#[test]
fn a_message_sets_the_pending_bit_it_names() {
    let machine = filed(&[
        // The identity in `t1`, written to this hart's machine-level page.
        sw(T1, T0, 0),
        csrrw(ZERO, MISELECT_N, S0),
        csrrs(A0, MIREG_N, ZERO),
    ])
    .reg(T1, 7)
    .reg(S0, imsic::EIP0)
    .run();
    assert_eq!(machine.reg(A0), 1 << 7, "the identity written is pending");
}

#[test]
fn an_identity_the_file_does_not_implement_is_dropped() {
    let machine = filed(&[
        sw(T1, T0, 0),
        sw(T2, T0, 0),
        csrrw(ZERO, MISELECT_N, S0),
        csrrs(A0, MIREG_N, ZERO),
    ])
    // Zero is never an identity, and this file stops at 255.
    .reg(T1, 0)
    .reg(T2, imsic::IDENTITIES as u64 + 1)
    .reg(S0, imsic::EIP0)
    .run();
    assert_eq!(machine.reg(A0), 0, "neither of them exists to be set");
}

#[test]
fn a_message_lands_in_the_file_whose_page_it_was_written_to() {
    let imsic = Imsic::new(2);
    let machine = prog(&[sw(T1, T0, 0)])
        .harts(2)
        .imsic(imsic.clone())
        // Hart one's page, written by both harts, so what decides where it lands is
        // the address and not who wrote it.
        .reg(T0, imsic::MACHINE + imsic::PAGE)
        .reg(T1, 3)
        .run();
    let _ = machine;
    assert!(
        !imsic.signalling(0, Level::Machine),
        "hart zero's file is untouched"
    );
    assert_eq!(
        imsic.topei(1, Level::Machine),
        0,
        "and hart one's is pending but not enabled, so it offers nothing"
    );
    imsic.write(1, Level::Machine, imsic::EIE0, 1 << 3);
    assert_eq!(
        imsic.topei(1, Level::Machine),
        (3 << 16) | 3,
        "once enabled, the identity that arrived is the one on offer"
    );
}

#[test]
fn the_top_external_interrupt_is_the_lowest_identity_pending_and_enabled() {
    let imsic = Imsic::new(1);
    imsic.deliver(0, Level::Machine, 9);
    imsic.deliver(0, Level::Machine, 5);
    let machine = prog(&[
        csrrw(ZERO, MISELECT_N, S0),
        csrrw(ZERO, MIREG_N, S1),
        csrrs(A0, MTOPEI_N, ZERO),
    ])
    .imsic(imsic)
    .reg(S0, imsic::EIE0)
    .reg(S1, (1 << 9) | (1 << 5))
    .run();
    assert_eq!(
        machine.reg(A0),
        (5 << 16) | 5,
        "the lower identity, reported as both identity and priority"
    );
}

#[test]
fn an_identity_above_the_threshold_does_not_signal() {
    let imsic = Imsic::new(1);
    imsic.deliver(0, Level::Machine, 9);
    imsic.write(0, Level::Machine, imsic::EIE0, 1 << 9);
    imsic.write(0, Level::Machine, imsic::EITHRESHOLD, 9);
    assert_eq!(
        imsic.topei(0, Level::Machine),
        0,
        "the threshold is the first identity that does not count"
    );
    imsic.write(0, Level::Machine, imsic::EITHRESHOLD, 10);
    assert_eq!(imsic.topei(0, Level::Machine), (9 << 16) | 9);
}

#[test]
fn writing_the_top_external_interrupt_claims_what_it_reported() {
    let imsic = Imsic::new(1);
    for identity in [5, 9] {
        imsic.deliver(0, Level::Machine, identity);
    }
    let machine = prog(&[
        csrrw(ZERO, MISELECT_N, S0),
        csrrw(ZERO, MIREG_N, S1),
        // A read and a write in one instruction, which is the only safe way to claim:
        // what is cleared is what was read, not what was written.
        csrrw(A0, MTOPEI_N, ZERO),
        csrrw(A1, MTOPEI_N, ZERO),
        csrrs(A2, MTOPEI_N, ZERO),
    ])
    .imsic(imsic)
    .reg(S0, imsic::EIE0)
    .reg(S1, (1 << 9) | (1 << 5))
    .run();
    assert_eq!(machine.reg(A0), (5 << 16) | 5);
    assert_eq!(machine.reg(A1), (9 << 16) | 9, "the next one down");
    assert_eq!(machine.reg(A2), 0, "and then there are none");
}

#[test]
fn a_file_signals_its_hart_only_once_delivery_is_enabled() {
    let imsic = Imsic::new(1);
    imsic.deliver(0, Level::Machine, 4);
    imsic.write(0, Level::Machine, imsic::EIE0, 1 << 4);
    let machine = prog(&[
        csrrs(A0, MIP as u32, ZERO),
        csrrw(ZERO, MISELECT_N, S0),
        csrrw(ZERO, MIREG_N, S1),
        csrrs(A1, MIP as u32, ZERO),
    ])
    .imsic(imsic)
    .reg(S0, imsic::EIDELIVERY)
    .reg(S1, 1)
    .run();
    assert_eq!(machine.reg(A0) & MEIP, 0, "nothing is delivered yet");
    assert_eq!(machine.reg(A1) & MEIP, MEIP, "and now the wire is up");
}

#[test]
fn the_supervisor_window_reaches_the_supervisor_file() {
    let imsic = Imsic::new(1);
    imsic.deliver(0, Level::Supervisor, 6);
    let machine = prog(&[
        csrrw(ZERO, SISELECT_N, S0),
        csrrs(A0, SIREG_N, ZERO),
        csrrw(ZERO, MISELECT_N, S0),
        csrrs(A1, MIREG_N, ZERO),
    ])
    .imsic(imsic)
    .reg(S0, imsic::EIP0)
    .run();
    assert_eq!(machine.reg(A0), 1 << 6, "the supervisor file has it");
    assert_eq!(
        machine.reg(A1),
        0,
        "and the machine file is a different file"
    );
}

// -------------------------------------------------------------- the registers

#[test]
fn the_added_registers_do_not_exist_on_a_hart_with_no_interrupt_file() {
    for csr in [MISELECT_N, MIREG_N, MTOPEI_N, MTOPI_N] {
        let read = csrrs(A0, csr, ZERO);
        prog(&[read]).expect(Exception::IllegalInstruction(read as u64));
    }
}

#[test]
fn the_odd_half_of_an_array_does_not_exist_on_a_sixty_four_bit_machine() {
    // Each register of the arrays holds sixty-four identities here, so the odd
    // numbers name nothing and are refused rather than read as zero.
    let read = csrrs(A0, MIREG_N, ZERO);
    filed(&[csrrw(ZERO, MISELECT_N, S0), read])
        .reg(S0, imsic::EIP0 + 1)
        .expect(Exception::IllegalInstruction(read as u64));
}

#[test]
fn a_number_the_window_has_no_register_space_for_is_refused() {
    let read = csrrs(A0, MIREG_N, ZERO);
    filed(&[csrrw(ZERO, MISELECT_N, S0), read])
        .reg(S0, 0x50)
        .expect(Exception::IllegalInstruction(read as u64));
}

#[test]
fn a_reserved_number_inside_a_range_that_exists_reads_as_zero() {
    // 0x71 and 0x73 to 0x7f are reserved, and reading through them is not an error.
    let machine = filed(&[csrrw(ZERO, MISELECT_N, S0), csrrs(A0, MIREG_N, ZERO)])
        .reg(S0, imsic::EIDELIVERY + 1)
        .run();
    assert_eq!(machine.reg(A0), 0);
}

#[test]
fn the_priority_array_is_read_only_zero() {
    let machine = filed(&[
        csrrw(ZERO, MISELECT_N, S0),
        csrrw(ZERO, MIREG_N, S1),
        csrrs(A0, MIREG_N, ZERO),
    ])
    .reg(S0, 0x30)
    .reg(S1, !0)
    .run();
    assert_eq!(machine.reg(A0), 0, "nothing written to it stays");
}

#[test]
fn the_top_interrupt_is_the_highest_priority_one_pending_and_enabled() {
    let machine = filed(&[csrrs(A0, MTOPI_N, ZERO), csrrs(A1, MTOPI_N, ZERO)])
        .csr(MIP, SSIP | STIP)
        .csr(MIE, SSIE)
        .run();
    // Only the software interrupt is enabled, so only it is reported, and this machine
    // has one priority to report anything with.
    assert_eq!(machine.reg(A0), (1 << 16) | 1);
    assert_eq!(machine.reg(A1), (1 << 16) | 1, "reading it changes nothing");
}

#[test]
fn the_top_interrupt_is_nothing_when_nothing_is_both_pending_and_enabled() {
    let machine = filed(&[csrrs(A0, MTOPI_N, ZERO)])
        .csr(MIP, STIP)
        .csr(MIE, SSIE)
        .run();
    assert_eq!(machine.reg(A0), 0);
}

// -------------------------------------------------------------------- aplic

/// A machine with an APLIC whose `SOURCE` is wired to a line this test holds, and
/// which delivers rather than forwards. `t0` is the machine-level control region.
fn wired(code: &[u32], msi: Msi) -> (Program, Line) {
    let aplic = Aplic::new(1, msi);
    let line = Line::default();
    aplic.connect(SOURCE, line.clone());
    let program = prog(code)
        .device(
            aplic::MACHINE,
            aplic::SIZE,
            Box::new(aplic.domain(Level::Machine)),
        )
        .device(
            aplic::SUPERVISOR,
            aplic::SIZE,
            Box::new(aplic.domain(Level::Supervisor)),
        )
        .reg(T0, aplic::MACHINE);
    (program, line)
}

#[test]
fn a_delivering_domain_offers_the_source_its_target_names() {
    let (program, line) = wired(
        &[
            sw(T4, T0, DOMAINCFG),
            sw(T5, T0, 4 * SOURCE as i32),
            sw(T6, T2, 4 * SOURCE as i32),
            sw(A1, T1, 0),
            sw(A2, T3, IDELIVERY),
            csrrs(A4, MIP as u32, ZERO),
            lw(A0, T3, TOPI),
            lw(A3, T3, CLAIMI),
        ],
        Msi::default(),
    );
    line.set(true);
    let machine = program
        .reg(T1, aplic::MACHINE + SETIE0)
        .reg(T2, aplic::MACHINE + TARGETS)
        .reg(T3, aplic::MACHINE + IDC0)
        .reg(T4, DELIVERS)
        .reg(T5, LEVEL1)
        // Hart zero, and the highest priority there is.
        .reg(T6, 1)
        .reg(A1, 1 << SOURCE)
        .reg(A2, 1)
        .run();
    assert_eq!(
        machine.reg(A0),
        (SOURCE as u64) << 16 | 1,
        "the source, and the priority its target gave it"
    );
    assert_eq!(machine.reg(A3), machine.reg(A0), "claiming reads the same");
    assert_eq!(
        machine.reg(A4) & MEIP,
        MEIP,
        "and the domain is driving the hart's external interrupt"
    );
}

#[test]
fn a_source_above_the_threshold_is_not_offered() {
    let (program, line) = wired(
        &[
            sw(T4, T0, DOMAINCFG),
            sw(T5, T0, 4 * SOURCE as i32),
            sw(T6, T2, 4 * SOURCE as i32),
            sw(A1, T1, 0),
            sw(A2, T3, IDELIVERY),
            // A threshold of two lets a priority of one through and nothing worse.
            sw(A2, T3, ITHRESHOLD),
            lw(A0, T3, TOPI),
            sw(A5, T3, ITHRESHOLD),
            lw(A3, T3, TOPI),
        ],
        Msi::default(),
    );
    line.set(true);
    let machine = program
        .reg(T1, aplic::MACHINE + SETIE0)
        .reg(T2, aplic::MACHINE + TARGETS)
        .reg(T3, aplic::MACHINE + IDC0)
        .reg(T4, DELIVERS)
        .reg(T5, LEVEL1)
        .reg(T6, 4)
        .reg(A1, 1 << SOURCE)
        .reg(A2, 1)
        .reg(A5, 5)
        .run();
    assert_eq!(
        machine.reg(A0),
        0,
        "priority four is not below a threshold of one"
    );
    assert_eq!(
        machine.reg(A3),
        (SOURCE as u64) << 16 | 4,
        "and it is below a threshold of five"
    );
}

#[test]
fn a_forwarding_domain_posts_a_message_rather_than_driving_a_wire() {
    let imsic = Imsic::new(1);
    let files = imsic.clone();
    let msi = Msi::new(move |addr, identity| {
        let level = match addr < imsic::SUPERVISOR {
            true => Level::Machine,
            false => Level::Supervisor,
        };
        let base = Imsic::base(level);
        files.deliver(((addr - base) / imsic::PAGE) as usize, level, identity);
    });
    let (program, line) = wired(
        &[
            // The interrupt file, told to accept identity five and to deliver.
            csrrw(ZERO, MISELECT_N, S0),
            csrrw(ZERO, MIREG_N, S1),
            csrrw(ZERO, MISELECT_N, A4),
            csrrw(ZERO, MIREG_N, A2),
            // The domain, forwarding that source as identity five to hart zero.
            sw(T4, T0, DOMAINCFG),
            sw(T5, T0, 4 * SOURCE as i32),
            sw(T6, T2, 4 * SOURCE as i32),
            sw(A1, T1, 0),
            // What an interrupt service routine does before it exits, and what makes a
            // level-sensitive source that is still asserted say so again.
            sw(A5, T3, 0),
            csrrs(A0, MTOPEI_N, ZERO),
            csrrs(A3, MIP as u32, ZERO),
        ],
        msi,
    );
    line.set(true);
    let machine = program
        .imsic(imsic)
        .reg(T1, aplic::MACHINE + SETIE0)
        .reg(T2, aplic::MACHINE + TARGETS)
        .reg(T3, aplic::MACHINE + SETIPNUM)
        .reg(T4, FORWARDS)
        .reg(T5, LEVEL1)
        .reg(T6, 5)
        .reg(A1, 1 << SOURCE)
        .reg(A2, 1)
        .reg(A4, imsic::EIDELIVERY)
        .reg(A5, SOURCE as u64)
        .reg(S0, imsic::EIE0)
        .reg(S1, 1 << 5)
        .run();
    assert_eq!(
        machine.reg(A0),
        (5 << 16) | 5,
        "the identity the target register named arrived at the file"
    );
    assert_eq!(
        machine.reg(A3) & MEIP,
        MEIP,
        "and it is the file driving the hart, not the domain"
    );
}

#[test]
fn a_delegated_source_belongs_to_the_child_domain() {
    let (program, line) = wired(
        &[
            sw(T4, T0, DOMAINCFG),
            sw(T4, S1, DOMAINCFG),
            // Handed to the child, and configured there.
            sw(A5, T0, 4 * SOURCE as i32),
            sw(T5, S1, 4 * SOURCE as i32),
            sw(T6, T2, 4 * SOURCE as i32),
            sw(A1, T1, 0),
            sw(A2, T3, IDELIVERY),
            lw(A0, T3, TOPI),
            // And the machine-level domain has nothing: the source is not its any more.
            lw(A3, S0, TOPI),
            csrrs(A4, MIP as u32, ZERO),
        ],
        Msi::default(),
    );
    line.set(true);
    let machine = program
        .reg(T1, aplic::SUPERVISOR + SETIE0)
        .reg(T2, aplic::SUPERVISOR + TARGETS)
        .reg(T3, aplic::SUPERVISOR + IDC0)
        .reg(T4, DELIVERS)
        .reg(T5, LEVEL1)
        .reg(T6, 1)
        .reg(A1, 1 << SOURCE)
        .reg(A2, 1)
        .reg(A5, DELEGATE)
        .reg(S0, aplic::MACHINE + IDC0)
        .reg(S1, aplic::SUPERVISOR)
        .run();
    assert_eq!(machine.reg(A0), (SOURCE as u64) << 16 | 1);
    assert_eq!(machine.reg(A3), 0, "the parent gave it away");
    assert_eq!(
        machine.reg(A4) & (MEIP | SEIP),
        SEIP,
        "so it is the supervisor's interrupt and not the machine's"
    );
}

#[test]
fn a_detached_source_ignores_its_wire_and_takes_a_write() {
    let (program, line) = wired(
        &[
            sw(T4, T0, DOMAINCFG),
            sw(T5, T0, 4 * SOURCE as i32),
            sw(T6, T2, 4 * SOURCE as i32),
            sw(A1, T1, 0),
            sw(A2, T3, IDELIVERY),
            lw(A0, T3, TOPI),
            sw(A5, S0, 0),
            lw(A3, T3, TOPI),
        ],
        Msi::default(),
    );
    // The wire is high the whole time and means nothing to a detached source.
    line.set(true);
    let machine = program
        .reg(T1, aplic::MACHINE + SETIE0)
        .reg(T2, aplic::MACHINE + TARGETS)
        .reg(T3, aplic::MACHINE + IDC0)
        .reg(T4, DELIVERS)
        .reg(T5, DETACHED)
        .reg(T6, 1)
        .reg(A1, 1 << SOURCE)
        .reg(A2, 1)
        .reg(A5, SOURCE as u64)
        .reg(S0, aplic::MACHINE + SETIPNUM)
        .run();
    assert_eq!(machine.reg(A0), 0, "the wire is not this source's business");
    assert_eq!(
        machine.reg(A3),
        (SOURCE as u64) << 16 | 1,
        "and a write by number is"
    );
}

#[test]
fn a_message_wakes_the_hart_it_was_sent_to() {
    // Hart zero posts an identity to hart one's supervisor file, and hart one is
    // waiting for exactly that: this is how one hart interrupts another on a machine
    // whose harts are reached by message rather than by wire.
    let receiver = 3 * 4;
    let mut machine = prog(&[
        bne(A0, ZERO, receiver),
        sw(T1, T0, 0),
        wfi(),
        // Enable the identity, then delivery, then wait for it.
        csrrw(ZERO, SISELECT_N, S0),
        csrrw(ZERO, SIREG_N, S1),
        csrrw(ZERO, SISELECT_N, A3),
        csrrw(ZERO, SIREG_N, A2),
        wfi(),
        addi(A1, ZERO, 1),
        wfi(),
    ])
    .harts(2)
    .imsic(Imsic::new(2))
    .hart_reg(1, A0, 1)
    .reg(T0, imsic::SUPERVISOR + imsic::PAGE)
    .reg(T1, 1)
    .reg(S0, imsic::EIE0)
    .reg(S1, 1 << 1)
    .reg(A2, 1)
    .reg(A3, imsic::EIDELIVERY)
    .csr(MIE, SEIP)
    .parked();
    // A hart parked on a wait is let go by the scheduler on its next turn rather than
    // by the wait itself, so the round that parks it is not the round it leaves.
    machine.step(1);
    assert_eq!(
        machine.harts[1].regs[A1 as usize], 1,
        "hart one went past the wait, so the message reached it"
    );
}

#[test]
fn reading_the_external_interrupt_back_into_itself_does_not_latch_it() {
    // A read of `mip` gives the controller's signal and the bit machine mode owns
    // together, so a read-modify-write that wrote the read value back would leave that
    // bit set for good and the interrupt would repeat until the machine gave up.
    let imsic = Imsic::new(1);
    imsic.deliver(0, Level::Supervisor, 1);
    imsic.write(0, Level::Supervisor, imsic::EIE0, 1 << 1);
    imsic.write(0, Level::Supervisor, imsic::EIDELIVERY, 1);
    let machine = prog(&[
        csrrs(A0, MIP as u32, T0),
        // Then take the interrupt away at its source.
        csrrw(ZERO, SISELECT_N, S0),
        csrrw(ZERO, SIREG_N, ZERO),
        csrrs(A1, MIP as u32, ZERO),
    ])
    .imsic(imsic)
    .reg(T0, SSIP)
    .reg(S0, imsic::EIDELIVERY)
    .run();
    assert_eq!(machine.reg(A0) & SEIP, SEIP, "the signal was asserted");
    assert_eq!(
        machine.reg(A0) & SSIP,
        0,
        "and the write had not happened yet"
    );
    assert_eq!(
        machine.reg(A1) & SEIP,
        0,
        "and it goes away with the signal rather than staying behind"
    );
    assert_eq!(machine.reg(A1) & SSIP, SSIP, "while what was written stays");
}
