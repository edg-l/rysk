use crate::common::*;
use rysk::bus::DRAM_BASE;

// ------------------------------------------------------------------ control flow

/// Each branch is followed by an `addi` that only runs when it is not taken, and the
/// target is an `addi` past that. Taken leaves 2, not taken leaves 1.
fn branch_taken(inst: u32, lhs: u64, rhs: u64) -> bool {
    let cpu = prog(&[inst, addi(T2, ZERO, 1), jal(ZERO, 8), addi(T2, ZERO, 2)])
        .reg(T0, lhs)
        .reg(T1, rhs)
        .run();
    match cpu.reg(T2) {
        1 => false,
        2 => true,
        other => panic!("branch test landed somewhere unexpected: t2 = {other}"),
    }
}

#[test]
fn branches_compare_the_way_their_names_say() {
    let neg = (-1i64) as u64;
    assert!(branch_taken(beq(T0, T1, 12), 5, 5));
    assert!(!branch_taken(beq(T0, T1, 12), 5, 6));
    assert!(branch_taken(bne(T0, T1, 12), 5, 6));
    assert!(!branch_taken(bne(T0, T1, 12), 5, 5));

    assert!(branch_taken(blt(T0, T1, 12), neg, 1), "-1 < 1 signed");
    assert!(
        !branch_taken(bltu(T0, T1, 12), neg, 1),
        "-1 is huge unsigned"
    );
    assert!(branch_taken(bge(T0, T1, 12), 1, neg), "1 >= -1 signed");
    assert!(
        branch_taken(bgeu(T0, T1, 12), neg, 1),
        "-1 is huge unsigned"
    );
    assert!(branch_taken(bge(T0, T1, 12), 5, 5), "greater or equal");
    assert!(branch_taken(bgeu(T0, T1, 12), 5, 5));
}

#[test]
fn a_backward_branch_closes_a_loop() {
    // t1 counts down from 5 to 0, t2 accumulates the iterations.
    let cpu = prog(&[
        addi(T1, ZERO, 5),
        addi(T2, ZERO, 0),
        addi(T2, T2, 1),
        addi(T1, T1, -1),
        bne(T1, ZERO, -8),
    ])
    .run();
    assert_eq!(cpu.reg(T2), 5);
    assert_eq!(cpu.reg(T1), 0);
}

#[test]
fn jal_links_the_following_instruction() {
    let cpu = run(&[jal(RA, 8), addi(T0, ZERO, 1), addi(T1, ZERO, 2)]);
    assert_eq!(cpu.reg(RA), DRAM_BASE + 4, "the return address is pc + 4");
    assert_eq!(cpu.reg(T0), 0, "the skipped instruction did not run");
    assert_eq!(cpu.reg(T1), 2);
}

#[test]
fn jalr_reads_its_base_before_writing_the_link() {
    // `jalr ra, ra, off` is how a compiler calls through an auipc, so the link must not
    // clobber the base first.
    let cpu = prog(&[
        auipc(RA, 0),
        jalr(RA, RA, 12),
        addi(T1, ZERO, 1),
        addi(T2, ZERO, 2),
    ])
    .run();
    assert_eq!(cpu.reg(T1), 0, "the skipped instruction did not run");
    assert_eq!(cpu.reg(T2), 2);
    assert_eq!(
        cpu.reg(RA),
        DRAM_BASE + 8,
        "the link is the address after the jalr"
    );
}

#[test]
fn jalr_clears_the_low_bit_of_its_target() {
    // An odd target must be rounded down, not fetched at an odd address.
    let cpu = prog(&[jalr(RA, T0, 1), addi(T1, ZERO, 1), addi(T2, ZERO, 2)])
        .reg(T0, DRAM_BASE + 8)
        .run();
    assert_eq!(cpu.reg(T1), 0, "the skipped instruction did not run");
    assert_eq!(cpu.reg(T2), 2);
}
