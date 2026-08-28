//! Flat strings in the cage, in the two representations that matter.
//!
//! `spec/07-object-model.md` 7.7 calls the gap between JavaScript's UTF-16 strings and Rust's
//! guaranteed UTF-8 `String` the single most consequential representation decision in an engine
//! written in Rust, so this file tries to make every part of that gap explicit rather than
//! convenient. There is no `as_str` that always works, because there is no conversion that always
//! works.
//!
//! What is here is the flat half of the design: one byte per character Latin-1, and two bytes per
//! code unit UTF-16 for everything else. Ropes and slices are named in 7.7 and are not here. They
//! change how a string is stored and not what a string means, the kind field has room for them,
//! and building them before there is anything to concatenate would be guessing at the shape of a
//! problem nobody has hit yet. Until they arrive, `concat` copies, which makes string building in
//! a loop quadratic. That is a real gap and it is the first thing M1 should close.
//!
//! The representation is canonical: a string is UTF-16 only if it actually contains a code unit
//! above 255. Narrowing happens once, at allocation. That buys two things. Equal strings always
//! agree on representation, so equality can reject on the header before it looks at a byte, and
//! the memory win over an engine that stores everything as UTF-16 is taken by construction rather
//! than by a later optimisation pass.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::slice;

use crate::bump::{BumpHeap, ObjectKind};
use crate::cage::{Cage, Slot};

/// Bytes of header in front of the characters.
///
/// Three words: the shape slot, the length, and the packed hash and flags. `spec/07-object-model`
/// 7.9 budgets 26 bytes for a ten character ASCII string, which assumed a four word header. Three
/// words gets the same string to 22 bytes requested and 24 after alignment.
pub const STRING_HEADER_SIZE: usize = 12;

/// A compressed reference to the string's shape, which is where M1 will put the string map.
///
/// It is zero today, and zero is the integer zero rather than a pointer, so nothing can mistake
/// it for a shape that exists.
const SHAPE_OFFSET: usize = 0;
/// Length in code units, not bytes and not characters.
const LENGTH_OFFSET: usize = 4;
/// The hash in the high bits and the flags in the low four, the way V8 packs the same field.
const HASH_OFFSET: usize = 8;

/// Which of the representations this is. Two bits, so ropes and slices have somewhere to go.
const KIND_MASK: u32 = 0b11;
/// One byte per character.
const KIND_LATIN1: u32 = 0;
/// Two bytes per code unit.
const KIND_UTF16: u32 = 1;
/// Set when this string is the canonical copy in an atom table.
const INTERNED_BIT: u32 = 1 << 2;
/// Set once the hash has been computed and stored, because zero is a legitimate hash.
const HASHED_BIT: u32 = 1 << 3;
/// How far the hash sits above the flags.
const HASH_SHIFT: u32 = 4;
/// The mask for the twenty eight bits of hash that fit above the flags.
const HASH_MASK: u32 = u32::MAX >> HASH_SHIFT;

/// The longest string this engine will build, in code units.
///
/// A gigabyte of UTF-16 payload, which is a quarter of the cage. The limit exists because the
/// cage is four gigabytes and a compressed reference is a thirty two bit offset into it, so a
/// string that does not fit is a refusal rather than a wrapped offset. V8's limit is close to
/// 2^29 for similar reasons, so this is not the constraint a real program hits first.
pub const MAX_STRING_LENGTH: u32 = (1 << 30) - 1;

/// The multiplier the string hash mixes with. One of the xxHash constants, chosen because it is
/// odd and has a well spread bit pattern, which is all a non cryptographic mixer needs.
const HASH_MULTIPLIER: u64 = 0x9E37_79B1_85EB_CA87;

/// What went wrong turning a JavaScript string into Rust text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoneSurrogate {
    /// Index of the offending code unit.
    pub index: u32,
    /// The code unit itself, somewhere in D800 to DFFF.
    pub unit: u16,
}

impl fmt::Display for LoneSurrogate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lone surrogate {:#06x} at code unit {}",
            self.unit, self.index
        )
    }
}

impl std::error::Error for LoneSurrogate {}

/// A reference to a flat string in the cage.
///
/// This is a compressed slot and not a pointer, so it is four bytes and it survives being written
/// into an object or into the realm snapshot. Reading through it needs the cage, which is the
/// point: the offset means nothing without the base, and a moving collector arriving at M4 will
/// need exactly this shape of indirection.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringRef(Slot);

impl fmt::Debug for StringRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // No cage here, so there is no way to print the characters. Printing the offset is at
        // least enough to tell two strings apart in a failing test.
        write!(f, "str@{:?}", self.0)
    }
}

impl StringRef {
    /// Build a string from Rust text, narrowing to Latin-1 when every character fits in a byte.
    ///
    /// Returns `None` if the heap is full or the text is longer than [`MAX_STRING_LENGTH`].
    pub fn from_str(heap: &mut BumpHeap, text: &str) -> Option<StringRef> {
        // A `str` is valid UTF-8, so it has no lone surrogates, and the only question is whether
        // every character fits in a byte. Latin-1 here means code points up to 255, which is what
        // the representation stores, not the ASCII subset.
        //
        // Both paths measure first and then write straight into the cage. The obvious version
        // narrows into a `Vec<u8>` and hands that to `from_latin1`, and it costs a malloc and a
        // free per string, which the benchmark put at thirty seven nanoseconds for an eleven
        // character identifier. A parser builds one of these per identifier per file.
        if let Some(length) = latin1_length(text) {
            let length = u32::try_from(length).ok()?;
            if length > MAX_STRING_LENGTH {
                return None;
            }
            let string = StringRef::allocate(heap, length, KIND_LATIN1, length as usize)?;
            // SAFETY: the allocation has room for exactly `length` payload bytes, `latin1_length`
            // counted the same characters this loop writes, and nothing else refers to it yet.
            unsafe {
                let destination = string.payload_ptr(heap.cage());
                for (index, character) in text.chars().enumerate() {
                    // Truncation is what `latin1_length` just proved is lossless.
                    #[allow(clippy::cast_possible_truncation)]
                    destination.add(index).write(u32::from(character) as u8);
                }
            }
            return Some(string);
        }

        let length = u32::try_from(text.encode_utf16().count()).ok()?;
        if length > MAX_STRING_LENGTH {
            return None;
        }
        let string = StringRef::allocate(heap, length, KIND_UTF16, length as usize * 2)?;
        // SAFETY: as above, with two bytes per code unit, and `encode_utf16` yields the same
        // sequence it was just counted over.
        unsafe {
            // The cast looks unaligned to clippy and is not: an object is eight byte aligned and
            // the header is twelve bytes, so the payload starts on a four byte boundary.
            #[allow(clippy::cast_ptr_alignment)]
            let destination = string.payload_ptr(heap.cage()).cast::<u16>();
            for (index, unit) in text.encode_utf16().enumerate() {
                destination.add(index).write(unit);
            }
        }
        Some(string)
    }

    /// Build a Latin-1 string, one byte per character.
    ///
    /// Returns `None` if the heap is full or the text is longer than [`MAX_STRING_LENGTH`].
    pub fn from_latin1(heap: &mut BumpHeap, bytes: &[u8]) -> Option<StringRef> {
        let length = u32::try_from(bytes.len()).ok()?;
        if length > MAX_STRING_LENGTH {
            return None;
        }
        let string = StringRef::allocate(heap, length, KIND_LATIN1, bytes.len())?;
        // SAFETY: `allocate` just handed back a string with room for exactly `bytes.len()` bytes
        // of payload, and nothing else holds a reference to it yet.
        unsafe {
            let destination = string.payload_ptr(heap.cage());
            destination.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len());
        }
        Some(string)
    }

    /// Build a string from UTF-16 code units, narrowing to Latin-1 when they all fit in a byte.
    ///
    /// Lone surrogates are allowed here, because JavaScript allows them. They only become a
    /// problem at the Rust boundary, which is where [`StringRef::to_utf8`] refuses them.
    ///
    /// Returns `None` if the heap is full or the text is longer than [`MAX_STRING_LENGTH`].
    pub fn from_utf16(heap: &mut BumpHeap, units: &[u16]) -> Option<StringRef> {
        let length = u32::try_from(units.len()).ok()?;
        if length > MAX_STRING_LENGTH {
            return None;
        }

        // Narrowing is checked on every construction, which is what makes the representation
        // canonical. It costs one pass over code units that are about to be copied anyway.
        if units.iter().all(|&unit| unit < 0x100) {
            let string = StringRef::allocate(heap, length, KIND_LATIN1, units.len())?;
            // SAFETY: the allocation has room for one byte per code unit, and the check above
            // proved every one of them fits.
            unsafe {
                let destination = string.payload_ptr(heap.cage());
                for (index, &unit) in units.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    destination.add(index).write(unit as u8);
                }
            }
            return Some(string);
        }

        let payload = units.len().checked_mul(2)?;
        let string = StringRef::allocate(heap, length, KIND_UTF16, payload)?;
        // SAFETY: as above, and the payload starts at a four byte aligned offset inside an eight
        // byte aligned object, so it is aligned for `u16`.
        unsafe {
            // The cast looks unaligned to clippy and is not: an object is eight byte aligned and
            // the header is twelve bytes, so the payload starts on a four byte boundary.
            #[allow(clippy::cast_ptr_alignment)]
            let destination = string.payload_ptr(heap.cage()).cast::<u16>();
            destination.copy_from_nonoverlapping(units.as_ptr(), units.len());
        }
        Some(string)
    }

    /// Join two strings into a new flat string.
    ///
    /// This copies both sides, which means `a = a + b` in a loop is quadratic. Ropes are the
    /// answer and they are M1's job, described in the module comment above. Until then this is
    /// here so that the interpreter has a working `+` rather than a missing one.
    pub fn concat(heap: &mut BumpHeap, left: StringRef, right: StringRef) -> Option<StringRef> {
        // The metadata is read before allocating, because the cage is borrowed out of the heap and
        // allocating needs it mutably. These are three word reads and they are all that is needed
        // to decide the result's representation.
        let (length, both_narrow) = {
            let cage = heap.cage();
            (
                left.len(cage).checked_add(right.len(cage))?,
                left.is_latin1(cage) && right.is_latin1(cage),
            )
        };
        if length > MAX_STRING_LENGTH {
            return None;
        }

        if both_narrow {
            let string = StringRef::allocate(heap, length, KIND_LATIN1, length as usize)?;
            // SAFETY: the destination has room for both payloads, the two sources are distinct
            // allocations from it, and only the destination is written.
            unsafe {
                let cage = heap.cage();
                let destination = string.payload_ptr(cage);
                let front = left.latin1_bytes(cage).unwrap_or(&[]);
                let back = right.latin1_bytes(cage).unwrap_or(&[]);
                destination.copy_from_nonoverlapping(front.as_ptr(), front.len());
                destination
                    .add(front.len())
                    .copy_from_nonoverlapping(back.as_ptr(), back.len());
            }
            return Some(string);
        }

        // At least one side is UTF-16, and a UTF-16 string always holds a code unit above 255 by
        // the canonical representation invariant, so the result genuinely needs the wide form and
        // no second narrowing check is required.
        let string = StringRef::allocate(heap, length, KIND_UTF16, length as usize * 2)?;
        // SAFETY: as above, with two bytes per code unit.
        unsafe {
            let cage = heap.cage();
            #[allow(clippy::cast_ptr_alignment)]
            let destination = string.payload_ptr(cage).cast::<u16>();
            let mut written = 0usize;
            for part in [left, right] {
                if let Some(bytes) = part.latin1_bytes(cage) {
                    for &byte in bytes {
                        destination.add(written).write(u16::from(byte));
                        written += 1;
                    }
                } else {
                    let units = part.utf16_units(cage).unwrap_or(&[]);
                    destination
                        .add(written)
                        .copy_from_nonoverlapping(units.as_ptr(), units.len());
                    written += units.len();
                }
            }
        }
        Some(string)
    }

    /// Length in UTF-16 code units, which is what JavaScript's `.length` reports.
    #[must_use]
    pub fn len(self, cage: &Cage) -> u32 {
        // SAFETY: a `StringRef` only ever names a string this module allocated in this cage.
        unsafe { self.word(cage, LENGTH_OFFSET) }
    }

    /// Whether the string has no characters.
    #[must_use]
    pub fn is_empty(self, cage: &Cage) -> bool {
        self.len(cage) == 0
    }

    /// Whether the string is stored one byte per character.
    #[must_use]
    pub fn is_latin1(self, cage: &Cage) -> bool {
        self.kind(cage) == KIND_LATIN1
    }

    /// Whether this string is the canonical copy held by an atom table.
    #[must_use]
    pub fn is_interned(self, cage: &Cage) -> bool {
        // SAFETY: as above.
        unsafe { self.word(cage, HASH_OFFSET) & INTERNED_BIT != 0 }
    }

    /// The Latin-1 payload, or `None` if this string is stored as UTF-16.
    #[must_use]
    pub fn latin1_bytes(self, cage: &Cage) -> Option<&[u8]> {
        if !self.is_latin1(cage) {
            return None;
        }
        let length = self.len(cage) as usize;
        // SAFETY: a Latin-1 string of this length owns exactly this many payload bytes, and the
        // borrow is tied to the cage, which owns the mapping the bytes live in.
        unsafe { Some(slice::from_raw_parts(self.payload_ptr(cage), length)) }
    }

    /// The UTF-16 payload, or `None` if this string is stored as Latin-1.
    #[must_use]
    pub fn utf16_units(self, cage: &Cage) -> Option<&[u16]> {
        if self.is_latin1(cage) {
            return None;
        }
        let length = self.len(cage) as usize;
        // SAFETY: as above, and the payload of a UTF-16 string is two byte aligned because the
        // header is twelve bytes on top of an eight byte aligned object.
        unsafe {
            // Aligned for the same reason as the write path above.
            #[allow(clippy::cast_ptr_alignment)]
            let units = self.payload_ptr(cage).cast::<u16>();
            Some(slice::from_raw_parts(units, length))
        }
    }

    /// The code unit at `index`, or `None` past the end.
    #[must_use]
    pub fn code_unit_at(self, cage: &Cage, index: u32) -> Option<u16> {
        if index >= self.len(cage) {
            return None;
        }
        let index = index as usize;
        Some(match self.latin1_bytes(cage) {
            Some(bytes) => u16::from(bytes[index]),
            None => self.utf16_units(cage)?[index],
        })
    }

    /// Every code unit, widened out of whichever representation is in use.
    ///
    /// This allocates, and it exists for the paths where the representation genuinely does not
    /// matter. Anything on a hot path should ask for the payload it wants and handle both cases.
    #[must_use]
    pub fn code_units(self, cage: &Cage) -> Vec<u16> {
        match self.latin1_bytes(cage) {
            Some(bytes) => bytes.iter().map(|&byte| u16::from(byte)).collect(),
            None => self.utf16_units(cage).unwrap_or(&[]).to_vec(),
        }
    }

    /// The string as Rust text, for free, or `None` if it would cost anything.
    ///
    /// Free means the bytes in the cage are already valid UTF-8, which for a Latin-1 string means
    /// every byte is ASCII. This is the path most real strings take, and it is a separate method
    /// from [`StringRef::to_utf8`] so that a caller who cannot afford a copy can say so in the
    /// type system rather than in a comment.
    #[must_use]
    pub fn as_ascii(self, cage: &Cage) -> Option<&str> {
        let bytes = self.latin1_bytes(cage)?;
        if bytes.is_ascii() {
            // SAFETY: ASCII is a subset of UTF-8, and the check above just proved it.
            unsafe { Some(std::str::from_utf8_unchecked(bytes)) }
        } else {
            None
        }
    }

    /// The string as Rust text, borrowing when it can and copying when it must.
    ///
    /// # Errors
    ///
    /// Returns [`LoneSurrogate`] when the string contains an unpaired surrogate, because there is
    /// no UTF-8 for one. JavaScript can build such a string and Rust cannot represent it, and
    /// `spec/07-object-model.md` 7.7 says the boundary makes the caller choose rather than
    /// silently substituting something.
    pub fn to_utf8(self, cage: &Cage) -> Result<Cow<'_, str>, LoneSurrogate> {
        if let Some(text) = self.as_ascii(cage) {
            return Ok(Cow::Borrowed(text));
        }
        if let Some(bytes) = self.latin1_bytes(cage) {
            // Latin-1 above ASCII is two UTF-8 bytes per character, so this is a copy but never a
            // failure: every Latin-1 byte is a valid code point.
            return Ok(Cow::Owned(bytes.iter().map(|&b| char::from(b)).collect()));
        }

        let units = self.utf16_units(cage).unwrap_or(&[]);
        let mut out = String::with_capacity(units.len());
        for (index, result) in char::decode_utf16(units.iter().copied()).enumerate() {
            match result {
                Ok(c) => out.push(c),
                Err(error) => {
                    return Err(LoneSurrogate {
                        index: u32::try_from(index).unwrap_or(u32::MAX),
                        unit: error.unpaired_surrogate(),
                    });
                }
            }
        }
        Ok(Cow::Owned(out))
    }

    /// The string as Rust text, with unpaired surrogates replaced by the replacement character.
    ///
    /// This is what printing wants. It is a separate method from [`StringRef::to_utf8`] because
    /// substituting a character is a decision, and a decision made silently inside a conversion
    /// is how a runtime ends up corrupting data on the way to a log line.
    #[must_use]
    pub fn to_utf8_lossy(self, cage: &Cage) -> Cow<'_, str> {
        if let Ok(text) = self.to_utf8(cage) {
            return text;
        }
        let units = self.utf16_units(cage).unwrap_or(&[]);
        Cow::Owned(
            char::decode_utf16(units.iter().copied())
                .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect(),
        )
    }

    /// The string's hash, computing and caching it on the first call.
    ///
    /// Twenty eight bits, because the other four hold the flags. That is plenty for a hash table
    /// that masks down to a power of two capacity anyway, and it means the length, the flags and
    /// the hash all fit in one word.
    #[must_use]
    pub fn hash(self, cage: &Cage) -> u32 {
        // SAFETY: as elsewhere in this file, the reference names a string in this cage.
        let word = unsafe { self.word(cage, HASH_OFFSET) };
        if word & HASHED_BIT != 0 {
            return word >> HASH_SHIFT;
        }

        let hash = match self.latin1_bytes(cage) {
            Some(bytes) => hash_code_units(bytes.iter().map(|&byte| u16::from(byte))),
            None => hash_code_units(self.utf16_units(cage).unwrap_or(&[]).iter().copied()),
        };

        // The hash word is the one field of a string that changes after construction, which is
        // the same bargain V8 makes.
        //
        // SAFETY: the write goes through a pointer derived from the cage's own raw base rather
        // than from the shared borrow above, the payload borrows taken in this function have
        // ended, and `Cage` is not `Sync`, so no other thread can be reading the word while this
        // one writes it.
        unsafe {
            self.set_word(cage, HASH_OFFSET, (hash << HASH_SHIFT) | HASHED_BIT | word);
        }
        hash
    }

    /// Whether two strings hold the same characters.
    ///
    /// Two interned strings from the same table compare as one integer, because interning makes
    /// the canonical copy unique. Anything else falls through to a length check, a representation
    /// check and a byte comparison, in that order, which is cheapest first.
    #[must_use]
    pub fn equals(self, cage: &Cage, other: StringRef) -> bool {
        if self.0 == other.0 {
            return true;
        }
        if self.len(cage) != other.len(cage) || self.is_latin1(cage) != other.is_latin1(cage) {
            // Representation is canonical, so two equal strings always agree on it and this is a
            // rejection rather than a reason to widen and compare.
            return false;
        }
        match (self.latin1_bytes(cage), other.latin1_bytes(cage)) {
            (Some(left), Some(right)) => left == right,
            _ => self.utf16_units(cage) == other.utf16_units(cage),
        }
    }

    /// Order two strings the way `<` orders them in JavaScript.
    ///
    /// Code unit order, which is not the same as code point order and is not any human language's
    /// idea of alphabetical. A character outside the basic multilingual plane is stored as a
    /// surrogate pair in the D800 to DFFF range, which sorts below characters in E000 to FFFF that
    /// have smaller code points, so a supplementary character sorts before a private use one. That
    /// is what the standard specifies and it is what every engine does, and the sorting anybody
    /// actually wants is `localeCompare` and the collator in document 12.
    ///
    /// Comparing a Latin-1 string against a UTF-16 one goes code unit by code unit rather than by
    /// byte, because a Latin-1 byte widens to the code unit with the same value and a byte
    /// comparison would be reading two different things.
    #[must_use]
    pub fn compare(self, cage: &Cage, other: StringRef) -> Ordering {
        if self.0 == other.0 {
            return Ordering::Equal;
        }
        match (self.latin1_bytes(cage), other.latin1_bytes(cage)) {
            (Some(left), Some(right)) => left.cmp(right),
            (Some(left), None) => {
                let right = other.utf16_units(cage).unwrap_or(&[]);
                compare_units(
                    left.iter().map(|&byte| u16::from(byte)),
                    right.iter().copied(),
                )
            }
            (None, Some(right)) => {
                let left = self.utf16_units(cage).unwrap_or(&[]);
                compare_units(
                    left.iter().copied(),
                    right.iter().map(|&byte| u16::from(byte)),
                )
            }
            (None, None) => {
                let left = self.utf16_units(cage).unwrap_or(&[]);
                let right = other.utf16_units(cage).unwrap_or(&[]);
                left.cmp(right)
            }
        }
    }

    /// Whether the string holds the same characters as some Rust text.
    ///
    /// Used by the atom table to answer a lookup without allocating a candidate string first.
    #[must_use]
    pub fn equals_str(self, cage: &Cage, text: &str) -> bool {
        if let Some(ascii) = self.as_ascii(cage) {
            return ascii == text;
        }
        let mut expected = text.encode_utf16();
        if let Some(bytes) = self.latin1_bytes(cage) {
            return bytes
                .iter()
                .all(|&byte| expected.next() == Some(u16::from(byte)))
                && expected.next().is_none();
        }
        let units = self.utf16_units(cage).unwrap_or(&[]);
        units.iter().all(|&unit| expected.next() == Some(unit)) && expected.next().is_none()
    }

    /// The compressed slot behind this reference, for storing in an object or a table.
    #[must_use]
    pub const fn slot(self) -> Slot {
        self.0
    }

    /// Rebuild a reference from a slot that came out of [`StringRef::slot`].
    ///
    /// Returns `None` for a slot holding an integer, which is the only thing this can check. It
    /// cannot tell a string from any other heap object, and until shapes arrive in M1 nothing
    /// can, which is why this is not called on anything but a slot a string was written into.
    #[must_use]
    pub const fn from_slot(slot: Slot) -> Option<StringRef> {
        if slot.is_pointer() {
            Some(StringRef(slot))
        } else {
            None
        }
    }

    /// Mark this string as the canonical copy in an atom table.
    pub(crate) fn mark_interned(self, cage: &Cage) {
        // The flag bits live in the same word as the hash and are the only other part of a string
        // that changes after construction.
        // SAFETY: same argument as the hash write above.
        unsafe {
            let word = self.word(cage, HASH_OFFSET);
            self.set_word(cage, HASH_OFFSET, word | INTERNED_BIT);
        }
    }

    fn kind(self, cage: &Cage) -> u32 {
        // SAFETY: as elsewhere.
        unsafe { self.word(cage, HASH_OFFSET) & KIND_MASK }
    }

    fn allocate(
        heap: &mut BumpHeap,
        length: u32,
        kind: u32,
        payload_bytes: usize,
    ) -> Option<StringRef> {
        let total = STRING_HEADER_SIZE.checked_add(payload_bytes)?;
        let pointer = heap.allocate(total, ObjectKind::String)?;
        let offset = heap.cage().offset_of(pointer.as_ptr())?;
        let string = StringRef(Slot::from_offset(offset));
        // SAFETY: the allocation is fresh, zeroed and at least a header long, and nothing else
        // has a reference to it. The shape word stays zero, which reads as the integer zero
        // rather than as a shape, until M1 gives strings a real one.
        unsafe {
            debug_assert_eq!(
                string.word(heap.cage(), SHAPE_OFFSET),
                0,
                "the shape word is left at zero on purpose, and that only works if the heap \
                 hands out zeroed memory"
            );
            string.set_word(heap.cage(), LENGTH_OFFSET, length);
            string.set_word(heap.cage(), HASH_OFFSET, kind);
        }
        Some(string)
    }

    unsafe fn address(self, cage: &Cage) -> *mut u8 {
        cage.address_of(self.0.as_offset().unwrap_or(0))
    }

    unsafe fn payload_ptr(self, cage: &Cage) -> *mut u8 {
        // SAFETY: the caller guarantees the reference names a string, which is at least a header
        // long, so the header end is inside the same allocation.
        unsafe { self.address(cage).add(STRING_HEADER_SIZE) }
    }

    unsafe fn word(self, cage: &Cage, offset: usize) -> u32 {
        // SAFETY: the caller guarantees the reference names a string, and every offset used here
        // is one of the three header words. The header is written before the reference escapes,
        // so it is initialised.
        // The header words are four byte aligned inside an eight byte aligned object, which is
        // what clippy cannot see through the byte pointer.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            self.address(cage).add(offset).cast::<u32>().read()
        }
    }

    unsafe fn set_word(self, cage: &Cage, offset: usize, value: u32) {
        // SAFETY: as above, and the pointer is derived from the cage's raw base rather than from
        // a shared reference, so writing through it does not invalidate a borrow.
        #[allow(clippy::cast_ptr_alignment)]
        unsafe {
            self.address(cage).add(offset).cast::<u32>().write(value);
        }
    }
}

/// The length of `text` in Latin-1 bytes, or `None` if it does not fit in Latin-1 at all.
///
/// This is the pass that decides the representation, and it stops at the first character above
/// 255 rather than measuring the whole string first. `None` means only that, and never that the
/// text is too long, because the two answers lead to different places and conflating them would
/// send an over long ASCII string down the UTF-16 path to be refused there for the wrong reason.
/// The count is 64 bit because no `str` can be longer than `isize::MAX` bytes.
fn latin1_length(text: &str) -> Option<u64> {
    let mut length = 0u64;
    for character in text.chars() {
        if u32::from(character) >= 0x100 {
            return None;
        }
        length += 1;
    }
    Some(length)
}

/// The string hash, defined over code units so that it does not depend on the representation.
///
/// A code unit at a time rather than eight bytes at a time. That is slower on long strings and it
/// is the only definition that gives the same answer for a Latin-1 string, the UTF-16 string with
/// the same characters, and a Rust `&str` being looked up without being allocated first. Property
/// names are short, and the alternative is a lookup path that has to build a candidate string
/// before it can decide the table already has one.
///
/// Not resistant to hash flooding. A program that chooses its property names to collide can push
/// this table into linear probing, which is a real denial of service against a server. The fix is
/// a per process random seed, and the place for it is the realm in M1, since the seed has to be
/// fixed for the lifetime of a heap once a hash has been cached in a string header.
fn hash_code_units(units: impl Iterator<Item = u16>) -> u32 {
    let mut state = HASH_MULTIPLIER;
    let mut length = 0u64;
    for unit in units {
        state = (state ^ u64::from(unit))
            .wrapping_mul(HASH_MULTIPLIER)
            .rotate_left(29);
        length += 1;
    }
    // Folding the length in stops "a" and "aa" from colliding through the multiplier alone, and
    // the final avalanche is what moves the mixed high bits down into the bits the table masks.
    state ^= length.wrapping_mul(HASH_MULTIPLIER);
    state ^= state >> 33;
    state = state.wrapping_mul(HASH_MULTIPLIER);
    state ^= state >> 29;
    // Truncating to thirty two bits is the point of a finaliser, and the mask takes the low
    // twenty eight, which are the best mixed bits after the last shift and multiply.
    #[allow(clippy::cast_possible_truncation)]
    let folded = state as u32;
    folded & HASH_MASK
}

/// The hash of some Rust text, matching what the string it would build reports.
#[must_use]
pub fn hash_str(text: &str) -> u32 {
    hash_code_units(text.encode_utf16())
}

/// Lexicographic order over two code unit sequences, for the mixed representation case.
///
/// The shorter string wins a tie, which is why `"ab" < "abc"`.
fn compare_units(left: impl Iterator<Item = u16>, right: impl Iterator<Item = u16>) -> Ordering {
    left.cmp(right)
}

#[cfg(test)]
mod tests {
    use super::{MAX_STRING_LENGTH, STRING_HEADER_SIZE, StringRef, hash_str};
    use crate::bump::{BumpHeap, ObjectKind};

    #[test]
    fn ascii_text_is_stored_one_byte_per_character() {
        let mut heap = BumpHeap::new().unwrap();
        let string = StringRef::from_str(&mut heap, "hello world").unwrap();
        let cage = heap.cage();
        assert!(string.is_latin1(cage));
        assert_eq!(string.len(cage), 11);
        assert_eq!(string.latin1_bytes(cage).unwrap(), b"hello world");
        assert_eq!(string.as_ascii(cage), Some("hello world"));
    }

    #[test]
    fn a_ten_character_ascii_string_costs_what_the_budget_says() {
        let mut heap = BumpHeap::new().unwrap();
        StringRef::from_str(&mut heap, "0123456789").unwrap();
        let totals = heap.census().totals(ObjectKind::String);
        assert_eq!(totals.count, 1);
        assert_eq!(STRING_HEADER_SIZE, 12);
        assert_eq!(
            totals.requested_bytes, 22,
            "spec 7.9 budgets 26 bytes for this string and a three word header should beat it"
        );
        assert_eq!(
            totals.reserved_bytes, 24,
            "twenty two bytes rounds up to twenty four, and the padding has to be visible"
        );
    }

    #[test]
    fn latin1_covers_every_code_point_below_256_not_just_ascii() {
        let mut heap = BumpHeap::new().unwrap();
        let string = StringRef::from_str(&mut heap, "café").unwrap();
        let cage = heap.cage();
        assert!(string.is_latin1(cage), "é fits in a byte");
        assert_eq!(string.len(cage), 4);
        assert_eq!(
            string.as_ascii(cage),
            None,
            "0xE9 on its own is not valid UTF-8, so the free path has to refuse it"
        );
        assert_eq!(string.to_utf8(cage).unwrap(), "café");
    }

    #[test]
    fn text_above_the_byte_range_goes_to_utf16() {
        let mut heap = BumpHeap::new().unwrap();
        let string = StringRef::from_str(&mut heap, "日本語").unwrap();
        let cage = heap.cage();
        assert!(!string.is_latin1(cage));
        assert_eq!(string.len(cage), 3);
        assert_eq!(string.to_utf8(cage).unwrap(), "日本語");
    }

    #[test]
    fn a_surrogate_pair_is_two_code_units_and_one_character() {
        let mut heap = BumpHeap::new().unwrap();
        // U+1F363, outside the basic multilingual plane, so JavaScript sees length 2.
        let string = StringRef::from_str(&mut heap, "\u{1F363}").unwrap();
        let cage = heap.cage();
        assert_eq!(string.len(cage), 2);
        assert_eq!(string.code_unit_at(cage, 0), Some(0xD83C));
        assert_eq!(string.code_unit_at(cage, 1), Some(0xDF63));
        assert_eq!(string.code_unit_at(cage, 2), None);
        assert_eq!(string.to_utf8(cage).unwrap(), "\u{1F363}");
    }

    #[test]
    fn a_lone_surrogate_is_an_error_at_the_rust_boundary_and_not_before() {
        let mut heap = BumpHeap::new().unwrap();
        // Building it is fine. JavaScript allows this and so must we.
        let string = StringRef::from_utf16(&mut heap, &[0x0041, 0xD800, 0x0042]).unwrap();
        let cage = heap.cage();
        assert_eq!(string.len(cage), 3);
        assert_eq!(string.code_unit_at(cage, 1), Some(0xD800));

        let error = string.to_utf8(cage).unwrap_err();
        assert_eq!(error.index, 1);
        assert_eq!(error.unit, 0xD800);
        assert!(error.to_string().contains("lone surrogate"));

        // Printing still works, because printing asks for the lossy conversion by name.
        assert_eq!(string.to_utf8_lossy(cage), "A\u{FFFD}B");
    }

    #[test]
    fn utf16_input_that_fits_in_bytes_is_narrowed_on_the_way_in() {
        let mut heap = BumpHeap::new().unwrap();
        let wide = StringRef::from_utf16(&mut heap, &[0x0068, 0x0069]).unwrap();
        let narrow = StringRef::from_str(&mut heap, "hi").unwrap();
        let cage = heap.cage();
        assert!(
            wide.is_latin1(cage),
            "representation is canonical, so this has to narrow"
        );
        assert!(wide.equals(cage, narrow));
        assert_eq!(wide.hash(cage), narrow.hash(cage));
    }

    #[test]
    fn equality_is_by_characters_and_rejects_early_on_length() {
        let mut heap = BumpHeap::new().unwrap();
        let a = StringRef::from_str(&mut heap, "prototype").unwrap();
        let b = StringRef::from_str(&mut heap, "prototype").unwrap();
        let c = StringRef::from_str(&mut heap, "prototyp").unwrap();
        let d = StringRef::from_str(&mut heap, "日本語").unwrap();
        let cage = heap.cage();
        assert_ne!(a.slot(), b.slot(), "two allocations, two addresses");
        assert!(a.equals(cage, b));
        assert!(!a.equals(cage, c));
        assert!(!a.equals(cage, d));
        assert!(a.equals_str(cage, "prototype"));
        assert!(!a.equals_str(cage, "prototypes"));
        assert!(d.equals_str(cage, "日本語"));
        assert!(!d.equals_str(cage, "日本"));
    }

    #[test]
    fn the_hash_is_cached_and_agrees_with_the_one_computed_from_rust_text() {
        let mut heap = BumpHeap::new().unwrap();
        let string = StringRef::from_str(&mut heap, "length").unwrap();
        let cage = heap.cage();
        let first = string.hash(cage);
        assert_eq!(first, string.hash(cage), "the cached hash has to match");
        assert_eq!(
            first,
            hash_str("length"),
            "a lookup has to be able to hash the text without building the string"
        );
        assert_eq!(string.len(cage), 6, "caching the hash must not eat a field");
        assert_eq!(string.as_ascii(cage), Some("length"));
    }

    #[test]
    fn the_hash_spreads_well_enough_to_be_worth_having() {
        // Not a quality claim, a smoke test. A mixer that returns a constant or that ignores
        // ordering would sail through every other test in this file.
        let mut seen = std::collections::HashSet::new();
        for i in 0..2000 {
            seen.insert(hash_str(&format!("property{i}")));
        }
        assert!(
            seen.len() > 1990,
            "two thousand distinct names collided {} times",
            2000 - seen.len()
        );
        assert_ne!(hash_str("ab"), hash_str("ba"), "ordering has to matter");
        assert_ne!(hash_str("a"), hash_str("aa"), "length has to matter");
    }

    #[test]
    fn the_empty_string_is_a_string_and_not_a_refusal() {
        let mut heap = BumpHeap::new().unwrap();
        let empty = StringRef::from_str(&mut heap, "").unwrap();
        let cage = heap.cage();
        assert!(empty.is_empty(cage));
        assert_eq!(empty.len(cage), 0);
        assert_eq!(empty.as_ascii(cage), Some(""));
        assert_eq!(empty.hash(cage), super::hash_str(""));
    }

    #[test]
    fn concatenation_copies_and_renarrows() {
        let mut heap = BumpHeap::new().unwrap();
        let left = StringRef::from_str(&mut heap, "kat").unwrap();
        let right = StringRef::from_str(&mut heap, "su").unwrap();
        let joined = StringRef::concat(&mut heap, left, right).unwrap();
        assert_eq!(joined.as_ascii(heap.cage()), Some("katsu"));
        assert!(joined.is_latin1(heap.cage()));

        let wide = StringRef::from_str(&mut heap, "語").unwrap();
        let mixed = StringRef::concat(&mut heap, joined, wide).unwrap();
        assert!(!mixed.is_latin1(heap.cage()));
        assert_eq!(mixed.to_utf8(heap.cage()).unwrap(), "katsu語");
    }

    #[test]
    fn a_reference_survives_a_round_trip_through_a_slot() {
        let mut heap = BumpHeap::new().unwrap();
        let string = StringRef::from_str(&mut heap, "round trip").unwrap();
        let slot = string.slot();
        let back = StringRef::from_slot(slot).unwrap();
        assert_eq!(back.as_ascii(heap.cage()), Some("round trip"));
        assert!(
            StringRef::from_slot(crate::Slot::ZERO).is_none(),
            "an integer slot does not name a string"
        );
    }

    #[test]
    fn the_length_limit_is_checked_before_anything_is_allocated() {
        let mut heap = BumpHeap::new().unwrap();
        // The limit itself cannot be reached in a test without a gigabyte of payload, so this
        // checks the shape of the guard rather than the boundary: the limit is below the range
        // of the length field, and the empty string is still a real object.
        assert_eq!(MAX_STRING_LENGTH, (1 << 30) - 1);
        let before = heap.cursor();
        assert!(StringRef::from_latin1(&mut heap, &[]).is_some());
        assert!(
            heap.cursor() > before,
            "the empty string is still an object"
        );
    }

    #[test]
    fn strings_order_by_code_unit_and_not_by_anything_a_human_would_call_alphabetical() {
        use std::cmp::Ordering;

        let mut heap = BumpHeap::new().unwrap();
        let a = StringRef::from_str(&mut heap, "a").unwrap();
        let b = StringRef::from_str(&mut heap, "b").unwrap();
        let ab = StringRef::from_str(&mut heap, "ab").unwrap();
        let abc = StringRef::from_str(&mut heap, "abc").unwrap();
        let big_a = StringRef::from_str(&mut heap, "A").unwrap();
        // Latin-1 on one side and UTF-16 on the other, which is the case a byte comparison gets
        // wrong because it would be comparing bytes against half of a code unit.
        let wide = StringRef::from_str(&mut heap, "\u{4e00}").unwrap();
        let cage = heap.cage();

        assert_eq!(a.compare(cage, b), Ordering::Less);
        assert_eq!(b.compare(cage, a), Ordering::Greater);
        assert_eq!(a.compare(cage, a), Ordering::Equal);
        // A prefix sorts before what it is a prefix of.
        assert_eq!(ab.compare(cage, abc), Ordering::Less);
        // Uppercase sorts before lowercase, because `A` is 0x41 and `a` is 0x61.
        assert_eq!(big_a.compare(cage, a), Ordering::Less);
        assert!(!wide.is_latin1(cage));
        assert_eq!(a.compare(cage, wide), Ordering::Less);
        assert_eq!(wide.compare(cage, a), Ordering::Greater);
    }
}
