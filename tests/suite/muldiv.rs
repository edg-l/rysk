use crate::common::*;

// ------------------------------------------------------------------ rv64m

#[test]
fn mul_returns_the_low_half_and_mulh_the_high_half() {
    let machine = prog(&[
        mul(T2, T0, T1),
        mulh(T3, T0, T1),
        mulhu(T4, T0, T1),
        mulhsu(T5, T0, T1),
    ])
    .reg(T0, u64::MAX) // -1
    .reg(T1, 2)
    .run();
    assert_eq!(machine.reg(T2), (-2i64) as u64);
    assert_eq!(machine.reg(T3), u64::MAX, "-1 * 2 is negative");
    assert_eq!(machine.reg(T4), 1, "u64::MAX * 2 overflows by one");
    assert_eq!(machine.reg(T5), u64::MAX, "signed times unsigned");
}

#[test]
fn division_by_zero_gives_all_ones_and_the_dividend() {
    let machine = prog(&[
        div(T2, T0, T1),
        divu(T3, T0, T1),
        rem(T4, T0, T1),
        remu(T5, T0, T1),
    ])
    .reg(T0, 42)
    .reg(T1, 0)
    .run();
    assert_eq!(machine.reg(T2), u64::MAX);
    assert_eq!(machine.reg(T3), u64::MAX);
    assert_eq!(
        machine.reg(T4),
        42,
        "the remainder of division by zero is the dividend"
    );
    assert_eq!(machine.reg(T5), 42);
}

#[test]
fn word_division_by_zero_sign_extends_the_dividend() {
    let machine = prog(&[
        divw(T2, T0, T1),
        remw(T3, T0, T1),
        divuw(T4, T0, T1),
        remuw(T5, T0, T1),
    ])
    .reg(T0, 0xffff_ffff_8000_0000) // -2^31 as a word
    .reg(T1, 0)
    .run();
    assert_eq!(machine.reg(T2), u64::MAX);
    assert_eq!(machine.reg(T3), 0xffff_ffff_8000_0000);
    assert_eq!(machine.reg(T4), u64::MAX);
    assert_eq!(machine.reg(T5), 0xffff_ffff_8000_0000);
}

#[test]
fn signed_division_overflow_returns_the_dividend_and_no_remainder() {
    let machine = prog(&[div(T2, T0, T1), rem(T3, T0, T1)])
        .reg(T0, i64::MIN as u64)
        .reg(T1, u64::MAX) // -1
        .run();
    assert_eq!(machine.reg(T2), i64::MIN as u64);
    assert_eq!(machine.reg(T3), 0);
}

#[test]
fn word_division_overflow_returns_the_dividend_and_no_remainder() {
    let machine = prog(&[divw(T2, T0, T1), remw(T3, T0, T1)])
        .reg(T0, 0xffff_ffff_8000_0000)
        .reg(T1, u64::MAX)
        .run();
    assert_eq!(machine.reg(T2), 0xffff_ffff_8000_0000);
    assert_eq!(machine.reg(T3), 0);
}

#[test]
fn word_multiply_sign_extends_its_result() {
    let machine = prog(&[mulw(T2, T0, T1)])
        .reg(T0, 0x0001_0000)
        .reg(T1, 0x0001_0000)
        .run();
    assert_eq!(machine.reg(T2), 0, "the product's low 32 bits are zero");
}
