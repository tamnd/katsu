//! The numeric conversions and operators, written to the specification rather than to intuition.
//!
//! Every one of these is a place where the obvious Rust expression is subtly not what JavaScript
//! says. `2 ** Infinity` is `Infinity` and `1 ** Infinity` is `NaN`. `-1 >>> 0` is `4294967295` and
//! not `-1`. `1 << 32` is `1` and not `0`. `0.1 | 0` is `0` because `|` converts through a wrap to
//! thirty two bits that is defined in terms of a modulo and not in terms of a cast.
//!
//! They live in their own module because they are pure functions of their inputs with no interpreter
//! state involved, which means they can be tested exhaustively against the values that break them
//! rather than through a program that happens to use them. The dispatch loop then reads as dispatch
//! and not as arithmetic.
//!
//! What is not here is anything that can call back into JavaScript. `ToPrimitive` on an object runs
//! `valueOf`, which is a call, which needs the interpreter, so it goes in the loop and not here.
//! Nothing in this module can throw and nothing in it can allocate.

// Comparing floats for exact equality is the whole job here. JavaScript's `==` is exact IEEE
// equality, not equality within a tolerance, and a comparison against a margin of error would be a
// different language. The lint is right about numerical code in general and wrong about this file,
// so it is turned off once at the top rather than per line, which would suggest each case had been
// weighed separately when they are all the same case.
#![allow(clippy::float_cmp)]

/// Two to the thirty two, the modulus the integer conversions are defined against.
const TWO_32: f64 = 4_294_967_296.0;
/// Two to the thirty one, where the signed range folds over.
const TWO_31: f64 = 2_147_483_648.0;

/// The ECMAScript `ToInt32` abstract operation.
///
/// Not a cast. `as i32` in Rust saturates, so `1e10 as i32` is `i32::MAX`, and the specification says
/// to take the value modulo two to the thirty two and then fold the top half negative, so `1e10 | 0`
/// is `1410065408`. Anything not finite converts to zero, which is why `NaN | 0` is `0` rather than
/// an error.
#[must_use]
pub(crate) fn to_int32(value: f64) -> i32 {
    let wrapped = wrap_to_32_bits(value);
    let signed = if wrapped >= TWO_31 {
        wrapped - TWO_32
    } else {
        wrapped
    };
    // The value is in `-2^31 ..= 2^31 - 1` by construction, so the cast cannot saturate and cannot
    // lose anything. This is the one place a cast is correct, and it is correct because of the two
    // lines above it.
    #[allow(clippy::cast_possible_truncation)]
    {
        signed as i32
    }
}

/// The ECMAScript `ToUint32` abstract operation.
///
/// The same wrap without the fold, which is why `-1 >>> 0` is `4294967295`.
#[must_use]
pub(crate) fn to_uint32(value: f64) -> u32 {
    // In `0 .. 2^32` by construction, so the cast is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        wrap_to_32_bits(value) as u32
    }
}

/// The shared part of both conversions: truncate towards zero, then take modulo two to the thirty
/// two with the sign of the divisor, so the answer is never negative.
fn wrap_to_32_bits(value: f64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return 0.0;
    }
    // `rem_euclid` is the modulo the specification means, the one that takes the sign of the
    // divisor. Rust's `%` takes the sign of the dividend, which would make every negative input
    // come out on the wrong side of the fold.
    value.trunc().rem_euclid(TWO_32)
}

/// How many bits a shift actually shifts by, which is the count taken modulo thirty two.
///
/// This is why `1 << 32` is `1` and not zero, and it is also why the shift below cannot overflow and
/// does not need a wrapping variant for safety, only for clarity.
#[must_use]
pub(crate) fn shift_count(value: f64) -> u32 {
    to_uint32(value) & 31
}

/// The ECMAScript `Number::exponentiate` operation, which is not `f64::powf`.
///
/// IEEE 754 `pow` says that `pow(x, ±0)` is one for every `x` including a NaN, and that `pow(1, y)`
/// is one for every `y` including a NaN and an infinity. JavaScript agrees about the first and
/// disagrees about the second: `1 ** NaN` is `NaN`, and so is `(-1) ** Infinity`. Rust's `powf` is
/// the IEEE operation, so the two disagreeing cases are handled before it is reached.
#[must_use]
pub(crate) fn exponentiate(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    // Checked before the base, because an exponent of zero produces one even for a NaN base.
    if exponent == 0.0 {
        return 1.0;
    }
    if base.abs() == 1.0 && exponent.is_infinite() {
        return f64::NAN;
    }
    base.powf(exponent)
}

#[cfg(test)]
mod tests {
    use super::{exponentiate, shift_count, to_int32, to_uint32};

    #[test]
    fn to_int32_wraps_where_a_cast_would_saturate() {
        // Every one of these is a value `as i32` gets wrong.
        assert_eq!(to_int32(1e10), 1_410_065_408);
        assert_eq!(to_int32(-1e10), -1_410_065_408);
        assert_eq!(to_int32(4_294_967_296.0), 0);
        assert_eq!(to_int32(4_294_967_297.0), 1);
        assert_eq!(to_int32(2_147_483_648.0), i32::MIN);
        assert_eq!(to_int32(-2_147_483_649.0), i32::MAX);
        assert_eq!(to_int32(1e21), -559_939_584);
    }

    #[test]
    fn to_int32_truncates_towards_zero_rather_than_rounding() {
        assert_eq!(to_int32(0.9), 0);
        assert_eq!(to_int32(-0.9), 0);
        assert_eq!(to_int32(1.9), 1);
        assert_eq!(to_int32(-1.9), -1);
    }

    #[test]
    fn everything_that_is_not_a_finite_number_converts_to_zero() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0] {
            assert_eq!(to_int32(value), 0, "{value} should convert to zero");
            assert_eq!(to_uint32(value), 0);
        }
    }

    #[test]
    fn to_uint32_does_not_fold_the_top_half_negative() {
        // The difference between the two conversions, and the reason `-1 >>> 0` is not `-1`.
        assert_eq!(to_uint32(-1.0), 4_294_967_295);
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_uint32(2_147_483_648.0), 2_147_483_648);
        assert_eq!(to_uint32(-2_147_483_648.0), 2_147_483_648);
    }

    #[test]
    fn a_shift_count_is_taken_modulo_thirty_two() {
        assert_eq!(shift_count(0.0), 0);
        assert_eq!(shift_count(31.0), 31);
        // The one everybody expects to be zero and is not.
        assert_eq!(shift_count(32.0), 0);
        assert_eq!(shift_count(33.0), 1);
        assert_eq!(shift_count(-1.0), 31);
        assert_eq!(shift_count(f64::NAN), 0);
    }

    #[test]
    fn exponentiate_disagrees_with_ieee_in_exactly_two_places() {
        // The cases where `powf` returns one and JavaScript says NaN.
        assert!(exponentiate(1.0, f64::NAN).is_nan());
        assert!(exponentiate(1.0, f64::INFINITY).is_nan());
        assert!(exponentiate(-1.0, f64::INFINITY).is_nan());
        assert!(exponentiate(-1.0, f64::NEG_INFINITY).is_nan());
        // And the case where it returns one and JavaScript agrees, which is why the exponent is
        // checked before the base.
        assert_eq!(exponentiate(f64::NAN, 0.0), 1.0);
        assert_eq!(exponentiate(f64::NAN, -0.0), 1.0);
    }

    #[test]
    fn exponentiate_is_ordinary_everywhere_else() {
        assert_eq!(exponentiate(2.0, 10.0), 1024.0);
        assert_eq!(exponentiate(2.0, -1.0), 0.5);
        assert_eq!(exponentiate(2.0, f64::INFINITY), f64::INFINITY);
        assert_eq!(exponentiate(0.5, f64::INFINITY), 0.0);
        assert!(exponentiate(f64::NAN, 1.0).is_nan());
        assert!(exponentiate(-2.0, 0.5).is_nan());
    }
}
