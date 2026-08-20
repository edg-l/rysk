//! The arithmetic, held against the host's.
//!
//! Round-to-nearest is the one mode a host also implements, so for that mode its
//! answers are a reference: anything rysk computes differently is rysk being wrong.
//! The modes a host cannot express are checked against what they mean instead.
//!
//! Round-to-nearest is also the mode `fpu` hands to the host itself now, so for the
//! values the fast path accepts, this holds the host against the host and proves only
//! that the handing over is faithful. What proves the two implementations agree is
//! `the_host_agrees_with_the_arithmetic_in_integers` in `fpu`, which has both of them
//! to call. The modes below are the ones only the software answers for, and they are
//! the reason this file is still the wider test of the two.

use rysk::fpu::{self, F32, F64, Round};

/// A spread of values chosen to land on the cases that break an implementation:
/// zeroes of both signs, subnormals, the ends of the range, and ordinary numbers.
fn doubles() -> Vec<f64> {
    let mut values = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        2.0,
        0.5,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::MIN_POSITIVE / 2.0,
        f64::from_bits(1),
        f64::from_bits(0x000f_ffff_ffff_ffff),
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        std::f64::consts::PI,
        -std::f64::consts::E,
        1e300,
        1e-300,
        1e16,
        1e17,
    ];
    // A deterministic scatter, so a failure is the same failure next time.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..300 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let value = f64::from_bits(state);
        if !value.is_nan() {
            values.push(value);
        }
    }
    values
}

fn floats() -> Vec<f32> {
    doubles().into_iter().map(|v| v as f32).collect()
}

#[test]
fn double_arithmetic_agrees_with_the_host() {
    for &a in &doubles() {
        for &b in &doubles() {
            for (name, ours, theirs) in [
                (
                    "add",
                    fpu::add(F64, a.to_bits(), b.to_bits(), Round::Nearest),
                    a + b,
                ),
                (
                    "sub",
                    fpu::sub(F64, a.to_bits(), b.to_bits(), Round::Nearest),
                    a - b,
                ),
                (
                    "mul",
                    fpu::mul(F64, a.to_bits(), b.to_bits(), Round::Nearest),
                    a * b,
                ),
                (
                    "div",
                    fpu::div(F64, a.to_bits(), b.to_bits(), Round::Nearest),
                    a / b,
                ),
            ] {
                let got = f64::from_bits(ours.0);
                assert!(
                    same(got, theirs),
                    "{a:e} {name} {b:e}: {got:e} ({:#018x}) not {theirs:e} ({:#018x})",
                    ours.0,
                    theirs.to_bits()
                );
            }
        }
    }
}

#[test]
fn single_arithmetic_agrees_with_the_host() {
    for &a in &floats() {
        for &b in &floats() {
            for (name, ours, theirs) in [
                (
                    "add",
                    fpu::add(F32, a.to_bits() as u64, b.to_bits() as u64, Round::Nearest),
                    a + b,
                ),
                (
                    "sub",
                    fpu::sub(F32, a.to_bits() as u64, b.to_bits() as u64, Round::Nearest),
                    a - b,
                ),
                (
                    "mul",
                    fpu::mul(F32, a.to_bits() as u64, b.to_bits() as u64, Round::Nearest),
                    a * b,
                ),
                (
                    "div",
                    fpu::div(F32, a.to_bits() as u64, b.to_bits() as u64, Round::Nearest),
                    a / b,
                ),
            ] {
                let got = f32::from_bits(ours.0 as u32);
                assert!(
                    same32(got, theirs),
                    "{a:e} {name} {b:e}: {got:e} ({:#010x}) not {theirs:e} ({:#010x})",
                    ours.0,
                    theirs.to_bits()
                );
            }
        }
    }
}

#[test]
fn square_root_and_fused_multiply_add_agree_with_the_host() {
    for &a in &doubles() {
        let ours = f64::from_bits(fpu::sqrt(F64, a.to_bits(), Round::Nearest).0);
        assert!(
            same(ours, a.sqrt()),
            "sqrt {a:e}: {ours:e} not {:e}",
            a.sqrt()
        );
        for &b in doubles().iter().take(40) {
            for &c in doubles().iter().take(40) {
                let ours = f64::from_bits(
                    fpu::fma(F64, a.to_bits(), b.to_bits(), c.to_bits(), Round::Nearest).0,
                );
                let theirs = a.mul_add(b, c);
                assert!(
                    same(ours, theirs),
                    "fma {a:e} {b:e} {c:e}: {ours:e} not {theirs:e}"
                );
            }
        }
    }
}

#[test]
fn a_nan_is_always_the_same_nan() {
    let nan = f64::NAN.to_bits();
    for (result, _) in [
        fpu::add(F64, nan, 1.0f64.to_bits(), Round::Nearest),
        fpu::mul(F64, nan, 0.0f64.to_bits(), Round::Nearest),
        fpu::div(F64, 0.0f64.to_bits(), 0.0f64.to_bits(), Round::Nearest),
        fpu::sqrt(F64, (-1.0f64).to_bits(), Round::Nearest),
    ] {
        assert_eq!(
            result,
            F64.canonical_nan(),
            "every nan this machine makes is the one it is allowed to make"
        );
    }
}

/// Two results are the same when their bits are, except that a NaN is only ever
/// compared for being one: rysk makes the canonical one and a host makes its own.
fn same(ours: f64, theirs: f64) -> bool {
    if theirs.is_nan() {
        return ours.is_nan();
    }
    ours.to_bits() == theirs.to_bits()
}

fn same32(ours: f32, theirs: f32) -> bool {
    if theirs.is_nan() {
        return ours.is_nan();
    }
    ours.to_bits() == theirs.to_bits()
}

#[test]
fn a_tie_goes_where_the_rounding_mode_says() {
    // Exactly halfway between 1.0 and the next double up, which is the case every
    // mode answers differently.
    let half_way = |sign: f64| (sign * 1.0f64).to_bits();
    let tiny = (2.0f64.powi(-53)).to_bits();
    let next = f64::from_bits(1.0f64.to_bits() + 1);

    for (mode, positive, negative) in [
        (Round::Nearest, 1.0, -1.0),
        (Round::NearestMax, next, -next),
        (Round::Zero, 1.0, -1.0),
        (Round::Down, 1.0, -next),
        (Round::Up, next, -1.0),
    ] {
        let (up, _) = fpu::add(F64, half_way(1.0), tiny, mode);
        assert_eq!(
            f64::from_bits(up),
            positive,
            "{mode:?} rounding a tie above one"
        );
        let (down, _) = fpu::sub(F64, half_way(-1.0), tiny, mode);
        assert_eq!(
            f64::from_bits(down),
            negative,
            "{mode:?} rounding a tie below minus one"
        );
    }
}

#[test]
fn the_flags_say_what_happened() {
    let bits = |v: f64| v.to_bits();
    for (name, (_, flags), expected) in [
        (
            "a division by zero is the one exception about the operands",
            fpu::div(F64, bits(1.0), bits(0.0), Round::Nearest),
            fpu::DZ,
        ),
        (
            "zero over zero has no answer at all",
            fpu::div(F64, bits(0.0), bits(0.0), Round::Nearest),
            fpu::NV,
        ),
        (
            "the root of a negative has none either",
            fpu::sqrt(F64, bits(-1.0), Round::Nearest),
            fpu::NV,
        ),
        (
            "past the top of the range is inexact as well as too large",
            fpu::mul(F64, bits(f64::MAX), bits(2.0), Round::Nearest),
            fpu::OF | fpu::NX,
        ),
        (
            "below the bottom of it is inexact as well as too small",
            fpu::mul(F64, bits(f64::MIN_POSITIVE), bits(0.5), Round::Nearest),
            0,
        ),
        (
            "and losing something on the way down is both",
            fpu::div(F64, bits(f64::from_bits(3)), bits(2.0), Round::Nearest),
            fpu::UF | fpu::NX,
        ),
        (
            "an exact result raises nothing",
            fpu::add(F64, bits(1.0), bits(1.0), Round::Nearest),
            0,
        ),
    ] {
        assert_eq!(flags, expected, "{name}");
    }
}

#[test]
fn what_a_number_is_matches_what_it_is_classified_as() {
    use rysk::fpu::*;
    for (value, class) in [
        (f64::NEG_INFINITY.to_bits(), CLASS_NEG_INF),
        ((-1.0f64).to_bits(), CLASS_NEG_NORMAL),
        (0x8000_0000_0000_0001, CLASS_NEG_SUBNORMAL),
        ((-0.0f64).to_bits(), CLASS_NEG_ZERO),
        (0.0f64.to_bits(), CLASS_POS_ZERO),
        (1, CLASS_POS_SUBNORMAL),
        (1.0f64.to_bits(), CLASS_POS_NORMAL),
        (f64::INFINITY.to_bits(), CLASS_POS_INF),
        (F64.canonical_nan(), CLASS_QUIET_NAN),
        // A NaN with the quiet bit clear is the signalling kind, and it needs a
        // payload: clearing that bit and leaving nothing behind is an infinity.
        (F64.canonical_nan() ^ (1 << 51) | 1, CLASS_SIGNALLING_NAN),
    ] {
        assert_eq!(classify(F64, value), class, "classifying {value:#018x}");
    }
}
