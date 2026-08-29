//! `Object`, and the two of its statics that a prototype chain can be reached through.
//!
//! Every object a program makes now inherits from `Object.prototype`, and this is where a program
//! can see that: `Object.getPrototypeOf({})` is that object, `Object.create(p)` makes something that
//! inherits from `p`, and a property that is not on an object is looked for above it.
//!
//! # What is here
//!
//! `Object.prototype`, `Object.create` and `Object.getPrototypeOf`.
//!
//! # `typeof Object` says "object" here and "function" in Node
//!
//! This is a known wrong answer and it is worth stating plainly rather than leaving to be
//! discovered. `Object` is a constructor, which is to say a function with properties hanging off it,
//! and in this build a function written in Rust is a native reference rather than an object, so it
//! cannot carry `create` and `prototype`. The choice was between an `Object` that has the right type
//! tag and no properties, which is useless, and one that has the properties and the wrong tag, which
//! is what a program actually reaches for.
//!
//! It is fixed by functions becoming ordinary objects with a call behaviour, which is the same piece
//! of work as `new` and as `Function.prototype`, and it is next.
//!
//! # What is not here
//!
//! `Object(x)` as a call, for the same reason: there is nothing to call.
//!
//! `new Object()`, which needs `new`.
//!
//! Anything on `Object.prototype`. Not an oversight and not laziness: there are no property
//! attributes yet, so a `toString` installed there would be enumerable, and it would then show up in
//! `console.log(Object.prototype)`, in `JSON.stringify` and in every `for in` loop over any object in
//! the program. An empty `Object.prototype` is wrong in one visible way, and a populated one would
//! be wrong in five. Attributes are the next piece of object model work and the methods arrive with
//! them.
//!
//! What that costs today is that `({}).toString()` and `o.hasOwnProperty('x')` do not work. The
//! conversion itself does, because `String({})` asks whether the chain reaches `Object.prototype`
//! rather than asking for a method that is not there yet.
//!
//! `Object.setPrototypeOf` and `__proto__`, which change an existing object's prototype. That is a
//! different operation from choosing one at creation: it has to move an object to a different shape
//! after it already has one, it needs the cycle check the specification puts on it, and every engine
//! treats an object it has been done to as damaged goods afterwards. It is worth doing carefully
//! rather than quickly, and the property lookup in the interpreter says what it will have to change.
//!
//! `Object.keys`, `Object.values`, `Object.entries` and `Object.getOwnPropertyNames`, all of which
//! answer with an array, and there are no arrays yet.
//!
//! `Object.defineProperty` and the descriptor objects, which are the attributes work.

use katsu_vm::{Interpreter, RuntimeError, Value, arg};

/// Put `Object` in the global scope.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the object, its two functions
/// or `Object.prototype`, which at startup means the heap is far too small rather than that anything
/// went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let prototype = interpreter.object_prototype()?;
    let create = interpreter.native_function("create", create)?;
    let get_prototype_of = interpreter.native_function("getPrototypeOf", get_prototype_of)?;
    let object = interpreter.host_object(&[
        ("prototype", prototype),
        ("create", create),
        ("getPrototypeOf", get_prototype_of),
    ])?;
    interpreter.define_global("Object", object)
}

/// `Object.create(prototype)`.
///
/// The second argument is a map of property descriptors, and descriptors are the attributes work
/// that has not happened, so passing one refuses by name rather than being ignored. Ignoring it
/// would produce an object missing every property the caller asked for, which is a wrong answer
/// dressed as a right one.
///
/// `undefined` is not the same as absent here. `Object.create()` is a `TypeError` in Node, because
/// the argument is required to be an object or `null` and `undefined` is neither, so the missing
/// argument falls into the same message rather than being defaulted.
fn create(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    let descriptors = arg(args, 1);
    if !descriptors.is_undefined() {
        return Err(RuntimeError::Unsupported(
            "Object.create does not support the property descriptors argument yet".to_owned(),
        ));
    }
    interpreter.new_object_with_prototype(arg(args, 0))
}

/// `Object.getPrototypeOf(value)`.
///
/// Three outcomes and they are three different things. An object answers with its prototype or with
/// `null`. `undefined` and `null` throw, in the words Node uses, because there is no object to ask.
/// Every other primitive has an answer in the specification, which is the prototype of the wrapper
/// it would be converted to, and this build has no wrapper prototypes, so it refuses by name instead
/// of saying `null` and being believed.
fn get_prototype_of(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    let value = arg(args, 0);
    if value.is_undefined() || value.is_null() {
        return Err(RuntimeError::Type(
            "Cannot convert undefined or null to object".to_owned(),
        ));
    }
    if let Some(prototype) = interpreter.prototype_of(value) {
        return Ok(prototype);
    }
    let what = if interpreter.is_callable(value) {
        "a function, because it needs Function.prototype"
    } else {
        "a primitive, because it needs the wrapper prototypes"
    };
    Err(RuntimeError::Unsupported(format!(
        "Object.getPrototypeOf is not supported yet for {what}"
    )))
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with `Object`, `String` and `console` in it and hand back what it printed.
    ///
    /// The value globals are in here because `undefined` is a binding on the global object rather
    /// than a keyword, so a program that writes it in an isolate without them gets a
    /// `ReferenceError` instead of the answer. That is the asymmetry `globals.rs` was written for
    /// and these tests walked straight into it.
    #[track_caller]
    fn printed(source: &str) -> Result<String, String> {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        crate::globals::install(&mut interpreter).expect("should install");
        super::install(&mut interpreter).expect("should install");
        crate::string::install(&mut interpreter).expect("should install");
        crate::console::install(&mut interpreter).expect("should install");
        let recorder = Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        let blueprint = katsu_vm::compile("test.js", source).map_err(|error| error.to_string())?;
        interpreter
            .run(&blueprint)
            .map_err(|error| error.to_string())?;
        Ok(recorder.text())
    }

    /// What a program printed, with the trailing newline off, for a program that should not fail.
    #[track_caller]
    fn logged(source: &str) -> String {
        let text = printed(source).expect("the program should run");
        text.strip_suffix('\n').unwrap_or(&text).to_owned()
    }

    #[test]
    fn an_object_literal_inherits_from_object_prototype() {
        assert_eq!(
            logged("console.log(Object.getPrototypeOf({}) === Object.prototype);"),
            "true"
        );
    }

    #[test]
    fn object_prototype_is_the_top_and_inherits_from_nothing() {
        assert_eq!(
            logged("console.log(Object.getPrototypeOf(Object.prototype));"),
            "null"
        );
    }

    #[test]
    fn a_property_that_is_not_on_an_object_is_looked_for_above_it() {
        assert_eq!(
            logged("var p = {x: 1}; var o = Object.create(p); console.log(o.x);"),
            "1"
        );
    }

    #[test]
    fn an_own_property_hides_the_inherited_one_of_the_same_name() {
        assert_eq!(
            logged("var p = {x: 1}; var o = Object.create(p); o.x = 2; console.log(o.x, p.x);"),
            "2 1"
        );
    }

    #[test]
    fn writing_makes_an_own_property_and_leaves_the_prototype_alone() {
        // There are no setters, so a write never goes up the chain. The prototype keeping its own
        // value is the whole observable difference between inheriting a property and sharing one.
        assert_eq!(
            logged(
                "var p = {x: 1}; var a = Object.create(p); var b = Object.create(p); a.x = 9; console.log(a.x, b.x, p.x);"
            ),
            "9 1 1"
        );
    }

    #[test]
    fn a_lookup_walks_the_whole_chain_and_not_one_step_of_it() {
        assert_eq!(
            logged("var o = Object.create(Object.create({deep: 'found'})); console.log(o.deep);"),
            "found"
        );
    }

    #[test]
    fn a_name_that_is_nowhere_on_the_chain_is_undefined_rather_than_an_error() {
        assert_eq!(
            logged("var o = Object.create({x: 1}); console.log(o.nope);"),
            "undefined"
        );
    }

    #[test]
    fn an_object_created_with_null_inherits_from_nothing() {
        assert_eq!(
            logged("var o = Object.create(null); console.log(Object.getPrototypeOf(o), o.x);"),
            "null undefined"
        );
    }

    #[test]
    fn an_object_with_no_prototype_says_so_when_it_is_printed() {
        assert_eq!(
            logged("console.log(Object.create(null));"),
            "[Object: null prototype] {}"
        );
        assert_eq!(
            logged("var o = Object.create(null); o.x = 1; console.log(o);"),
            "[Object: null prototype] { x: 1 }"
        );
    }

    #[test]
    fn an_object_with_no_prototype_has_no_text_and_says_which_error_that_is() {
        // The chain is what converts an object to text, so an object with no chain cannot be
        // converted, and node's message says exactly that.
        let error = printed("String(Object.create(null));").expect_err("should throw");
        assert!(
            error.contains("Cannot convert object to primitive value"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_chain_that_does_not_reach_object_prototype_has_no_text_either() {
        // Having a prototype is not enough. What matters is whether the walk arrives somewhere that
        // would have a `toString` on it, which is `Object.prototype` and nowhere else.
        let error = printed("'' + Object.create(Object.create(null));").expect_err("should throw");
        assert!(
            error.contains("Cannot convert object to primitive value"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn an_ordinary_object_still_converts_to_the_text_it_always_did() {
        assert_eq!(
            logged("console.log(String({}), '' + {a: 1});"),
            "[object Object] [object Object]"
        );
    }

    #[test]
    fn creating_from_something_that_is_not_an_object_names_the_value() {
        let error = printed("Object.create(1);").expect_err("should throw");
        assert!(
            error.contains("Object prototype may only be an Object or null: 1"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn creating_from_nothing_at_all_is_the_same_refusal_as_creating_from_undefined() {
        let missing = printed("Object.create();").expect_err("should throw");
        let explicit = printed("Object.create(undefined);").expect_err("should throw");
        assert_eq!(missing, explicit);
        assert!(
            missing.contains("Object prototype may only be an Object or null: undefined"),
            "unexpected error: {missing}"
        );
    }

    #[test]
    fn the_descriptors_argument_refuses_by_name_rather_than_being_ignored() {
        let error = printed("Object.create(null, {x: {value: 1}});").expect_err("should throw");
        assert!(
            error.contains("Object.create does not support the property descriptors argument yet"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn asking_undefined_or_null_for_a_prototype_throws_the_way_node_does() {
        for source in [
            "Object.getPrototypeOf(undefined);",
            "Object.getPrototypeOf(null);",
        ] {
            let error = printed(source).expect_err("should throw");
            assert!(
                error.contains("Cannot convert undefined or null to object"),
                "unexpected error for {source}: {error}"
            );
        }
    }

    #[test]
    fn asking_a_primitive_for_a_prototype_refuses_by_name_rather_than_answering_null() {
        // The specification's answer is `Number.prototype`, and saying `null` would be a wrong
        // answer that a program would believe. Refusing says which piece of work is missing.
        let error = printed("Object.getPrototypeOf(1);").expect_err("should throw");
        assert!(
            error.contains("it needs the wrapper prototypes"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn asking_a_function_for_a_prototype_names_the_other_missing_piece() {
        let error = printed("Object.getPrototypeOf(function () {});").expect_err("should throw");
        assert!(
            error.contains("it needs Function.prototype"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_refusal_cannot_be_caught_because_it_is_not_a_program_error() {
        // Same rule as the rest of the runtime. A gap is the runtime's fault and a `catch` written
        // for bad input would swallow it and report the wrong thing.
        let error =
            printed("try { Object.getPrototypeOf(1); } catch (e) { console.log('caught'); }")
                .expect_err("should not be catchable");
        assert!(
            error.contains("it needs the wrapper prototypes"),
            "unexpected error: {error}"
        );
    }
}
