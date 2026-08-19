//! Floating point, done rather than delegated.
//!
//! The host's arithmetic cannot answer for this machine's: it has no
//! round-to-nearest-ties-away, setting its rounding mode per instruction is a per-host
//! affair, and which NaN it invents is its own business. So the arithmetic is here, in
//! integers, and the answers are the same wherever rysk runs. QEMU and spike both do
//! this, for the same reasons.
//!
//! Every operation ends at [`round`], which is where inexactness, overflow and
//! underflow are decided once rather than in each of them.
//!
//! The RISC-V Instruction Set Manual Volume I, chapters 20 and 21.

/// A binary format, as its width and how many significand bits it has counting the one
/// the exponent implies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bits: u32,
    pub sig: u32,
}

pub const F32: Format = Format { bits: 32, sig: 24 };
pub const F64: Format = Format { bits: 64, sig: 53 };

impl Format {
    const fn exp_bits(self) -> u32 {
        self.bits - self.sig
    }

    const fn bias(self) -> i32 {
        (1 << (self.exp_bits() - 1)) - 1
    }

    /// The exponent of the smallest normal value.
    const fn min_exp(self) -> i32 {
        1 - self.bias()
    }

    /// The exponent that goes with the significand's lowest bit when the value is as
    /// small as this format goes, which is what a subnormal is measured against.
    const fn floor(self) -> i32 {
        self.min_exp() - (self.sig as i32 - 1)
    }

    const fn sign_bit(self) -> u64 {
        1 << (self.bits - 1)
    }

    const fn sig_mask(self) -> u64 {
        (1 << (self.sig - 1)) - 1
    }

    const fn exp_mask(self) -> u64 {
        ((1 << self.exp_bits()) - 1) << (self.sig - 1)
    }

    /// Set means quiet. The RISC-V Instruction Set Manual Volume I, 20.3.
    const fn quiet_bit(self) -> u64 {
        1 << (self.sig - 2)
    }

    /// The one NaN this machine invents, which is positive and quiet.
    pub const fn canonical_nan(self) -> u64 {
        self.exp_mask() | self.quiet_bit()
    }

    const fn infinity(self, sign: bool) -> u64 {
        self.exp_mask() | self.zero(sign)
    }

    const fn largest(self, sign: bool) -> u64 {
        (self.exp_mask() - (1 << (self.sig - 1))) | self.sig_mask() | self.zero(sign)
    }

    const fn zero(self, sign: bool) -> u64 {
        if sign { self.sign_bit() } else { 0 }
    }

    /// The bits of this format, with anything above it dropped.
    pub const fn trim(self, bits: u64) -> u64 {
        match self.bits {
            64 => bits,
            n => bits & ((1 << n) - 1),
        }
    }
}

/// How a result that is not exact is chosen.
/// The RISC-V Instruction Set Manual Volume I, 20.2, table 12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Round {
    Nearest,
    Zero,
    Down,
    Up,
    /// Ties away from zero, which no host offers.
    NearestMax,
}

impl Round {
    /// Three of the eight encodings are reserved, and an instruction naming one is
    /// illegal rather than rounded some other way.
    pub fn from_bits(bits: u64) -> Option<Self> {
        Some(match bits {
            0 => Self::Nearest,
            1 => Self::Zero,
            2 => Self::Down,
            3 => Self::Up,
            4 => Self::NearestMax,
            _ => return None,
        })
    }
}

/// The five exceptions, in the order `fflags` keeps them.
pub const NX: u64 = 1 << 0;
pub const UF: u64 = 1 << 1;
pub const OF: u64 = 1 << 2;
pub const DZ: u64 = 1 << 3;
pub const NV: u64 = 1 << 4;

/// What `fclass` reports, one bit each.
/// The RISC-V Instruction Set Manual Volume I, 20.9, table 15.
pub const CLASS_NEG_INF: u64 = 1 << 0;
pub const CLASS_NEG_NORMAL: u64 = 1 << 1;
pub const CLASS_NEG_SUBNORMAL: u64 = 1 << 2;
pub const CLASS_NEG_ZERO: u64 = 1 << 3;
pub const CLASS_POS_ZERO: u64 = 1 << 4;
pub const CLASS_POS_SUBNORMAL: u64 = 1 << 5;
pub const CLASS_POS_NORMAL: u64 = 1 << 6;
pub const CLASS_POS_INF: u64 = 1 << 7;
pub const CLASS_SIGNALLING_NAN: u64 = 1 << 8;
pub const CLASS_QUIET_NAN: u64 = 1 << 9;

/// A result, and what producing it raised.
pub type Outcome = (u64, u64);

/// A number taken apart. A finite one is `sig * 2^exp`, negated when `sign`: keeping
/// the exponent against the significand's lowest bit rather than its highest is what
/// lets every operation hand the same thing to the rounder.
#[derive(Debug, Clone, Copy)]
enum Number {
    Zero(bool),
    Infinite(bool),
    /// Carrying whether it was signalling.
    NotANumber(bool),
    Finite {
        sign: bool,
        exp: i32,
        sig: u128,
    },
}

impl Number {
    fn sign(self) -> bool {
        match self {
            Self::Zero(sign) | Self::Infinite(sign) | Self::Finite { sign, .. } => sign,
            Self::NotANumber(_) => false,
        }
    }

    fn is_nan(self) -> bool {
        matches!(self, Self::NotANumber(_))
    }

    /// A NaN that has to be reported as an invalid operation: a signalling one always,
    /// and a quiet one never.
    fn signalling(self) -> bool {
        matches!(self, Self::NotANumber(true))
    }
}

fn unpack(f: Format, bits: u64) -> Number {
    let bits = f.trim(bits);
    let sign = bits & f.sign_bit() != 0;
    let exp = ((bits & f.exp_mask()) >> (f.sig - 1)) as i32;
    let sig = bits & f.sig_mask();
    let top = (1 << f.exp_bits()) - 1;
    match (exp, sig) {
        (0, 0) => Number::Zero(sign),
        // A subnormal has no implied one, and its exponent is the smallest there is.
        (0, _) => Number::Finite {
            sign,
            exp: f.floor(),
            sig: sig as u128,
        },
        (e, 0) if e == top => Number::Infinite(sign),
        (e, _) if e == top => Number::NotANumber(sig & f.quiet_bit() == 0),
        (e, _) => Number::Finite {
            sign,
            exp: e - f.bias() - (f.sig as i32 - 1),
            sig: (sig | (1 << (f.sig - 1))) as u128,
        },
    }
}

/// Round `sig * 2^exp` into `f`, and say what that cost. `sticky` is whatever was
/// already discarded below the significand, which a division or a wide product knows
/// and this cannot see.
fn round(f: Format, sign: bool, mut exp: i32, mut sig: u128, sticky: bool, mode: Round) -> Outcome {
    if sig == 0 && !sticky {
        return (f.zero(sign), 0);
    }

    // Bring the significand to the width the format keeps, or fewer if the value is
    // too small to be normal: that is what a subnormal is, fewer bits rather than
    // fewer values.
    let width = 128 - sig.leading_zeros() as i32;
    let mut shift = width - f.sig as i32;
    if exp + shift < f.floor() {
        shift = f.floor() - exp;
    }

    let (mut half, mut rest) = (false, sticky);
    match shift {
        0 => {}
        s if s >= 128 => {
            rest |= sig != 0;
            sig = 0;
            exp += s;
        }
        s if s > 0 => {
            half = (sig >> (s - 1)) & 1 == 1;
            rest |= sig & ((1 << (s - 1)) - 1) != 0;
            sig >>= s;
            exp += s;
        }
        s => {
            sig <<= -s;
            exp += s;
        }
    }

    let inexact = half || rest;
    let up = match mode {
        Round::Nearest => half && (rest || sig & 1 == 1),
        Round::NearestMax => half,
        Round::Zero => false,
        Round::Down => inexact && sign,
        Round::Up => inexact && !sign,
    };
    if up {
        sig += 1;
        // Rounding can carry out of the significand and into the next binade.
        if sig >> f.sig != 0 {
            sig >>= 1;
            exp += 1;
        }
    }

    if sig == 0 {
        // Everything was rounded away. It is still a zero of the operation's sign.
        return (f.zero(sign), if inexact { NX | UF } else { 0 });
    }

    let normal = sig >> (f.sig - 1) != 0;
    let stored = if normal {
        exp + f.sig as i32 - 1 + f.bias()
    } else {
        0
    };
    if stored >= (1 << f.exp_bits()) - 1 {
        // Too large to represent. Which way it goes is the rounding mode's business,
        // and it is always inexact.
        let to_infinity = match mode {
            Round::Nearest | Round::NearestMax => true,
            Round::Zero => false,
            Round::Down => sign,
            Round::Up => !sign,
        };
        let value = if to_infinity {
            f.infinity(sign)
        } else {
            f.largest(sign)
        };
        return (value, OF | NX);
    }

    let packed = f.zero(sign) | ((stored as u64) << (f.sig - 1)) | (sig as u64 & f.sig_mask());
    // Underflow is a result that is both too small to be normal and not exact, which
    // is why it is decided here and after rounding rather than before.
    let flags = if inexact {
        NX | if normal { 0 } else { UF }
    } else {
        0
    };
    (packed, flags)
}

/// What every operation does first: if either operand is a NaN, that is the answer.
fn nan_result(f: Format, a: Number, b: Number) -> Option<Outcome> {
    if a.is_nan() || b.is_nan() {
        let invalid = a.signalling() || b.signalling();
        return Some((f.canonical_nan(), if invalid { NV } else { 0 }));
    }
    None
}

/// Line two finite numbers up on one exponent so they can be added.
///
/// Everything is kept when everything fits, which is what a fused multiply-add needs:
/// its product is twice as wide as a significand, and the bits that a cancellation
/// leaves behind are exactly the ones at the bottom of it. Only when the two are more
/// than the working width apart does anything fall off, and nothing can cancel across
/// a gap like that.
///
/// Each operand is told separately whether it lost anything, because in a subtraction
/// it matters which side the missing part was on.
fn align(a: (i32, u128), b: (i32, u128)) -> (i32, u128, bool, u128, bool) {
    let width = |sig: u128| 128 - sig.leading_zeros() as i32;
    let top = (a.0 + width(a.1)).max(b.0 + width(b.1));
    let exp = a.0.min(b.0).max(top - 127);
    let place = |x: (i32, u128)| -> (u128, bool) {
        match x.0 - exp {
            shift if shift >= 0 => (x.1 << shift, false),
            shift if -shift >= 128 => (0, x.1 != 0),
            shift => {
                let shift = (-shift) as u32;
                (x.1 >> shift, x.1 & ((1u128 << shift) - 1) != 0)
            }
        }
    };
    let ((ga, lost_a), (gb, lost_b)) = (place(a), place(b));
    (exp, ga, lost_a, gb, lost_b)
}

/// Add two finite numbers and hand the result to the rounder.
fn add_finite(f: Format, a: (bool, i32, u128), b: (bool, i32, u128), mode: Round) -> Outcome {
    let (exp, ga, lost_a, gb, lost_b) = align((a.1, a.2), (b.1, b.2));
    let lost = lost_a || lost_b;

    if a.0 == b.0 {
        return round(f, a.0, exp, ga + gb, lost, mode);
    }
    // Taking one away from the other. Whatever the alignment dropped is still part of
    // the number it came from, so it adds to that side of the difference: it makes the
    // winner larger, or the loser larger and so the difference smaller by a borrow.
    let (bigger, smaller, sign, kept) = match ga.cmp(&gb) {
        std::cmp::Ordering::Greater => (ga, gb, a.0, lost_a),
        std::cmp::Ordering::Less => (gb, ga, b.0, lost_b),
        // They cancel to nothing, and what is left is whatever was dropped, which
        // belongs to whichever side dropped it.
        std::cmp::Ordering::Equal if lost => {
            return round(f, if lost_a { a.0 } else { b.0 }, exp, 0, true, mode);
        }
        // Exactly nothing, which is a positive zero except when rounding down, where
        // the sign of a zero follows the way the rounding goes.
        std::cmp::Ordering::Equal => return (f.zero(mode == Round::Down), 0),
    };
    let borrow = (lost && !kept) as u128;
    round(f, sign, exp, bigger - smaller - borrow, lost, mode)
}

pub fn add(f: Format, a: u64, b: u64, mode: Round) -> Outcome {
    let (a, b) = (unpack(f, a), unpack(f, b));
    if let Some(nan) = nan_result(f, a, b) {
        return nan;
    }
    match (a, b) {
        // Infinities of opposite sign have no answer.
        (Number::Infinite(x), Number::Infinite(y)) if x != y => (f.canonical_nan(), NV),
        (Number::Infinite(sign), _) | (_, Number::Infinite(sign)) => (f.infinity(sign), 0),
        (Number::Zero(x), Number::Zero(y)) => {
            // Two zeroes keep their sign only when they agree, and rounding down makes
            // the disagreement negative.
            let sign = if x == y { x } else { mode == Round::Down };
            (f.zero(sign), 0)
        }
        (Number::Zero(_), other) | (other, Number::Zero(_)) => match other {
            Number::Finite { sign, exp, sig } => round(f, sign, exp, sig, false, mode),
            _ => unreachable!("the other cases are already answered"),
        },
        (
            Number::Finite {
                sign: sa,
                exp: ea,
                sig: ga,
            },
            Number::Finite {
                sign: sb,
                exp: eb,
                sig: gb,
            },
        ) => add_finite(f, (sa, ea, ga), (sb, eb, gb), mode),
        _ => unreachable!("nan is answered above"),
    }
}

/// Subtraction is addition with the second operand's sign turned over, which is exact
/// and so cannot be done any other way.
pub fn sub(f: Format, a: u64, b: u64, mode: Round) -> Outcome {
    if unpack(f, b).is_nan() {
        return add(f, a, b, mode);
    }
    add(f, a, f.trim(b) ^ f.sign_bit(), mode)
}

pub fn mul(f: Format, a: u64, b: u64, mode: Round) -> Outcome {
    let (a, b) = (unpack(f, a), unpack(f, b));
    if let Some(nan) = nan_result(f, a, b) {
        return nan;
    }
    let sign = a.sign() ^ b.sign();
    match (a, b) {
        // An infinity times a zero has no answer.
        (Number::Infinite(_), Number::Zero(_)) | (Number::Zero(_), Number::Infinite(_)) => {
            (f.canonical_nan(), NV)
        }
        (Number::Infinite(_), _) | (_, Number::Infinite(_)) => (f.infinity(sign), 0),
        (Number::Zero(_), _) | (_, Number::Zero(_)) => (f.zero(sign), 0),
        (
            Number::Finite {
                exp: ea, sig: ga, ..
            },
            Number::Finite {
                exp: eb, sig: gb, ..
            },
        ) => round(f, sign, ea + eb, ga * gb, false, mode),
        _ => unreachable!("nan is answered above"),
    }
}

pub fn div(f: Format, a: u64, b: u64, mode: Round) -> Outcome {
    let (a, b) = (unpack(f, a), unpack(f, b));
    if let Some(nan) = nan_result(f, a, b) {
        return nan;
    }
    let sign = a.sign() ^ b.sign();
    match (a, b) {
        (Number::Infinite(_), Number::Infinite(_)) | (Number::Zero(_), Number::Zero(_)) => {
            (f.canonical_nan(), NV)
        }
        (Number::Infinite(_), _) => (f.infinity(sign), 0),
        (_, Number::Infinite(_)) => (f.zero(sign), 0),
        // Anything finite over zero is an infinity, and saying so is the one exception
        // that is about the operands rather than the result.
        (_, Number::Zero(_)) => (f.infinity(sign), DZ),
        (Number::Zero(_), _) => (f.zero(sign), 0),
        (
            Number::Finite {
                exp: ea, sig: ga, ..
            },
            Number::Finite {
                exp: eb, sig: gb, ..
            },
        ) => {
            // Shift the dividend up so the quotient comes out with enough bits to
            // round by, and let the remainder be the sticky.
            let headroom = ga.leading_zeros() - 1;
            let (quotient, remainder) = ((ga << headroom) / gb, (ga << headroom) % gb);
            round(
                f,
                sign,
                ea - eb - headroom as i32,
                quotient,
                remainder != 0,
                mode,
            )
        }
        _ => unreachable!("nan is answered above"),
    }
}

pub fn sqrt(f: Format, a: u64, mode: Round) -> Outcome {
    let a = unpack(f, a);
    if a.is_nan() {
        return (f.canonical_nan(), if a.signalling() { NV } else { 0 });
    }
    match a {
        // The root of a negative is not a number, but the root of a negative zero is
        // that zero.
        Number::Zero(sign) => (f.zero(sign), 0),
        Number::Finite { sign: true, .. } | Number::Infinite(true) => (f.canonical_nan(), NV),
        Number::Infinite(false) => (f.infinity(false), 0),
        Number::Finite { exp, sig, .. } => {
            // Halving the exponent needs it even, and the significand shifted up as
            // far as it will go so the root has bits to spare.
            let mut exp = exp;
            let mut sig = sig;
            let headroom = (sig.leading_zeros() - 1) & !1;
            sig <<= headroom;
            exp -= headroom as i32;
            if exp % 2 != 0 {
                sig <<= 1;
                exp -= 1;
            }
            let root = integer_sqrt(sig);
            round(f, false, exp / 2, root, root * root != sig, mode)
        }
        Number::NotANumber(_) => unreachable!("answered above"),
    }
}

/// The largest integer whose square is no greater than `value`.
fn integer_sqrt(value: u128) -> u128 {
    if value == 0 {
        return 0;
    }
    // Start above the answer and come down; the iteration is monotone from there.
    let mut guess = 1u128 << ((128 - value.leading_zeros()).div_ceil(2));
    loop {
        let next = (guess + value / guess) / 2;
        if next >= guess {
            return guess;
        }
        guess = next;
    }
}

/// `a * b + c`, rounded once rather than twice, which is the whole point of it.
pub fn fma(f: Format, a: u64, b: u64, c: u64, mode: Round) -> Outcome {
    let (ua, ub, uc) = (unpack(f, a), unpack(f, b), unpack(f, c));
    if ua.is_nan() || ub.is_nan() || uc.is_nan() {
        let invalid = ua.signalling() || ub.signalling() || uc.signalling();
        return (f.canonical_nan(), if invalid { NV } else { 0 });
    }
    // The product is formed first, and its invalid cases are the multiplication's.
    let product_sign = ua.sign() ^ ub.sign();
    let product = match (ua, ub) {
        (Number::Infinite(_), Number::Zero(_)) | (Number::Zero(_), Number::Infinite(_)) => {
            return (f.canonical_nan(), NV);
        }
        (Number::Infinite(_), _) | (_, Number::Infinite(_)) => {
            // An infinite product and an addend that disagrees have no answer.
            return match uc {
                Number::Infinite(sign) if sign != product_sign => (f.canonical_nan(), NV),
                _ => (f.infinity(product_sign), 0),
            };
        }
        (Number::Zero(_), _) | (_, Number::Zero(_)) => None,
        (
            Number::Finite {
                exp: ea, sig: ga, ..
            },
            Number::Finite {
                exp: eb, sig: gb, ..
            },
        ) => Some((ea + eb, ga * gb)),
        _ => unreachable!("nan is answered above"),
    };

    match (product, uc) {
        (None, Number::Zero(sign)) => {
            // Both a zero: they keep a sign only when they agree.
            let sign = if sign == product_sign {
                sign
            } else {
                mode == Round::Down
            };
            (f.zero(sign), 0)
        }
        (None, Number::Infinite(sign)) => (f.infinity(sign), 0),
        (None, Number::Finite { sign, exp, sig }) => round(f, sign, exp, sig, false, mode),
        (Some((exp, sig)), Number::Zero(_)) => round(f, product_sign, exp, sig, false, mode),
        (Some(_), Number::Infinite(sign)) => (f.infinity(sign), 0),
        (
            Some((exp, sig)),
            Number::Finite {
                sign,
                exp: ec,
                sig: gc,
            },
        ) => add_finite(f, (product_sign, exp, sig), (sign, ec, gc), mode),
        (_, Number::NotANumber(_)) => unreachable!("answered above"),
    }
}

/// How two numbers compare, or nothing when one of them is a NaN and so the question
/// has no answer.
fn compare(f: Format, a: u64, b: u64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    let (ua, ub) = (unpack(f, a), unpack(f, b));
    if ua.is_nan() || ub.is_nan() {
        return None;
    }
    let key = |n: Number| -> (bool, i32, u128) {
        match n {
            Number::Zero(sign) => (sign, i32::MIN, 0),
            Number::Infinite(sign) => (sign, i32::MAX, 0),
            Number::Finite { sign, exp, sig } => (sign, exp, sig),
            Number::NotANumber(_) => unreachable!("answered above"),
        }
    };
    // Two zeroes are equal whichever signs they carry.
    if matches!(ua, Number::Zero(_)) && matches!(ub, Number::Zero(_)) {
        return Some(Ordering::Equal);
    }
    let (sa, ea, ga) = key(ua);
    let (sb, eb, gb) = key(ub);
    if sa != sb {
        return Some(if sa {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    // Within one sign a larger exponent is a larger magnitude and the significand
    // breaks the tie, which holds for subnormals too since they all share the smallest
    // exponent there is.
    let magnitude = (ea, ga).cmp(&(eb, gb));
    Some(if sa { magnitude.reverse() } else { magnitude })
}

/// Equality, which is quiet: only a signalling NaN is an invalid operation.
pub fn eq(f: Format, a: u64, b: u64) -> Outcome {
    let (ua, ub) = (unpack(f, a), unpack(f, b));
    let invalid = ua.signalling() || ub.signalling();
    match compare(f, a, b) {
        Some(std::cmp::Ordering::Equal) => (1, if invalid { NV } else { 0 }),
        Some(_) => (0, if invalid { NV } else { 0 }),
        None => (0, if invalid { NV } else { 0 }),
    }
}

/// Ordering, which signals: any NaN at all is an invalid operation, because an
/// unordered result is not something a program asking for less-than can act on.
pub fn less(f: Format, a: u64, b: u64, or_equal: bool) -> Outcome {
    use std::cmp::Ordering;
    match compare(f, a, b) {
        Some(Ordering::Less) => (1, 0),
        Some(Ordering::Equal) => (or_equal as u64, 0),
        Some(Ordering::Greater) => (0, 0),
        None => (0, NV),
    }
}

/// The smaller or larger of two, where a NaN is not an answer but is not fatal either:
/// if only one operand is a NaN the other one is the result.
/// The RISC-V Instruction Set Manual Volume I, 20.6.
pub fn min_max(f: Format, a: u64, b: u64, want_max: bool) -> Outcome {
    use std::cmp::Ordering;
    let (ua, ub) = (unpack(f, a), unpack(f, b));
    let invalid = if ua.signalling() || ub.signalling() {
        NV
    } else {
        0
    };
    match (ua.is_nan(), ub.is_nan()) {
        (true, true) => return (f.canonical_nan(), invalid),
        (true, false) => return (f.trim(b), invalid),
        (false, true) => return (f.trim(a), invalid),
        (false, false) => {}
    }
    // Two zeroes compare equal, so which one is smaller is decided by the sign rather
    // than by the comparison.
    if let (Number::Zero(sa), Number::Zero(sb)) = (ua, ub) {
        let sign = if want_max { sa && sb } else { sa || sb };
        return (f.zero(sign), invalid);
    }
    let take_a = match compare(f, a, b) {
        Some(Ordering::Less) => !want_max,
        Some(Ordering::Greater) => want_max,
        _ => true,
    };
    (f.trim(if take_a { a } else { b }), invalid)
}

/// Which of the ten kinds of value this is.
pub fn classify(f: Format, a: u64) -> u64 {
    match unpack(f, a) {
        Number::NotANumber(true) => CLASS_SIGNALLING_NAN,
        Number::NotANumber(false) => CLASS_QUIET_NAN,
        Number::Infinite(true) => CLASS_NEG_INF,
        Number::Infinite(false) => CLASS_POS_INF,
        Number::Zero(true) => CLASS_NEG_ZERO,
        Number::Zero(false) => CLASS_POS_ZERO,
        Number::Finite { sign, exp, sig } => {
            let subnormal = exp == f.floor() && sig >> (f.sig - 1) == 0;
            match (sign, subnormal) {
                (true, true) => CLASS_NEG_SUBNORMAL,
                (true, false) => CLASS_NEG_NORMAL,
                (false, true) => CLASS_POS_SUBNORMAL,
                (false, false) => CLASS_POS_NORMAL,
            }
        }
    }
}

/// Move a number from one format to another.
pub fn convert(from: Format, to: Format, a: u64, mode: Round) -> Outcome {
    match unpack(from, a) {
        Number::NotANumber(signalling) => (to.canonical_nan(), if signalling { NV } else { 0 }),
        Number::Infinite(sign) => (to.infinity(sign), 0),
        Number::Zero(sign) => (to.zero(sign), 0),
        Number::Finite { sign, exp, sig } => round(to, sign, exp, sig, false, mode),
    }
}

/// A number as an integer of `bits` bits, saturating rather than wrapping, because a
/// value that does not fit has no nearest integer to give.
/// The RISC-V Instruction Set Manual Volume I, 20.5, table 13.
pub fn to_integer(f: Format, a: u64, bits: u32, signed: bool, mode: Round) -> Outcome {
    let (low, high): (i128, u128) = match (signed, bits) {
        (true, 32) => (i32::MIN as i128, i32::MAX as u128),
        (true, _) => (i64::MIN as i128, i64::MAX as u128),
        (false, 32) => (0, u32::MAX as u128),
        (false, _) => (0, u64::MAX as u128),
    };
    let saturate = |value: i128| -> u64 {
        match bits {
            32 => value as i32 as i64 as u64,
            _ => value as u64,
        }
    };
    let number = unpack(f, a);
    let invalid = |value: i128| (saturate(value), NV);
    match number {
        // Every value that has no integer to round to saturates and says so, and a NaN
        // goes to the largest rather than to either end.
        Number::NotANumber(_) => return invalid(high as i128),
        Number::Infinite(sign) => return invalid(if sign { low } else { high as i128 }),
        Number::Zero(_) => return (0, 0),
        Number::Finite { .. } => {}
    }
    let Number::Finite { sign, exp, sig } = number else {
        unreachable!("answered above")
    };

    // Take the value apart into what is above the point and what is below it, and let
    // the rounding mode decide what the part below does to the part above.
    let (whole, half, rest) = if exp >= 0 {
        if exp > 127 || sig.leading_zeros() < exp as u32 {
            return invalid(if sign { low } else { high as i128 });
        }
        (sig << exp, false, false)
    } else if -exp >= 128 {
        (0, false, sig != 0)
    } else {
        let shift = (-exp) as u32;
        (
            sig >> shift,
            (sig >> (shift - 1)) & 1 == 1,
            sig & ((1u128 << (shift - 1)) - 1) != 0,
        )
    };

    let inexact = half || rest;
    let up = match mode {
        Round::Nearest => half && (rest || whole & 1 == 1),
        Round::NearestMax => half,
        Round::Zero => false,
        Round::Down => inexact && sign,
        Round::Up => inexact && !sign,
    };
    let magnitude = whole + up as u128;
    let value = if sign {
        match magnitude.checked_neg() {
            _ if magnitude > (low.unsigned_abs()) => return invalid(low),
            _ => -(magnitude as i128),
        }
    } else if magnitude > high {
        return invalid(high as i128);
    } else {
        magnitude as i128
    };
    (saturate(value), if inexact { NX } else { 0 })
}

/// An integer as a number, which is exact unless it has more significant bits than the
/// format keeps.
pub fn from_integer(f: Format, value: u64, bits: u32, signed: bool, mode: Round) -> Outcome {
    let value = match (signed, bits) {
        (true, 32) => value as i32 as i128,
        (true, _) => value as i64 as i128,
        (false, 32) => (value as u32) as i128,
        (false, _) => value as i128,
    };
    if value == 0 {
        return (f.zero(false), 0);
    }
    let sign = value < 0;
    round(f, sign, 0, value.unsigned_abs(), false, mode)
}

/// Take the sign of one number and the rest of another. It is a bit operation, so a
/// NaN passes through it untouched and nothing is raised.
pub fn sign_inject(f: Format, a: u64, b: u64, negate: bool, xor: bool) -> u64 {
    let sign = match (negate, xor) {
        (_, true) => (f.trim(a) ^ f.trim(b)) & f.sign_bit(),
        (false, _) => f.trim(b) & f.sign_bit(),
        (true, _) => !f.trim(b) & f.sign_bit(),
    };
    (f.trim(a) & !f.sign_bit()) | sign
}
