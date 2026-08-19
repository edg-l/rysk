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

// ------------------------------------------------------------------- zacas

#[test]
fn a_compare_and_swap_stores_only_when_the_value_matches() {
    let cpu = prog(&[sd(T1, T0, 0), amocas_d(T2, T3, T0), ld(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 5)
        .reg(T2, 5) // what it expects to find
        .reg(T3, 9) // what to put there instead
        .run();
    assert_eq!(cpu.reg(T2), 5, "rd takes what was there");
    assert_eq!(cpu.reg(T4), 9, "and the swap happened");

    let cpu = prog(&[sd(T1, T0, 0), amocas_d(T2, T3, T0), ld(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 5)
        .reg(T2, 4)
        .reg(T3, 9)
        .run();
    assert_eq!(cpu.reg(T2), 5, "rd still takes what was there");
    assert_eq!(cpu.reg(T4), 5, "but nothing was written");
}

#[test]
fn a_word_compare_and_swap_compares_the_low_half_and_sign_extends_what_it_read() {
    let cpu = prog(&[sw(T1, T0, 0), amocas_w(T2, T3, T0), lw(T4, T0, 0)])
        .reg(T0, SCRATCH)
        .reg(T1, 0xffff_ffff)
        // the high half differs, and a word compare-and-swap does not look at it
        .reg(T2, 0x1_ffff_ffff)
        .reg(T3, 7)
        .run();
    assert_eq!(cpu.reg(T2), !0, "the word it read, sign-extended");
    assert_eq!(cpu.reg(T4), 7, "and it matched, so the swap happened");
}

#[test]
fn a_quadword_compare_and_swap_is_a_register_pair_at_each_end() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        sd(T2, T0, 8),
        amocas_q(A0, A2, T0),
        ld(T3, T0, 0),
        ld(T4, T0, 8),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0x1111)
    .reg(T2, 0x2222)
    .reg(A0, 0x1111)
    .reg(A1, 0x2222)
    .reg(A2, 0xaaaa)
    .reg(A3, 0xbbbb)
    .run();
    assert_eq!(
        (cpu.reg(A0), cpu.reg(A1)),
        (0x1111, 0x2222),
        "both halves read"
    );
    assert_eq!(
        (cpu.reg(T3), cpu.reg(T4)),
        (0xaaaa, 0xbbbb),
        "and both written"
    );
}

#[test]
fn a_quadword_compare_and_swap_swaps_neither_half_unless_both_match() {
    let cpu = prog(&[
        sd(T1, T0, 0),
        sd(T2, T0, 8),
        amocas_q(A0, A2, T0),
        ld(T3, T0, 0),
        ld(T4, T0, 8),
    ])
    .reg(T0, SCRATCH)
    .reg(T1, 0x1111)
    .reg(T2, 0x2222)
    .reg(A0, 0x1111)
    .reg(A1, 0xdead) // only the high half differs
    .reg(A2, 0xaaaa)
    .reg(A3, 0xbbbb)
    .run();
    assert_eq!(
        (cpu.reg(T3), cpu.reg(T4)),
        (0x1111, 0x2222),
        "one half matching is not a match"
    );
}

#[test]
fn a_quadword_pair_at_x0_reads_as_zero_and_throws_the_result_away() {
    // x1 holds something, so reading the pair as x0 and x1 rather than as two zeroes
    // would compare against it and refuse the swap, and writing the result back would
    // destroy it.
    let cpu = prog(&[amocas_q(ZERO, A2, T0), ld(T3, T0, 0), ld(T4, T0, 8)])
        .reg(T0, SCRATCH)
        .reg(RA, 0xdead)
        .reg(A2, 0xaaaa)
        .reg(A3, 0xbbbb)
        .run();
    assert_eq!(
        (cpu.reg(T3), cpu.reg(T4)),
        (0xaaaa, 0xbbbb),
        "zeroed memory matched a comparison of zero, so the swap happened"
    );
    assert_eq!(cpu.reg(RA), 0xdead, "and nothing was written back over x1");
}

#[test]
fn a_quadword_compare_and_swap_names_even_registers_or_nothing() {
    for inst in [amocas_q(A1, A2, T0), amocas_q(A0, A3, T0)] {
        prog(&[inst])
            .reg(T0, SCRATCH)
            .expect(rysk::trap::Exception::IllegalInstruction(inst as u64));
    }
}

#[test]
fn a_compare_and_swap_has_to_be_aligned_to_its_own_width() {
    prog(&[amocas_q(A0, A2, T0)]).reg(T0, SCRATCH + 8).expect(
        rysk::trap::Exception::StoreAmoAddressMisaligned(SCRATCH + 8),
    );
    prog(&[amocas_d(T2, T3, T0)]).reg(T0, SCRATCH + 4).expect(
        rysk::trap::Exception::StoreAmoAddressMisaligned(SCRATCH + 4),
    );
}
