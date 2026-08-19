use crate::common::*;

// ------------------------------------------------------------------ zalrsc

#[test]
fn a_store_conditional_succeeds_right_after_its_reservation() {
    let cpu = prog(&[
        sw(T1, T0, 0),
        lr_w(T2, ZERO, T0),
        sc_w(T3, T4, T0),
        lw(T5, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 7)
    .reg(T4, 9)
    .run();
    assert_eq!(cpu.reg(T2), 7, "the reserved load returns what was there");
    assert_eq!(cpu.reg(T3), 0, "zero means it succeeded");
    assert_eq!(cpu.reg(T5), 9, "and the store happened");
}

#[test]
fn an_intervening_store_of_the_same_value_still_breaks_the_reservation() {
    let cpu = prog(&[
        sw(T1, T0, 0),
        lr_w(T2, ZERO, T0),
        sw(T1, T0, 0), // the same value, so only a reservation can tell it happened
        sc_w(T3, T4, T0),
        lw(T5, T0, 0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 7)
    .reg(T4, 9)
    .run();
    assert_ne!(cpu.reg(T3), 0, "nonzero means it failed");
    assert_eq!(cpu.reg(T5), 7, "and nothing was written");
}

#[test]
fn a_store_conditional_without_a_reservation_fails() {
    let cpu = prog(&[sw(T1, T0, 0), sc_w(T3, T4, T0), lw(T5, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 7)
        .reg(T4, 9)
        .run();
    assert_ne!(cpu.reg(T3), 0);
    assert_eq!(cpu.reg(T5), 7);
}

#[test]
fn a_second_store_conditional_fails_because_the_first_released_the_reservation() {
    let cpu = prog(&[lr_w(T2, ZERO, T0), sc_w(T3, T4, T0), sc_w(T5, T4, T0)])
        .reg(T0, SCRATCH)
        .reg(T4, 9)
        .run();
    assert_eq!(cpu.reg(T3), 0);
    assert_ne!(cpu.reg(T5), 0);
}

#[test]
fn a_store_outside_the_reservation_leaves_it_alone() {
    let cpu = prog(&[lr_w(T2, ZERO, T0), sw(T1, T0, 64), sc_w(T3, T4, T0)])
        .reg(T0, SCRATCH)
        .reg(T1, 1)
        .reg(T4, 9)
        .run();
    assert_eq!(
        cpu.reg(T3),
        0,
        "a store 64 bytes away is not in the reservation set"
    );
}

#[test]
fn a_doubleword_reservation_covers_the_whole_doubleword() {
    let cpu = prog(&[
        lr_d(T2, ZERO, T0),
        sw(T1, T0, 4), // the upper half of the reserved doubleword
        sc_d(T3, T4, T0),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 1)
    .reg(T4, 9)
    .run();
    assert_ne!(cpu.reg(T3), 0);
}

// ------------------------------------------------------------------ zaamo

/// Every atomic memory operation, as (instruction, initial memory, operand, result).
#[test]
fn atomic_memory_operations_return_the_old_value_and_store_the_new() {
    let cases: &[(&str, u32, u64, u64, u64)] = &[
        ("amoswap.d", amoswap_d(T2, T1, T0), 5, 9, 9),
        ("amoadd.d", amoadd_d(T2, T1, T0), 5, 9, 14),
        ("amoxor.d", amoxor_d(T2, T1, T0), 0b1100, 0b1010, 0b0110),
        ("amoand.d", amoand_d(T2, T1, T0), 0b1100, 0b1010, 0b1000),
        ("amoor.d", amoor_d(T2, T1, T0), 0b1100, 0b1010, 0b1110),
        (
            "amomin.d",
            amomin_d(T2, T1, T0),
            (-1i64) as u64,
            1,
            (-1i64) as u64,
        ),
        ("amomax.d", amomax_d(T2, T1, T0), (-1i64) as u64, 1, 1),
        ("amominu.d", amominu_d(T2, T1, T0), (-1i64) as u64, 1, 1),
        (
            "amomaxu.d",
            amomaxu_d(T2, T1, T0),
            (-1i64) as u64,
            1,
            (-1i64) as u64,
        ),
    ];

    for &(name, inst, initial, operand, expected) in cases {
        let cpu = prog(&[sd(T3, T0, 0), inst, ld(T4, T0, 0)])
            .reg(T0, SCRATCH)
            .reg(T1, operand)
            .reg(T3, initial)
            .run();
        assert_eq!(cpu.reg(T2), initial, "{name} returns the old value");
        assert_eq!(cpu.reg(T4), expected, "{name} stores the new value");
    }
}

#[test]
fn word_atomics_sign_extend_what_they_return() {
    let cpu = prog(&[sw(T3, T0, 0), amoswap_w(T2, T1, T0), lwu(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 1)
        .reg(T3, 0xffff_ffff)
        .run();
    assert_eq!(
        cpu.reg(T2),
        u64::MAX,
        "the old word comes back sign-extended"
    );
    assert_eq!(cpu.reg(T4), 1);
}

#[test]
fn word_atomics_leave_the_neighbouring_word_alone() {
    let cpu = prog(&[sd(T3, T0, 0), amoswap_w(T2, T1, T0), ld(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 0xaaaa_aaaa)
        .reg(T3, 0x1111_1111_2222_2222)
        .run();
    assert_eq!(cpu.reg(T4), 0x1111_1111_aaaa_aaaa);
}
