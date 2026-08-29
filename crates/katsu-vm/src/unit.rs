//! A blueprint tree, flattened into something a call can index into.
//!
//! Lowering produces functions nested inside one another, the way they were written, and
//! `NewClosure` names its target by position among its parent's children. That shape is right for
//! the compiler and wrong for the interpreter, because a closure has to remember which function it
//! runs and "the second child of the first child of the top level" is not a number.
//!
//! So the tree is walked once before the first instruction runs and laid out flat. Every function
//! gets an index, a closure stores that index, and a call is an array lookup. The walk also does the
//! constant resolution that used to happen for the top level only, which is the pass that turns the
//! Rust strings lowering left in the pool into interned strings in this isolate's heap.
//!
//! # Why this is not called a module
//!
//! Because it is not one. There is no module system here yet, nothing is imported, and everything in
//! this structure came out of one file. Calling it a module would be claiming a boundary that does
//! not exist. A unit is what one compilation produced, which is the only thing that is true today,
//! and when modules arrive a module will own units rather than being renamed into one.
//!
//! # Where this belongs eventually
//!
//! Doing the flatten and the interning on every `run` is the shape and not the destination. Both are
//! load time work, once per unit per realm, and they move there when there is a realm to load into.
//! A program that runs one unit once, which is every program M0 can run, cannot tell the difference.

use katsu_ir::FunctionBlueprint;

use crate::Value;
use crate::cache::Caches;

/// What resolving one function against a heap produced.
///
/// A struct rather than a tuple because the two halves are unrelated. The constants are what the
/// code indexes into and the name is what a closure over this function carries so that printing it
/// works after the unit is gone.
#[derive(Debug)]
pub struct Resolved {
    /// The constant pool, as values in the isolate's heap.
    pub constants: Vec<Value>,
    /// The function's name, or the empty value for a function written without one.
    pub name: Value,
}

impl Default for Resolved {
    /// Nothing resolved and no name, which is what a function with an empty pool starts as.
    fn default() -> Resolved {
        Resolved {
            constants: Vec::new(),
            name: Value::EMPTY,
        }
    }
}

/// One function, ready to run.
#[derive(Debug)]
pub struct Loaded<'a> {
    /// The code and the metadata, still owned by whoever compiled it.
    pub blueprint: &'a FunctionBlueprint,
    /// The constant pool, resolved into values in this isolate's heap.
    pub constants: Vec<Value>,
    /// The function's name, interned once here rather than at every closure that mentions it.
    pub name: Value,
    /// Where each of this function's nested functions ended up in the flat list.
    pub children: Vec<u32>,
    /// One inline cache per access site in this function, shared by every frame running it.
    pub caches: Caches,
}

/// Every function one compilation produced, flattened and indexed.
#[derive(Debug)]
pub struct Unit<'a> {
    functions: Vec<Loaded<'a>>,
}

impl<'a> Unit<'a> {
    /// Build a unit from a root blueprint, resolving each function's constants with `resolve`.
    ///
    /// The walk is breadth first, which is not a meaningful choice beyond keeping the top level at
    /// index zero. What matters is that a parent is visited before its children, so that a child's
    /// index is known by the time the parent's `children` list is written.
    ///
    /// # Errors
    ///
    /// Whatever `resolve` returns, which in practice is running out of heap while interning a string
    /// literal.
    pub fn load<E>(
        root: &'a FunctionBlueprint,
        mut resolve: impl FnMut(&FunctionBlueprint) -> Result<Resolved, E>,
    ) -> Result<Unit<'a>, E> {
        let resolved = resolve(root)?;
        let mut functions = vec![Loaded {
            blueprint: root,
            constants: resolved.constants,
            name: resolved.name,
            children: Vec::new(),
            caches: Caches::new(root.cache_slots),
        }];
        let mut at = 0;
        while at < functions.len() {
            let blueprint = functions[at].blueprint;
            let mut children = Vec::with_capacity(blueprint.blueprints.len());
            for child in &blueprint.blueprints {
                // A unit with more than four billion functions in it is not a program, and the cast
                // is the only place the flat index could ever be wrong, so it is checked here rather
                // than assumed everywhere it is used.
                let index = u32::try_from(functions.len())
                    .expect("a compilation unit has fewer than four billion functions in it");
                children.push(index);
                let resolved = resolve(child)?;
                functions.push(Loaded {
                    blueprint: child,
                    constants: resolved.constants,
                    name: resolved.name,
                    children: Vec::new(),
                    caches: Caches::new(child.cache_slots),
                });
            }
            functions[at].children = children;
            at += 1;
        }
        Ok(Unit { functions })
    }

    /// One function by its flat index.
    ///
    /// # Panics
    ///
    /// Panics on an index this unit does not contain, which can only come from a fabricated closure
    /// or from a unit other than the one the closure was made in. Neither is something a JavaScript
    /// program can cause.
    #[must_use]
    pub fn function(&self, index: u32) -> &Loaded<'a> {
        self.functions
            .get(index as usize)
            .expect("a closure only ever names a function in the unit it was made in")
    }

    /// The flat index of the `child`th function written inside function `parent`.
    ///
    /// # Panics
    ///
    /// Panics if the blueprint index is not one of the parent's children, which means the bytecode
    /// and the blueprint tree disagree and lowering built one of them wrong.
    #[must_use]
    pub fn child(&self, parent: u32, child: u32) -> u32 {
        *self
            .function(parent)
            .children
            .get(child as usize)
            .expect("lowering only emits a blueprint index it wrote a nested function for")
    }

    /// How many functions this unit holds, counting the top level.
    #[must_use]
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    /// Whether this unit holds no functions, which cannot happen because the top level is one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Resolved, Unit};
    use crate::Value;

    fn compile(source: &str) -> katsu_ir::FunctionBlueprint {
        crate::compile("t.js", source).expect("should compile")
    }

    fn load(blueprint: &katsu_ir::FunctionBlueprint) -> Unit<'_> {
        Unit::load(blueprint, |_| Ok::<_, ()>(Resolved::default())).expect("nothing can fail")
    }

    #[test]
    fn a_program_with_no_functions_is_a_unit_of_one() {
        let blueprint = compile("let x = 1 + 2;");
        let unit = load(&blueprint);
        assert_eq!(unit.len(), 1);
        assert!(unit.function(0).children.is_empty());
    }

    #[test]
    fn nested_functions_are_flattened_and_reachable_from_their_parents() {
        let blueprint = compile(
            "function outer() { function first() { return 1; } function second() { return 2; } \
             return first() + second(); }",
        );
        let unit = load(&blueprint);
        assert_eq!(unit.len(), 4, "the top level, outer, first and second");
        let outer = unit.child(0, 0);
        assert_eq!(unit.function(outer).blueprint.name, "outer");
        assert_eq!(unit.function(unit.child(outer, 0)).blueprint.name, "first");
        assert_eq!(unit.function(unit.child(outer, 1)).blueprint.name, "second");
    }

    #[test]
    fn a_blueprint_index_is_relative_to_its_parent_and_not_to_the_unit() {
        // The reason the flat index exists at all. Both `outer` and `inner` say blueprint zero and
        // they mean different functions, so an interpreter that used the raw index would call the
        // wrong one.
        let blueprint = compile("function a() { function b() { return 1; } return b(); }");
        let unit = load(&blueprint);
        assert_eq!(unit.child(0, 0), 1);
        assert_eq!(unit.child(1, 0), 2);
        assert_eq!(unit.function(2).blueprint.name, "b");
    }

    #[test]
    fn constants_are_resolved_for_every_function_and_not_only_the_top_level() {
        let blueprint = compile("function greet() { return 'hello'; } greet();");
        let mut seen = 0;
        let unit = Unit::load(&blueprint, |bp| {
            seen += 1;
            Ok::<_, ()>(Resolved {
                constants: vec![Value::UNDEFINED; bp.constants.len()],
                name: Value::UNDEFINED,
            })
        })
        .expect("nothing can fail");
        assert_eq!(seen, 2);
        assert_eq!(unit.function(1).constants.len(), 1);
    }
}
