use crate::common::*;
use rysk::bus::DRAM_BASE;

// ------------------------------------------------------------------ loads and stores

#[test]
fn stores_and_loads_round_trip_every_width() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        ld(T2, T0, 0),
        sw(T1, T0, 16),
        lwu(T3, T0, 16),
        sh(T1, T0, 24),
        lhu(T4, T0, 24),
        sb(T1, T0, 32),
        lbu(T5, T0, 32),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0x8899_aabb_ccdd_eeff)
    .run();
    assert_eq!(cpu.reg(T2), 0x8899_aabb_ccdd_eeff);
    assert_eq!(cpu.reg(T3), 0xccdd_eeff);
    assert_eq!(cpu.reg(T4), 0xeeff);
    assert_eq!(cpu.reg(T5), 0xff);
}

#[test]
fn a_doubleword_keeps_its_top_bits() {
    // Storing and loading are wrong symmetrically if the byte lanes are misplaced, so
    // the value has to be one whose top bits are dropped rather than merely shuffled.
    let cpu = prog(&[sd(T1, T0, 0), ld(T2, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, u64::MAX)
        .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
}

#[test]
fn a_doubleword_lands_in_the_right_byte_lanes() {
    let cpu = prog(&[sd(T1, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 0x0807_0605_0403_0201)
        .run();
    for byte in 0..8u64 {
        assert_eq!(cpu.load(SCRATCH + byte, 1), byte + 1, "byte {byte}");
    }
}

#[test]
fn narrow_loads_sign_extend_and_their_unsigned_forms_do_not() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        lb(T2, T0, 0),
        lbu(T3, T0, 0),
        lh(T4, T0, 0),
        lhu(T5, T0, 0),
        lw(T6, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0xffff_ffff_ffff_ffff)
    .run();
    assert_eq!(cpu.reg(T2), u64::MAX);
    assert_eq!(cpu.reg(T3), 0xff);
    assert_eq!(cpu.reg(T4), u64::MAX);
    assert_eq!(cpu.reg(T5), 0xffff);
    assert_eq!(cpu.reg(T6), u64::MAX);
}

#[test]
fn a_negative_offset_addresses_below_the_base() {
    let cpu = prog(&[sd(T1, T0, -8), ld(T2, T0, -8)])
        .reg(T0, SCRATCH)
        .reg(T1, 0x1234)
        .run();
    assert_eq!(cpu.reg(T2), 0x1234);
    assert_eq!(cpu.load(SCRATCH - 8, 8), 0x1234);
}

// ------------------------------------------------------------------ instruction memory

#[test]
fn a_rewritten_instruction_takes_effect_after_fence_i() {
    // The first instruction runs, is overwritten with a different one, and is reached
    // again. RISC-V does not promise a store to instruction memory is visible to
    // fetch until the hart executes `fence.i`, so this is what that instruction has
    // to mean: The RISC-V Instruction Set Manual Volume I, 5.
    let cpu = prog(&[
        addi(A0, A0, 1),
        bne(A1, ZERO, 20),
        addi(A1, ZERO, 1),
        sw(T2, T1, 0),
        fence_i(),
        jalr(ZERO, T1, 0),
    ])
    .reg(T1, DRAM_BASE)
    .reg(T2, addi(A0, A0, 16) as u64)
    .run();
    assert_eq!(
        cpu.reg(A0),
        17,
        "the second pass ran the instruction that is there now, not the one that was"
    );
}
