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

/// How far above zero a decimal exponent can be before the answer switches to exponential form.
///
/// Twenty one is in the standard as a literal and there is no derivation behind it. It is the reason
/// `1e20` prints as `100000000000000000000` and `1e21` prints as `1e+21`.
const POSITIVE_EXPONENT_LIMIT: i32 = 21;

/// How far below zero it can go before the same thing happens, exclusive.
///
/// Also a literal in the standard, and the reason `1e-6` prints as `0.000001` and `1e-7` does not.
const NEGATIVE_EXPONENT_LIMIT: i32 = -6;

/// The ECMAScript `Number::toString` operation in base ten.
///
/// Rust and JavaScript agree on how many digits to print and disagree on where to put them, and in
/// one case on what the last digit is. Both pick the shortest decimal that reads back as the same
/// double, which is the hard half and the half we do not have to write. What differs is the
/// formatting rule around those digits: Rust's `Display` never uses exponential notation, so it
/// prints `1e21` as a one followed by twenty one zeros, and it prints `1e-7` as `0.0000001`.
/// JavaScript switches to exponential form outside a window that runs from `1e-7` up to `1e21`, and
/// the boundaries are literals in the standard rather than anything derivable.
///
/// So the digits come from Rust and the placement comes from here. `{:e}` gives the shortest digits
/// and the decimal exponent already separated, which is exactly the `s` and the `n` the standard's
/// step five asks for, and everything after that is the standard's own case analysis. The one place
/// the digits themselves need a correction is the tie in that step, which `even_on_a_tie` handles.
pub(crate) fn to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    // Before the sign test, because negative zero prints as `0` and not as `-0`. `Object.is` can
    // still tell the two apart, which is why the value keeps the sign and only the text loses it.
    if value == 0.0 {
        return "0".to_owned();
    }
    if value < 0.0 {
        return format!("-{}", to_string(-value));
    }
    if value.is_infinite() {
        return "Infinity".to_owned();
    }

    // `{:e}` produces `de±x` or `d.ddde±x`, always with the shortest digit string that round trips.
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("`{:e}` always writes an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i32 = exponent.parse().expect("`{:e}` always writes an integer");
    let digits = even_on_a_tie(value, digits, exponent);

    // The standard's names. `k` is how many significant digits there are and `n` is where the
    // decimal point goes relative to the front of them, so the value is `0.digits` times `10^n`.
    let k = i32::try_from(digits.len()).expect("a shortest round trip has at most 17 digits");
    let n = exponent + 1;

    if k <= n && n <= POSITIVE_EXPONENT_LIMIT {
        // An integer with room to spare: the digits, then enough zeros to reach the point.
        let zeros = usize::try_from(n - k).expect("n is at least k here");
        return format!("{digits}{}", "0".repeat(zeros));
    }
    if 0 < n && n <= POSITIVE_EXPONENT_LIMIT {
        // The point falls inside the digits.
        let at = usize::try_from(n).expect("n is positive here");
        return format!("{}.{}", &digits[..at], &digits[at..]);
    }
    if NEGATIVE_EXPONENT_LIMIT < n && n <= 0 {
        // Small enough to write out in full, which is a leading zero and then the gap.
        let zeros = usize::try_from(-n).expect("n is not positive here");
        return format!("0.{}{digits}", "0".repeat(zeros));
    }

    // Outside the window, so exponential form. The exponent written is `n - 1`, which is the one
    // `{:e}` handed over, and it always carries an explicit sign.
    let sign = if exponent < 0 { '-' } else { '+' };
    let magnitude = exponent.abs();
    if k == 1 {
        return format!("{digits}e{sign}{magnitude}");
    }
    format!("{}.{}e{sign}{magnitude}", &digits[..1], &digits[1..])
}

/// How many significant decimal digits it takes to write any double exactly.
///
/// Not an estimate. A double is a whole number times a power of two, so its decimal expansion always
/// terminates, and the longest one belongs to a subnormal near `1.112536929253602e-308`, which needs
/// exactly this many. Asking for more only appends zeros and asking for fewer would round, which is
/// the one thing the check below cannot afford.
const EXACT_DIGITS: usize = 767;

/// The standard's tie break in step five, which Rust does the other way.
///
/// Both the standard and Rust ask for the shortest digit string that reads back as the same double,
/// and both then ask for the one closest to the value it came from. Sometimes two strings of that
/// length are exactly the same distance away, and there the two rules part company: the standard
/// says to take the even one and Rust takes the larger one. `9007199254740993 / 10` is the case the
/// differential harness found. The double is exactly `900719925474099.25`, so `...99.2` and `...99.3`
/// are both sixteen digits and both exactly a twentieth away, Rust prints the `3` and Node prints the
/// `2`, and the standard is on Node's side.
///
/// The correction is only ever downward by one in the last digit, because Rust always takes the
/// larger of the two and the two differ by exactly that. So there is nothing to do when the last
/// digit is already even, which is half of all numbers and the reason the expensive check below
/// almost never runs.
fn even_on_a_tie(value: f64, digits: String, exponent: i32) -> String {
    let last = *digits.as_bytes().last().expect("`{:e}` writes a digit");
    if last.is_multiple_of(2) {
        return digits;
    }

    // The other candidate, which is this one with the last digit down by one. No borrow is possible
    // because an odd digit is at least one, and the string cannot lose a digit that way, which would
    // have meant a shorter representation existed and Rust would have printed that instead.
    let mut lower = digits.clone().into_bytes();
    let last_index = lower.len() - 1;
    lower[last_index] -= 1;
    let lower = String::from_utf8(lower).expect("digits are ascii");

    // The cheap filter, and also a correctness condition rather than only a filter. If the lower
    // candidate does not read back as the same double then it is not a candidate at all, and at the
    // very edge of the rounding interval that really happens, because reading a decimal back has a
    // tie break of its own and it can send the lower one to the neighbouring double.
    if !reads_back_as(&lower, exponent, value) {
        return digits;
    }
    if !is_exactly_between(value, digits.len()) {
        return digits;
    }
    lower
}

/// Whether a digit string with an exponent parses back to exactly this double.
fn reads_back_as(digits: &str, exponent: i32, value: f64) -> bool {
    let (head, tail) = digits.split_at(1);
    let text = if tail.is_empty() {
        format!("{head}e{exponent}")
    } else {
        format!("{head}.{tail}e{exponent}")
    };
    text.parse::<f64>().is_ok_and(|parsed| parsed == value)
}

/// Whether the value sits exactly halfway between the two candidates of length `k`.
///
/// Halfway means its own exact expansion is those `k` digits followed by a five and then nothing,
/// so the test is to write the value out exactly and look. This is the slow path in the file, a
/// several hundred digit conversion, and it is reached only when the shortest form ends in an odd
/// digit and the candidate below it also reads back, which is rare enough that the cost never shows
/// up in a program. It is written this way rather than as exact integer arithmetic because the
/// arithmetic needs numbers wider than a `u128` and this needs nothing at all.
fn is_exactly_between(value: f64, k: usize) -> bool {
    let exact = format!("{value:.*e}", EXACT_DIGITS - 1);
    let mantissa = exact
        .split_once('e')
        .expect("`{:e}` always writes an exponent")
        .0;
    let digits: Vec<u8> = mantissa.bytes().filter(|byte| *byte != b'.').collect();
    // A shortest form is at most seventeen digits, so there is always a digit at `k` to look at.
    digits[k] == b'5' && digits[k + 1..].iter().all(|byte| *byte == b'0')
}

/// The ECMAScript `StringToNumber` abstract operation.
///
/// This is what `Number("12")`, `+"12"` and `"12" * 1` all go through, and it is not the same
/// grammar as a JavaScript numeric literal. A literal cannot be empty and this can, producing zero.
/// A literal can have a numeric separator and this cannot, so `Number("1_000")` is `NaN`. A literal
/// cannot start with a sign because the sign is a separate unary operator, and this can. The two
/// grammars agree about hexadecimal, binary and octal prefixes, and both refuse a sign in front of
/// one, which is why `Number("-0x10")` is `NaN` rather than minus sixteen.
///
/// Rust's own parser does the hard part, which is turning a decimal string into the correctly
/// rounded double. What it will not do is decide which strings are allowed: it accepts `inf`, `nan`
/// and `infinity` in any case, and JavaScript accepts exactly `Infinity` and nothing else that looks
/// like a word. So the shape is checked here and the digits are converted there.
pub(crate) fn from_string(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_white_space);
    // Empty is zero, which is the reason `+""` is `0` and `[] == 0` is true.
    if trimmed.is_empty() {
        return 0.0;
    }

    // The prefixed forms, each with the digit count that still fits a `u128` exactly.
    for (prefix, radix, exact) in [("0x", 16, 32), ("0o", 8, 42), ("0b", 2, 128)] {
        let upper = prefix.to_ascii_uppercase();
        if let Some(digits) = trimmed
            .strip_prefix(prefix)
            .or_else(|| trimmed.strip_prefix(upper.as_str()))
        {
            return radix_value(digits, radix, exact);
        }
    }

    // Spelled exactly like this and no other way, which is why `Number("infinity")` is `NaN`.
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }

    // The gate that stops Rust from accepting words JavaScript does not. Everything that gets past
    // it is made only of characters a decimal literal can contain, so a `NaN` from here on means the
    // arrangement was wrong rather than that a letter sneaked through.
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return f64::NAN;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

/// The value of a run of digits in a base, or `NaN` if it is empty or has a digit that base has not.
///
/// The specification asks for the exact mathematical value and then one rounding to a double at the
/// end. Accumulating in a double would round at every step instead, and those roundings compound, so
/// the digits go into a `u128` first and get rounded once on the way out. Above the width a `u128`
/// holds there is nothing left to accumulate into, and a string of more than thirty two hexadecimal
/// digits is far outside anything a program writes down, so that case falls back to the double
/// accumulation and its last bit is not guaranteed.
fn radix_value(digits: &str, radix: u32, exact_digits: usize) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    if digits.len() <= exact_digits {
        let mut value: u128 = 0;
        for character in digits.chars() {
            let Some(digit) = character.to_digit(radix) else {
                return f64::NAN;
            };
            value = value * u128::from(radix) + u128::from(digit);
        }
        // The rounding the specification asks for, happening once and in the right place.
        #[allow(clippy::cast_precision_loss)]
        return value as f64;
    }

    let mut value = 0.0;
    for character in digits.chars() {
        let Some(digit) = character.to_digit(radix) else {
            return f64::NAN;
        };
        value = value * f64::from(radix) + f64::from(digit);
    }
    value
}

/// Whether a character is `StrWhiteSpace`, which is not the same set as Rust's `char::is_whitespace`.
///
/// Two differences, both of which change an answer. JavaScript counts the byte order mark U+FEFF as
/// whitespace here and Rust does not, so `Number("\u{feff}1")` is one and `str::trim` would leave the
/// mark in place and produce `NaN`. Rust counts the next line character U+0085 and JavaScript does
/// not, so `Number("\u{85}1")` is `NaN` and `str::trim` would produce one. Getting either backwards
/// is a silent wrong answer rather than a crash, which is why the set is written out.
fn is_white_space(character: char) -> bool {
    matches!(
        character,
        // Tab, line feed, vertical tab, form feed, carriage return and space.
        '\u{9}'..='\u{d}' | '\u{20}'
        // The byte order mark, which is whitespace in this grammar and nowhere else.
        | '\u{feff}'
        // The line separators, which are line terminators rather than spaces and count all the same.
        | '\u{2028}' | '\u{2029}'
        // Everything Unicode files under the space separator category.
        | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

#[cfg(test)]
mod tests {
    use super::{exponentiate, from_string, shift_count, to_int32, to_string, to_uint32};

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

    /// Every expected string below was produced by running `String(x)` under Node 24.18.0 rather
    /// than written from memory, because the interesting cases are exactly the ones intuition gets
    /// wrong.
    #[test]
    fn a_number_prints_the_way_node_prints_it() {
        let cases = [
            (0.0, "0"),
            // Negative zero prints without the sign, even though `Object.is` can still see it.
            (-0.0, "0"),
            (1.0, "1"),
            (-1.0, "-1"),
            (1.5, "1.5"),
            (-1.5, "-1.5"),
            (100.0, "100"),
            (0.1, "0.1"),
            (0.5, "0.5"),
            (1.0 / 3.0, "0.3333333333333333"),
            (123.456, "123.456"),
            (4_294_967_295.0, "4294967295"),
            (9_007_199_254_740_991.0, "9007199254740991"),
            (9_007_199_254_740_992.0, "9007199254740992"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(value), expected, "{value:?}");
        }
    }

    #[test]
    fn the_window_where_a_number_stops_being_written_out_in_full() {
        // Both boundaries are literals in the standard with no derivation behind them, and both
        // are places where Rust's own `Display` would print something else.
        let cases = [
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (-1e-7, "-1e-7"),
            (1.5e-7, "1.5e-7"),
            (1e300, "1e+300"),
            (1.234_567_890_123_456_8e21, "1.2345678901234568e+21"),
            (1.234_567_890_123_456_8e20, "123456789012345680000"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(value), expected, "{value:?}");
        }
    }

    /// The bug the differential harness found on its twenty thousand program run, and the family it
    /// belongs to. Every expected string came from `String(x)` under Node 26.7.0.
    #[test]
    #[expect(
        clippy::excessive_precision,
        reason = "the lint compares a literal against the way Rust prints the double it parses to, \
                  and printing that double the way Rust does is the bug this test is about, so its \
                  suggestion would replace every value below with the wrong one of the two spellings"
    )]
    fn a_tie_between_two_shortest_forms_goes_to_the_even_one() {
        let cases = [
            // The one the harness printed, which came out of `9007199254740993 / "10"`. The double
            // is exactly `900719925474099.25`, so both neighbours are sixteen digits and equally
            // far away, and Rust picks the larger while the standard picks the even one.
            (900_719_925_474_099.25, "900719925474099.2"),
            (243_635_934_593_564.63, "243635934593564.62"),
            (769_173_474_699_971.3, "769173474699971.2"),
            (2_231_345_704_263_678.3, "2231345704263678.2"),
            (1_113_339_353_519_017.3, "1113339353519017.2"),
            (1_698_740_550_241_089.3, "1698740550241089.2"),
            // Ties where the larger candidate is already the even one, so nothing is corrected and
            // Rust and the standard agree. These are here because a correction applied in both
            // directions would break them and nothing else would notice.
            (1_886_581_632_185_860.8, "1886581632185860.8"),
            (1_257_207_345_813_426.8, "1257207345813426.8"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(value), expected, "{value:?}");
            assert_eq!(to_string(-value), format!("-{expected}"), "{value:?}");
        }
    }

    #[test]
    fn an_odd_last_digit_that_is_not_a_tie_is_left_alone() {
        // The guard the exact check is there for. Each of these ends in an odd digit and is the
        // only shortest form there is, so lowering the last digit would print a different number.
        let cases = [
            (0.3, "0.3"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0 / 3.0, "0.3333333333333333"),
            (123.457, "123.457"),
            (9_007_199_254_740_993.0, "9007199254740992"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
        ];
        for (value, expected) in cases {
            assert_eq!(to_string(value), expected, "{value:?}");
        }
    }

    #[test]
    fn the_edges_of_the_double_range_print_as_themselves() {
        assert_eq!(to_string(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(to_string(5e-324), "5e-324");
        assert_eq!(to_string(f64::NAN), "NaN");
        assert_eq!(to_string(f64::INFINITY), "Infinity");
        assert_eq!(to_string(f64::NEG_INFINITY), "-Infinity");
    }

    /// As above, every expected value below came from running `Number(x)` under Node 24.18.0.
    #[test]
    fn a_string_converts_to_the_number_node_converts_it_to() {
        let cases = [
            // The empty string and whitespace are zero, which is where `[] == 0` comes from.
            ("", 0.0),
            (" ", 0.0),
            ("\t\n\r 12 \t", 12.0),
            ("12", 12.0),
            ("12.5", 12.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("+12", 12.0),
            ("-12", -12.0),
            ("1e3", 1000.0),
            ("1E3", 1000.0),
            ("1e+3", 1000.0),
            ("1e-3", 0.001),
            // Out of range in both directions, and neither of them is an error.
            ("1e1000", f64::INFINITY),
            ("1e-1000", 0.0),
            // More digits than a double has, rounded rather than refused.
            ("9007199254740993", 9_007_199_254_740_992.0),
        ];
        for (text, expected) in cases {
            assert_eq!(from_string(text), expected, "{text:?}");
        }
    }

    #[test]
    fn the_prefixed_forms_are_accepted_and_a_sign_in_front_of_one_is_not() {
        assert_eq!(from_string("0x10"), 16.0);
        assert_eq!(from_string("0X1f"), 31.0);
        assert_eq!(from_string("0b101"), 5.0);
        assert_eq!(from_string("0o17"), 15.0);
        assert_eq!(from_string("  0x10  "), 16.0);
        // A hexadecimal literal is unsigned in this grammar, so the sign is not a sign here, it is
        // a character the number cannot contain.
        assert!(from_string("-0x10").is_nan());
        assert!(from_string("+0x10").is_nan());
        // A prefix with nothing after it, and a digit the base does not have.
        for text in ["0x", "0b", "0o", "0xg", "0b102"] {
            assert!(from_string(text).is_nan(), "{text:?}");
        }
        // Wider than a double, rounded once at the end rather than at every digit.
        assert_eq!(
            from_string("0xFFFFFFFFFFFFFFFFFF"),
            4.722_366_482_869_645e21
        );
    }

    #[test]
    fn infinity_is_spelled_one_way_and_a_word_is_never_a_number() {
        assert_eq!(from_string("Infinity"), f64::INFINITY);
        assert_eq!(from_string("+Infinity"), f64::INFINITY);
        assert_eq!(from_string("-Infinity"), f64::NEG_INFINITY);
        // Rust's own parser accepts all of these and JavaScript accepts none of them, which is the
        // entire reason the shape is checked before the digits are handed over.
        for text in ["infinity", "INFINITY", "inf", "nan", "NaN", "e5", "12abc"] {
            assert!(from_string(text).is_nan(), "{text:?}");
        }
    }

    #[test]
    fn a_numeric_separator_is_a_literal_feature_and_not_a_conversion_one() {
        // `1_000` is a valid number in source text and is not a valid string to convert, which
        // catches anyone who assumed the two grammars were the same one.
        assert!(from_string("1_000").is_nan());
        assert!(from_string(" 1 2 ").is_nan());
        for text in ["1e", ".", "-.", "1.2.3", "+", "-"] {
            assert!(from_string(text).is_nan(), "{text:?}");
        }
    }

    #[test]
    fn the_whitespace_that_gets_trimmed_is_javascripts_set_and_not_rusts() {
        // The byte order mark is whitespace to JavaScript and not to Rust, so `str::trim` would
        // leave it in place and this would come back NaN.
        assert_eq!(from_string("\u{feff}1"), 1.0);
        assert_eq!(from_string("\u{3000}1"), 1.0);
        assert_eq!(from_string("\u{a0}1\u{2028}"), 1.0);
        // And the next line character is whitespace to Rust and not to JavaScript, so `str::trim`
        // would strip it and this would come back as one.
        assert!(from_string("\u{85}1").is_nan());
    }

    #[test]
    fn a_negative_zero_string_converts_to_a_negative_zero() {
        // The sign survives even though it is invisible in the value's own printed form, and
        // `Object.is` can see it, so losing it here would be a wrong answer nobody would notice.
        assert_eq!(from_string("-0").to_bits(), (-0.0_f64).to_bits());
        assert_eq!(from_string("0").to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn every_printed_number_reads_back_as_itself() {
        // The property the shortest round trip guarantee is for, checked over a spread wide enough
        // to cross both boundaries and both signs.
        let mut value = 1e-30;
        while value < 1e30 {
            for candidate in [value, -value, value * 1.5, value / 3.0] {
                let printed = to_string(candidate);
                let parsed: f64 = printed.parse().expect("should parse back");
                assert_eq!(parsed.to_bits(), candidate.to_bits(), "{printed}");
            }
            value *= 7.0;
        }
    }
}
