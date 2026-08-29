//! `String`, called as a function.
//!
//! `String(x)` is the explicit spelling of the conversion that `'' + x` does implicitly, and it is
//! the one that turns up in real code because it says what it means. It is also the one every
//! benchmark harness reaches for when it wants a checksum printed as text rather than as a number.
//!
//! # What is here
//!
//! `String(value)` and nothing else. It goes through the interpreter's own `ToString`, the same one
//! concatenation goes through, rather than reimplementing the rules. That matters more than it
//! looks: the number to text conversion alone is the shortest round tripping decimal with a tie
//! broken towards even, which the differential harness already caught us getting wrong once, and a
//! second copy of it would eventually be a second answer.
//!
//! # What is not here
//!
//! `new String(x)`, the wrapper object, which needs `new` and therefore prototype chains. Nobody
//! writes it on purpose and it is one of the few corners of the language that is agreed to have been
//! a mistake, but it exists and it is not here.
//!
//! `String.fromCharCode`, `String.fromCodePoint`, `String.raw` and the whole of `String.prototype`.
//! The prototype is the big one and it is what the `strings` workload in tamnd/katsu-bench is
//! waiting for, along with a collector. All of it needs prototype chains.
//!
//! `String(symbol)`, which is the one case where `String(x)` and `'' + x` genuinely differ: the
//! conversion throws for a symbol and `String` is specified to describe it instead. There are no
//! symbols yet, so there is nothing to get wrong, and the case is written down here so that whoever
//! adds them knows this function has to learn about it.

use katsu_vm::{Interpreter, RuntimeError, Value, arg};

/// Put `String` in the global scope.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the function, which at startup
/// means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    interpreter.define_native("String", string)
}

/// `String(value)`.
///
/// `String()` with nothing at all is the empty string and not `"undefined"`, which is the one case
/// where the missing argument is not simply `undefined` padded in. The specification says so
/// explicitly and it is the reason [`arg`] is not enough on its own here.
fn string(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let text = if args.is_empty() {
        String::new()
    } else {
        interpreter.to_text(arg(args, 0))?
    };
    interpreter.new_string(&text)
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with `String` and `console` in it and hand back everything it printed.
    #[track_caller]
    fn printed(source: &str) -> String {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        crate::globals::install(&mut interpreter).expect("should install");
        super::install(&mut interpreter).expect("should install");
        crate::console::install(&mut interpreter).expect("should install");
        let recorder = Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        let blueprint = katsu_vm::compile("t.js", source).expect("should compile");
        interpreter.run(&blueprint).expect("should not throw");
        recorder.text()
    }

    #[test]
    fn every_primitive_converts_the_way_node_converts_it() {
        // Every one of these was run under node v26.8.1 and the answer copied, rather than
        // remembered. The two worth staring at are the last two.
        for (source, expected) in [
            ("String(undefined)", "undefined"),
            ("String(null)", "null"),
            ("String(true)", "true"),
            ("String(false)", "false"),
            ("String(0)", "0"),
            ("String(1.5)", "1.5"),
            ("String(NaN)", "NaN"),
            ("String(Infinity)", "Infinity"),
            ("String(-Infinity)", "-Infinity"),
            ("String('hi')", "hi"),
        ] {
            assert_eq!(
                printed(&format!("console.log({source})")),
                format!("{expected}\n")
            );
        }
    }

    #[test]
    fn negative_zero_converts_to_zero_even_though_the_console_prints_it_as_minus_zero() {
        // The one place `String(x)` and `console.log(x)` are specified to differ, and the reason
        // `to_text` and `display` are two functions rather than one. `ToString` of negative zero is
        // "0" because the standard says so, and the console prints "-0" because a console that
        // cannot tell you which zero you have is hiding the thing you turned it on to see.
        assert_eq!(printed("console.log(String(-0))"), "0\n");
        assert_eq!(printed("console.log(-0)"), "-0\n");
    }

    #[test]
    fn an_object_converts_to_object_object_and_not_to_what_the_console_shows() {
        // The other place the two differ, and the one that surprises people. `String({a: 1})` really
        // is `[object Object]`, and a program that prints a converted object and expects to see its
        // contents is a program with a bug that we should reproduce rather than fix.
        assert_eq!(printed("console.log(String({}))"), "[object Object]\n");
        assert_eq!(printed("console.log(String({a: 1}))"), "[object Object]\n");
        assert_eq!(printed("console.log({a: 1})"), "{ a: 1 }\n");
    }

    #[test]
    fn calling_it_with_nothing_is_the_empty_string_and_not_the_word_undefined() {
        // The case that is not simply the missing argument padded with `undefined`. `String()` is
        // "" and `String(undefined)` is "undefined", and getting that wrong is invisible until
        // somebody builds a string out of an optional value.
        assert_eq!(printed("console.log('[' + String() + ']')"), "[]\n");
        assert_eq!(
            printed("console.log('[' + String(undefined) + ']')"),
            "[undefined]\n"
        );
    }

    #[test]
    fn it_agrees_with_concatenation_on_every_value_there_is() {
        // The property that matters more than any individual answer, because these are one
        // conversion in the language and two code paths would eventually be two answers. `-0` is in
        // the list on purpose: it is the value most likely to make them diverge.
        assert_eq!(
            printed(
                "let ok = true;
                 function same(v) { if (String(v) !== '' + v) { ok = false; } }
                 same(undefined); same(null); same(true); same(false);
                 same(0); same(-0); same(1.5); same(NaN); same(Infinity); same(-Infinity);
                 same('hi'); same({}); same({a: 1});
                 console.log(ok);"
            ),
            "true\n"
        );
    }

    #[test]
    fn it_is_a_function_and_extra_arguments_are_ignored() {
        assert_eq!(printed("console.log(typeof String)"), "function\n");
        assert_eq!(printed("console.log(String(1, 2, 3))"), "1\n");
    }

    #[test]
    fn a_string_passed_through_comes_back_equal_to_what_went_in() {
        // Equal rather than identical, because whether the same reference comes back out is an
        // implementation detail a program cannot see, and a test that asserted on it would be a test
        // of the allocator rather than of the conversion.
        assert_eq!(
            printed("console.log(String('katsu') === 'katsu')"),
            "true\n"
        );
        assert_eq!(printed("console.log(String('') === '')"), "true\n");
    }
}
