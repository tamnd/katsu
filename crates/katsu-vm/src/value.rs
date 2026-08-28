//! The 64 bit tagged value that lives in an interpreter register.
//!
//! Spec 07.1 asks for two representations: 32 bit compressed values in heap slots, where the
//! memory goal lives, and 64 bit tagged values in registers, so that a double does not have
//! to be boxed while it is in flight. This module is the second of those. The first arrives
//! with the cage.
//!
//! The encoding is the one JavaScriptCore arrived at, and the reasoning behind copying it is
//! in `spec/07-object-model.md`. The short version is that the obvious scheme, where any
//! quiet NaN pattern means "not a double", is wrong on real hardware: x86 SSE produces
//! `0xFFF8_0000_0000_0000` as its default NaN, which is a negative quiet NaN, so the sign bit
//! cannot be used to tell payloads apart from arithmetic results. Offsetting the doubles
//! sidesteps the question entirely.

use std::fmt;

/// Added to a double's bit pattern on encode and subtracted on decode.
///
/// Two to the forty ninth. After the offset every non NaN double lands in
/// `0x0002_0000_0000_0000 ..= 0xFFF2_0000_0000_0000`, which sits above the pointer range and
/// below the integer tag, so the three cases never collide.
const DOUBLE_ENCODE_OFFSET: u64 = 1 << 49;

/// The top sixteen bits of an encoded 32 bit integer.
const INT32_TAG: u64 = 0xFFFE_0000_0000_0000;

/// The NaN every NaN is canonicalised to before encoding.
///
/// Without this the largest encodable double would wrap past the integer tag. It also means
/// a NaN that came from `0/0` and a NaN that came from a signalling operation are the same
/// value, which JavaScript cannot observe anyway because it has exactly one NaN.
const CANONICAL_NAN_BITS: u64 = 0x7FF8_0000_0000_0000;

// The immediates. These are deliberately small integers in the pointer range rather than
// arbitrary bit patterns, because the bits they share make the common predicates cheap.
// `null` is 0b0010 and `undefined` is 0b1010, so they differ in one bit and the nullish test
// is a mask and a compare. `false` is 0b0110 and `true` is 0b0111, so the boolean test is
// the same shape and the value is the low bit.
const TAG_EMPTY: u64 = 0x0;
const TAG_NULL: u64 = 0x2;
const TAG_FALSE: u64 = 0x6;
const TAG_TRUE: u64 = 0x7;
const TAG_UNDEFINED: u64 = 0xA;

/// The bit that `null` and `undefined` differ in.
const UNDEFINED_BIT: u64 = 0x8;
/// The bit that `true` and `false` differ in.
const BOOL_BIT: u64 = 0x1;

/// The largest address a pointer payload may hold.
///
/// Everything at or below this is a pointer or an immediate, and everything above it is an
/// encoded double or an integer. Current 64 bit platforms give userspace 48 bits of address,
/// and the cage from spec 07.2 is a 4 GB reservation well inside that, so a real heap pointer
/// always fits. The assertion in `from_pointer` is what turns a future platform with wider
/// addresses into a loud failure rather than a silently corrupted value.
const MAX_POINTER: u64 = 0x0000_FFFF_FFFF_FFFF;

/// A JavaScript value as the interpreter and the JIT hold it in a register.
///
/// Registers hold 64 bits. Object slots on the heap hold 32, because pointer compression is
/// a day one decision and not an optimisation to add later. The two are deliberately
/// different types so that the compiler catches a confusion between them.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Value(u64);

impl Value {
    /// The absence of a value, which is not the same thing as `undefined`.
    ///
    /// A hole in a sparse array and a property that was never installed are empty. A property
    /// explicitly set to `undefined` is not. JavaScript can observe the difference through
    /// `in` and through `Object.hasOwn`, so the engine has to be able to as well.
    pub const EMPTY: Value = Value(TAG_EMPTY);
    /// `undefined`.
    pub const UNDEFINED: Value = Value(TAG_UNDEFINED);
    /// `null`.
    pub const NULL: Value = Value(TAG_NULL);
    /// `true`.
    pub const TRUE: Value = Value(TAG_TRUE);
    /// `false`.
    pub const FALSE: Value = Value(TAG_FALSE);

    /// A boolean.
    #[must_use]
    pub const fn from_bool(b: bool) -> Value {
        if b { Value::TRUE } else { Value::FALSE }
    }

    /// A 32 bit integer, held without going through a double.
    #[must_use]
    pub const fn from_i32(n: i32) -> Value {
        // Reinterpreting as unsigned first is what keeps a negative number from sign
        // extending across the tag, which is why the integer tag sits at the top of the range.
        //
        // The widening is an `as` rather than `u64::from` because `From` is not const yet,
        // and this wants to stay const so that a constant pool entry costs nothing at runtime.
        #[allow(clippy::cast_lossless)]
        Value(INT32_TAG | n.cast_unsigned() as u64)
    }

    /// A double.
    ///
    /// Prefer [`Value::from_f64`] unless you specifically want the double representation even
    /// for a value that would fit in an integer.
    #[must_use]
    pub fn from_double(n: f64) -> Value {
        let bits = if n.is_nan() {
            CANONICAL_NAN_BITS
        } else {
            n.to_bits()
        };
        Value(bits.wrapping_add(DOUBLE_ENCODE_OFFSET))
    }

    /// A number, using the integer representation when the value fits in one exactly.
    ///
    /// This is the constructor arithmetic should use. JavaScript has one number type and does
    /// not care which representation carries it, but the engine cares a great deal: an
    /// integer needs no decode arithmetic, compares by bit pattern, and is what an inline
    /// cache wants to see on an array index.
    #[must_use]
    pub fn from_f64(n: f64) -> Value {
        // The sign check is not redundant. Negative zero is a double, because `Object.is`
        // distinguishes it from positive zero and the integer representation cannot.
        //
        // The float comparisons are exact on purpose. This is asking whether the round trip
        // through i32 was lossless, which is a bit pattern question, and comparing within an
        // epsilon would answer a different question wrongly.
        #[allow(clippy::cast_possible_truncation, clippy::float_cmp)]
        let truncated = n as i32;
        #[allow(clippy::float_cmp)]
        let exact = f64::from(truncated) == n && !(n == 0.0 && n.is_sign_negative());
        if exact {
            Value::from_i32(truncated)
        } else {
            Value::from_double(n)
        }
    }

    /// A pointer to a heap object.
    ///
    /// # Panics
    ///
    /// Panics if the address does not fit in the pointer range, which would mean the encoding
    /// assumption in this module no longer holds on this platform.
    #[must_use]
    pub fn from_pointer(address: u64) -> Value {
        assert!(
            address <= MAX_POINTER,
            "heap pointer {address:#x} is outside the {MAX_POINTER:#x} range the value \
             encoding reserves for pointers"
        );
        Value(address)
    }

    /// The raw bits, for the JIT and for tests. Not meaningful without this module.
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Rebuild a value from bits produced by [`Value::to_bits`].
    #[must_use]
    pub const fn from_bits(bits: u64) -> Value {
        Value(bits)
    }

    /// Whether this is the empty value.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == TAG_EMPTY
    }

    /// Whether this is `undefined`.
    #[must_use]
    pub const fn is_undefined(self) -> bool {
        self.0 == TAG_UNDEFINED
    }

    /// Whether this is `null`.
    #[must_use]
    pub const fn is_null(self) -> bool {
        self.0 == TAG_NULL
    }

    /// Whether this is `null` or `undefined`, which is the check `==` against either performs
    /// and the one `?.` short circuits on.
    #[must_use]
    pub const fn is_nullish(self) -> bool {
        self.0 & !UNDEFINED_BIT == TAG_NULL
    }

    /// Whether this is a boolean.
    #[must_use]
    pub const fn is_bool(self) -> bool {
        self.0 & !BOOL_BIT == TAG_FALSE
    }

    /// Whether this is held as a 32 bit integer.
    ///
    /// This is a representation question rather than a language one. `1` and `1.0` are the
    /// same JavaScript number and this returns true for the first and false for the second.
    /// Use [`Value::is_number`] to ask the language question.
    #[must_use]
    pub const fn is_i32(self) -> bool {
        self.0 & INT32_TAG == INT32_TAG
    }

    /// Whether this is held as a double.
    #[must_use]
    pub const fn is_double(self) -> bool {
        !self.is_i32() && self.0 > MAX_POINTER
    }

    /// Whether this is a number, held either way.
    #[must_use]
    pub const fn is_number(self) -> bool {
        self.0 > MAX_POINTER
    }

    /// Whether this is a pointer to a heap object.
    #[must_use]
    pub const fn is_pointer(self) -> bool {
        // An immediate is a small integer inside the pointer range, so the range test alone
        // is not enough. Zero is the empty value and the immediates all fit under 16.
        self.0 != TAG_EMPTY && self.0 > 0xF && self.0 <= MAX_POINTER
    }

    /// The boolean, if this is one.
    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        if self.is_bool() {
            Some(self.0 & BOOL_BIT != 0)
        } else {
            None
        }
    }

    /// The integer, if it is held as one.
    #[must_use]
    pub const fn as_i32(self) -> Option<i32> {
        if self.is_i32() {
            #[allow(clippy::cast_possible_truncation)]
            let low = self.0 as u32;
            Some(low.cast_signed())
        } else {
            None
        }
    }

    /// The double, if it is held as one.
    #[must_use]
    pub fn as_double(self) -> Option<f64> {
        self.is_double()
            .then(|| f64::from_bits(self.0.wrapping_sub(DOUBLE_ENCODE_OFFSET)))
    }

    /// The number, however it is held.
    #[must_use]
    pub fn as_f64(self) -> Option<f64> {
        if let Some(n) = self.as_i32() {
            Some(f64::from(n))
        } else {
            self.as_double()
        }
    }

    /// The heap address, if this is a pointer.
    #[must_use]
    pub const fn as_pointer(self) -> Option<u64> {
        if self.is_pointer() {
            Some(self.0)
        } else {
            None
        }
    }

    /// The ECMAScript `ToBoolean` abstract operation, for the cases that exist so far.
    ///
    /// Objects are always truthy, including `new Boolean(false)`, which is why the pointer
    /// arm does not look at what it points to. Strings will need to, once they exist, because
    /// the empty string is falsy.
    #[must_use]
    pub fn to_boolean(self) -> bool {
        if let Some(b) = self.as_bool() {
            return b;
        }
        if let Some(n) = self.as_f64() {
            return n != 0.0 && !n.is_nan();
        }
        self.is_pointer()
    }

    /// The `typeof` operator, for the cases that exist so far.
    #[must_use]
    pub const fn type_of(self) -> &'static str {
        if self.is_undefined() {
            "undefined"
        } else if self.is_null() {
            // typeof null is "object". It is a bug from 1995 and it is in the standard.
            "object"
        } else if self.is_bool() {
            "boolean"
        } else if self.is_number() {
            "number"
        } else {
            "object"
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            f.write_str("empty")
        } else if self.is_undefined() {
            f.write_str("undefined")
        } else if self.is_null() {
            f.write_str("null")
        } else if let Some(b) = self.as_bool() {
            write!(f, "{b}")
        } else if let Some(n) = self.as_i32() {
            write!(f, "{n}i32")
        } else if let Some(n) = self.as_double() {
            write!(f, "{n}f64")
        } else {
            write!(f, "object@{:#x}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_POINTER, Value};

    #[test]
    fn the_immediates_are_all_distinct_and_know_themselves() {
        assert!(Value::UNDEFINED.is_undefined());
        assert!(Value::NULL.is_null());
        assert!(Value::EMPTY.is_empty());
        assert_eq!(Value::TRUE.as_bool(), Some(true));
        assert_eq!(Value::FALSE.as_bool(), Some(false));

        let all = [
            Value::EMPTY,
            Value::UNDEFINED,
            Value::NULL,
            Value::TRUE,
            Value::FALSE,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.to_bits(), b.to_bits(), "{a:?} and {b:?} share bits");
            }
        }
    }

    #[test]
    fn null_and_undefined_are_nullish_and_nothing_else_is() {
        assert!(Value::NULL.is_nullish());
        assert!(Value::UNDEFINED.is_nullish());
        assert!(!Value::FALSE.is_nullish());
        assert!(!Value::EMPTY.is_nullish());
        assert!(!Value::from_i32(0).is_nullish());
        assert!(!Value::from_double(0.0).is_nullish());
    }

    #[test]
    fn an_immediate_is_never_mistaken_for_a_pointer() {
        for v in [
            Value::EMPTY,
            Value::UNDEFINED,
            Value::NULL,
            Value::TRUE,
            Value::FALSE,
        ] {
            assert!(!v.is_pointer(), "{v:?} claims to be a pointer");
            assert_eq!(v.as_pointer(), None);
        }
    }

    #[test]
    fn every_i32_round_trips() {
        // Every boundary plus a stride across the whole range. The stride is prime so it does
        // not land on the same low bits every time.
        let mut cases = vec![0, 1, -1, i32::MAX, i32::MIN, i32::MAX - 1, i32::MIN + 1];
        let mut n: i64 = i64::from(i32::MIN);
        while n <= i64::from(i32::MAX) {
            #[allow(clippy::cast_possible_truncation)]
            cases.push(n as i32);
            n += 7_919_191;
        }
        for n in cases {
            let v = Value::from_i32(n);
            assert!(v.is_i32(), "{n} did not encode as an integer");
            assert!(v.is_number());
            assert!(!v.is_double());
            assert!(!v.is_pointer());
            assert_eq!(v.as_i32(), Some(n), "{n} did not round trip");
            assert_eq!(v.as_f64(), Some(f64::from(n)));
        }
    }

    #[test]
    fn every_interesting_double_round_trips() {
        let cases = [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::EPSILON,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9_007_199_254_740_991.0,
            -9_007_199_254_740_991.0,
            1e308,
            1e-308,
            5e-324,
        ];
        for n in cases {
            let v = Value::from_double(n);
            assert!(v.is_double(), "{n} did not encode as a double");
            assert!(v.is_number());
            assert!(!v.is_i32());
            assert!(!v.is_pointer());
            assert_eq!(v.as_double(), Some(n), "{n} did not round trip");
            assert_eq!(v.as_double().map(f64::to_bits), Some(n.to_bits()));
        }
    }

    #[test]
    fn nan_survives_as_a_nan_rather_than_as_its_original_bits() {
        // JavaScript has exactly one NaN, so canonicalising is not observable. It is required
        // here because an uncanonicalised NaN would encode past the integer tag.
        for bits in [
            0x7FF8_0000_0000_0000_u64,
            0xFFF8_0000_0000_0000,
            0x7FF0_0000_0000_0001,
            0xFFFF_FFFF_FFFF_FFFF,
        ] {
            let v = Value::from_double(f64::from_bits(bits));
            assert!(v.is_double(), "{bits:#x} did not encode as a double");
            assert!(!v.is_i32(), "{bits:#x} collided with the integer tag");
            assert!(
                v.as_double().expect("a double").is_nan(),
                "{bits:#x} stopped being a NaN"
            );
        }
    }

    #[test]
    fn from_f64_picks_the_integer_representation_when_it_is_exact() {
        assert!(Value::from_f64(1.0).is_i32());
        assert!(Value::from_f64(-1.0).is_i32());
        assert!(Value::from_f64(0.0).is_i32());
        assert!(Value::from_f64(2_147_483_647.0).is_i32());
        assert!(Value::from_f64(-2_147_483_648.0).is_i32());

        assert!(Value::from_f64(1.5).is_double());
        assert!(Value::from_f64(2_147_483_648.0).is_double());
        assert!(Value::from_f64(-2_147_483_649.0).is_double());
        assert!(Value::from_f64(f64::NAN).is_double());
        assert!(Value::from_f64(f64::INFINITY).is_double());
    }

    #[test]
    fn negative_zero_stays_a_double_because_object_is_can_see_it() {
        let v = Value::from_f64(-0.0);
        assert!(v.is_double(), "negative zero was flattened into an integer");
        let n = v.as_f64().expect("a number");
        assert!(n.is_sign_negative() && n == 0.0);
        assert!(Value::from_f64(0.0).is_i32());
    }

    #[test]
    fn a_pointer_round_trips_and_is_not_a_number() {
        for address in [0x10_u64, 0x1000, 0x7FFF_FFFF_F000, MAX_POINTER] {
            let v = Value::from_pointer(address);
            assert!(v.is_pointer(), "{address:#x} did not encode as a pointer");
            assert!(!v.is_number());
            assert!(!v.is_bool());
            assert!(!v.is_nullish());
            assert_eq!(v.as_pointer(), Some(address));
            assert_eq!(v.as_f64(), None);
        }
    }

    #[test]
    #[should_panic(expected = "outside the")]
    fn an_address_that_does_not_fit_fails_loudly_rather_than_corrupting() {
        let _ = Value::from_pointer(MAX_POINTER + 1);
    }

    #[test]
    fn no_double_ever_lands_in_the_pointer_range_or_the_integer_tag() {
        // The property the whole encoding rests on. Walk the exponent and mantissa space
        // rather than a handful of literals, because the failure this guards against is a
        // single bit pattern colliding rather than a whole class of them.
        let mut collisions = 0;
        for exponent in 0..=0x7FF_u64 {
            for mantissa_bit in 0..52 {
                for sign in [0_u64, 1] {
                    let bits = (sign << 63) | (exponent << 52) | (1_u64 << mantissa_bit);
                    let v = Value::from_double(f64::from_bits(bits));
                    if !v.is_double() || v.is_pointer() || v.is_i32() {
                        collisions += 1;
                    }
                }
            }
        }
        assert_eq!(collisions, 0, "{collisions} double patterns collided");
    }

    #[test]
    fn to_boolean_follows_the_specification_including_the_awkward_parts() {
        assert!(!Value::UNDEFINED.to_boolean());
        assert!(!Value::NULL.to_boolean());
        assert!(!Value::FALSE.to_boolean());
        assert!(!Value::from_i32(0).to_boolean());
        assert!(!Value::from_double(-0.0).to_boolean());
        assert!(!Value::from_double(f64::NAN).to_boolean());

        assert!(Value::TRUE.to_boolean());
        assert!(Value::from_i32(-1).to_boolean());
        assert!(Value::from_double(0.1).to_boolean());
        assert!(Value::from_double(f64::INFINITY).to_boolean());
        // An object is truthy without being read, including `new Boolean(false)`.
        assert!(Value::from_pointer(0x1000).to_boolean());
    }

    #[test]
    fn typeof_null_is_object_because_the_standard_says_so() {
        assert_eq!(Value::NULL.type_of(), "object");
        assert_eq!(Value::UNDEFINED.type_of(), "undefined");
        assert_eq!(Value::TRUE.type_of(), "boolean");
        assert_eq!(Value::from_i32(1).type_of(), "number");
        assert_eq!(Value::from_double(1.5).type_of(), "number");
        assert_eq!(Value::from_pointer(0x1000).type_of(), "object");
    }

    #[test]
    fn a_register_holding_a_value_is_still_eight_bytes() {
        // The point of the encoding. If this ever grows, every frame in the interpreter and
        // every spill slot in the JIT grows with it.
        assert_eq!(size_of::<Value>(), 8);
        assert_eq!(align_of::<Value>(), 8);
        assert_eq!(size_of::<Option<Value>>(), 16);
    }

    #[test]
    fn debug_output_names_the_representation_rather_than_hiding_it() {
        assert_eq!(format!("{:?}", Value::from_i32(7)), "7i32");
        assert_eq!(format!("{:?}", Value::from_double(7.5)), "7.5f64");
        assert_eq!(format!("{:?}", Value::UNDEFINED), "undefined");
        assert_eq!(format!("{:?}", Value::from_pointer(0x20)), "object@0x20");
    }
}
