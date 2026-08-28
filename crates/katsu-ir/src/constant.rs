//! The per function constant pool.
//!
//! Spec 4.5 says constants live in a per function pool and that strings are interned isolate wide,
//! so every function that mentions `"length"` ends up sharing one atom. Only the first half of that
//! happens here. `katsu-parse` and `katsu-gc` are both at layer 2 and neither can depend on the
//! other, so the pass that builds this pool cannot reach the atom table, and the pool holds plain
//! Rust strings. Interning happens once when a realm loads the blueprint, which is the first moment
//! there is a heap to intern into. The pool is the list of things to intern and the index into it is
//! the operand, so the second half costs one walk at load time and nothing per execution.
//!
//! Note that spec 4.1.1 predicted interning would happen at lowering because lowering would sit
//! above both crates. It does not. Lowering sits in `katsu-parse` next to the tree it reads, and the
//! atom table stays out of reach, which is why the pool exists in this shape rather than holding
//! atoms directly.

use std::fmt;
use std::sync::Arc;

use rustc_hash::FxHashMap;

/// An index into a blueprint's constant pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConstIndex(pub u32);

/// A value known at lowering time.
///
/// Booleans, `null` and `undefined` are not here because each of them has its own opcode, which is
/// smaller than a pool entry plus a load and saves the pool from holding four values that every
/// function in the program would otherwise duplicate.
#[derive(Clone, Debug)]
pub enum Constant {
    /// A number, already converted from source text to the double it denotes.
    Number(f64),
    /// A string, with escapes resolved. Interned into an atom when a realm loads the blueprint.
    String(Arc<str>),
}

impl fmt::Display for Constant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value:?}"),
        }
    }
}

/// The constants one function needs, each stored once.
///
/// Deduplication is worth doing here rather than later because the same string literal appearing
/// twice in a function is normal, `o.x` and `o.x` in two places is the same key, and every duplicate
/// removed is one fewer atom to intern at load time.
#[derive(Clone, Debug, Default)]
pub struct ConstantPool {
    values: Vec<Constant>,
    /// Numbers are keyed by bit pattern rather than by value. That makes `0.0` and `-0.0` different
    /// entries, which is correct, because `1 / -0` is not `1 / 0`. It also makes every NaN with the
    /// same payload one entry, which is correct for the same reason it is uninteresting: source text
    /// cannot write a NaN with a payload.
    numbers: FxHashMap<u64, ConstIndex>,
    /// Strings share one allocation with the pool entry, the same trick the scope pass uses for
    /// declared names, so adding a string that is already there costs a hash and no copy.
    strings: FxHashMap<Arc<str>, ConstIndex>,
}

impl ConstantPool {
    /// Add a number, or return the index it already has.
    pub fn number(&mut self, value: f64) -> ConstIndex {
        let key = value.to_bits();
        if let Some(index) = self.numbers.get(&key) {
            return *index;
        }
        let index = self.push(Constant::Number(value));
        self.numbers.insert(key, index);
        index
    }

    /// Add a string, or return the index it already has.
    pub fn string(&mut self, value: &str) -> ConstIndex {
        if let Some(index) = self.strings.get(value) {
            return *index;
        }
        let shared: Arc<str> = Arc::from(value);
        let index = self.push(Constant::String(Arc::clone(&shared)));
        self.strings.insert(shared, index);
        index
    }

    /// Read a constant back, for the interpreter and for a disassembly.
    pub fn get(&self, index: ConstIndex) -> Option<&Constant> {
        self.values.get(index.0 as usize)
    }

    /// Every constant in pool order, which is the order a realm interns them in.
    pub fn values(&self) -> &[Constant] {
        &self.values
    }

    /// How many constants the pool holds.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the function needs no constants at all.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn push(&mut self, constant: Constant) -> ConstIndex {
        let index = ConstIndex(u32::try_from(self.values.len()).expect("a pool fits in u32"));
        self.values.push(constant);
        index
    }
}

#[cfg(test)]
mod tests {
    use super::{Constant, ConstantPool};

    #[test]
    fn the_same_string_twice_is_one_entry() {
        let mut pool = ConstantPool::default();
        let first = pool.string("length");
        let second = pool.string("length");

        assert_eq!(first, second);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn the_same_number_twice_is_one_entry() {
        let mut pool = ConstantPool::default();
        assert_eq!(pool.number(1.5), pool.number(1.5));
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn negative_zero_is_not_positive_zero() {
        // If these shared an entry then `1 / -0` would produce positive infinity, which is the kind
        // of bug that survives a whole test suite and then shows up in somebody's physics code.
        let mut pool = ConstantPool::default();
        let positive = pool.number(0.0);
        let negative = pool.number(-0.0);

        assert_ne!(positive, negative);
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn a_number_and_a_string_do_not_collide() {
        let mut pool = ConstantPool::default();
        pool.number(1.0);
        pool.string("1");
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn constants_come_back_in_the_order_they_were_added() {
        let mut pool = ConstantPool::default();
        pool.string("first");
        pool.number(2.0);
        pool.string("third");

        let index = pool.string("third");
        let Some(Constant::String(third)) = pool.get(index) else {
            panic!("expected the string back");
        };
        assert_eq!(&**third, "third");
        assert_eq!(pool.values().len(), 3);
    }

    #[test]
    fn a_string_constant_prints_quoted_so_a_disassembly_is_unambiguous() {
        let mut pool = ConstantPool::default();
        let index = pool.string("hello world");
        assert_eq!(
            pool.get(index).expect("just added").to_string(),
            "\"hello world\""
        );
    }
}
