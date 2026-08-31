//! `Function`, which is the constructor every function in the realm points back at.
//!
//! This is a small file for a reason. Almost everything a program does with `Function` it does
//! through `Function.prototype`, and `call`, `apply`, `bind` and `toString` all need a native to be
//! able to call a JavaScript function, which nothing in this build can do yet. What is here is the
//! part that has to exist for the rest of the object model to tell the truth.
//!
//! # Why this arrived with `Object.prototype.constructor`
//!
//! `constructor` on `Object.prototype` is inherited by everything in the realm, functions included,
//! so the moment it was defined a plain `function f() {}` started answering `f.constructor ===
//! Object`. Node answers `Function`. The fix is the link this file installs: `Function.prototype`
//! gets its own `constructor`, it is found first because it sits below `Object.prototype` on a
//! function's chain, and `f.constructor` is `Function` again.
//!
//! # What is here
//!
//! `Function`, `Function.prototype` and `Function.prototype.constructor`, and the chain around them.
//! `typeof Function` is "function", `Object.getPrototypeOf(Function)` is `Function.prototype` the way
//! it is for any other function, and `Object.getPrototypeOf(Function.prototype)` is
//! `Object.prototype`.
//!
//! # What is not here
//!
//! Calling `Function`, which compiles its last argument as a function body. That is `eval` under
//! another name and it refuses by name rather than doing something smaller and calling it the same
//! thing.
//!
//! `Function.prototype` being callable. In Node it is a function that takes anything and returns
//! `undefined`, which is why `typeof Function.prototype` is "function" and it prints as `[Function
//! (anonymous)]`. Here it is an ordinary object, because a callable value is a closure or a native
//! and neither can be the top of a prototype chain yet. Nothing a program is likely to write depends
//! on it, and it is written down so that it stays a known gap rather than a surprise.
//!
//! `call`, `apply`, `bind`, `toString`, `length` and `name`.

use katsu_vm::{Attributes, Interpreter, RuntimeError, Value};

/// Put `Function` in the global scope.
///
/// Runs after `Object`, because `Function.prototype` inherits from `Object.prototype` and the
/// `constructor` defined here has to be the one found first.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the function or its prototype,
/// which at startup means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let prototype = interpreter.function_prototype()?;
    let function = interpreter.native_function("Function", call)?;
    // Exactly the shape `Object` has, and for the same reasons. Nothing can rewrite `prototype`,
    // because the top of every function's chain moving would take every function with it, and
    // `constructor` is an ordinary hidden property that a program is allowed to replace.
    interpreter.define_property(function, "prototype", prototype, Attributes::NONE)?;
    interpreter.define_property(prototype, "constructor", function, Attributes::BUILTIN)?;
    interpreter.define_global("Function", function)
}

/// `Function(body)`, which builds a function out of text.
///
/// It refuses by name. The argument is source, so this is a compiler entry point rather than a
/// constructor, and it carries every question `eval` carries about which scope the result closes
/// over. Answering with something that ignores the arguments would be worse than not answering.
fn call(
    _interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    Err(RuntimeError::Unsupported(
        "Function is not supported yet as a call, because building a function out of text is a compiler entry point and needs the same decisions as eval".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with `Object`, `Function` and `console` in it and hand back what it printed.
    #[track_caller]
    fn printed(source: &str) -> Result<String, String> {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        crate::globals::install(&mut interpreter).expect("should install");
        crate::object::install(&mut interpreter).expect("should install");
        super::install(&mut interpreter).expect("should install");
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
    fn every_function_answers_with_function_and_not_with_object() {
        // The reason this module exists. `constructor` is inherited from `Object.prototype` by
        // everything, so without the link installed here a function would name the wrong maker.
        assert_eq!(
            logged(
                "function f() {}\n\
                 console.log(f.constructor === Function, Object.constructor === Function, ({}).constructor === Object);"
            ),
            "true true true"
        );
    }

    #[test]
    fn function_is_a_function_and_sits_on_its_own_prototype() {
        assert_eq!(
            logged(
                "console.log(typeof Function, Object.getPrototypeOf(Function) === Function.prototype, Object.getPrototypeOf(Function.prototype) === Object.prototype);"
            ),
            "function true true"
        );
    }

    #[test]
    fn a_function_written_by_a_program_inherits_the_same_prototype_object() {
        assert_eq!(
            logged(
                "function f() {} console.log(Object.getPrototypeOf(f) === Function.prototype, Function.prototype.constructor === Function);"
            ),
            "true true"
        );
    }

    #[test]
    fn the_link_between_function_and_its_prototype_cannot_be_rewritten() {
        assert_eq!(
            logged(
                "var d = Object.getOwnPropertyDescriptor(Function, 'prototype'); console.log(d.writable, d.enumerable, d.configurable);"
            ),
            "false false false"
        );
    }

    #[test]
    fn constructor_is_hidden_the_way_node_hides_it() {
        assert_eq!(
            logged(
                "var d = Object.getOwnPropertyDescriptor(Function.prototype, 'constructor'); console.log(d.value === Function, d.writable, d.enumerable, d.configurable);"
            ),
            "true true false true"
        );
    }

    #[test]
    fn building_a_function_out_of_text_refuses_by_name() {
        let error = printed("Function('return 1');").expect_err("should throw");
        assert!(
            error.contains("needs the same decisions as eval"),
            "unexpected error: {error}"
        );
    }
}
