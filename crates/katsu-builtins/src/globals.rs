//! The three value properties of the global object.
//!
//! `undefined`, `NaN` and `Infinity` are not keywords. They are ordinary bindings on the global
//! object, which is why `typeof undefined` answers "undefined" in an engine that has never heard of
//! them and `let x = undefined` throws a `ReferenceError` in the same engine. That asymmetry is
//! exactly how this gap survived: `typeof` of an unresolvable reference is defined to be the string
//! "undefined" rather than an error, so every test anybody would think to write passes and the
//! commonest expression in JavaScript does not work.
//!
//! Found by the differential harness on its first run against node, in a generated program that had
//! nothing to do with globals and simply happened to contain the word.
//!
//! # What is not here
//!
//! `globalThis`, which needs the global scope to be reachable as an object rather than as a table
//! the interpreter owns. That is a real piece of design and not an oversight, and it belongs with
//! the object model in M1 rather than with a one line binding here.

use katsu_vm::{Interpreter, RuntimeError, Value};

/// Bind the value properties of the global object.
///
/// All three are writable in sloppy mode in no engine anybody ships, and the specification marks
/// them writable false, enumerable false, configurable false. There are no property attributes yet,
/// so they go in as plain bindings and the attributes arrive with the object model.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap cannot hold three names, which at startup
/// means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    interpreter.define_global("undefined", Value::UNDEFINED)?;
    interpreter.define_global("NaN", Value::from_f64(f64::NAN))?;
    interpreter.define_global("Infinity", Value::from_f64(f64::INFINITY))
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with the globals and `console` in it and hand back everything it printed.
    ///
    /// Through what it printed rather than through the value the program returned, because a script
    /// has no completion value in this build yet, so every program answers `undefined` and every
    /// assertion on the return value would pass without testing anything.
    #[track_caller]
    fn printed(source: &str) -> Result<String, String> {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
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

    #[test]
    fn undefined_is_a_name_a_program_can_read() {
        // The one the harness found. `let x = undefined;` is in more JavaScript than almost any
        // other expression, and it threw a ReferenceError.
        assert_eq!(
            printed("console.log(undefined)"),
            Ok("undefined\n".to_owned())
        );
        assert_eq!(
            printed("let x = undefined; console.log(x)"),
            Ok("undefined\n".to_owned())
        );
        assert_eq!(
            printed("console.log(undefined === undefined)"),
            Ok("true\n".to_owned())
        );
    }

    #[test]
    fn nan_is_a_name_and_is_not_equal_to_itself() {
        assert_eq!(printed("console.log(NaN)"), Ok("NaN\n".to_owned()));
        assert_eq!(
            printed("console.log(NaN === NaN)"),
            Ok("false\n".to_owned())
        );
        assert_eq!(printed("console.log(NaN !== NaN)"), Ok("true\n".to_owned()));
    }

    #[test]
    fn infinity_is_a_name_and_has_a_negative() {
        assert_eq!(
            printed("console.log(Infinity)"),
            Ok("Infinity\n".to_owned())
        );
        assert_eq!(
            printed("console.log(-Infinity)"),
            Ok("-Infinity\n".to_owned())
        );
        assert_eq!(
            printed("console.log(1 / 0 === Infinity)"),
            Ok("true\n".to_owned())
        );
    }

    #[test]
    fn typeof_still_answers_for_a_name_that_is_genuinely_not_there() {
        // The behaviour that hid the bug for as long as it did. Worth a test of its own, because it
        // is what makes `typeof x === 'undefined'` the safe way to ask about a global that may not
        // exist, and because it is what stopped every obvious test from noticing.
        assert_eq!(
            printed("console.log(typeof nowhere)"),
            Ok("undefined\n".to_owned())
        );
        assert!(printed("console.log(nowhere)").is_err());
    }
}
