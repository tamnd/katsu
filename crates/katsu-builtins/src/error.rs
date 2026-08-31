//! `Error` and the six standard subclasses.
//!
//! Every one of these is a constructor, which is why they arrive together and why they arrive before
//! the rest of `Function.prototype`. A program that catches anything at all names one of them, and
//! 20,360 of the 53,874 JavaScript files in test262 mention an error constructor or `assert.throws`,
//! and until this file existed `typeof Error` answered "undefined", so more than a third of the
//! suite could not get a correct answer out of the harness it runs under.
//!
//! # One body, seven constructors
//!
//! `Error`, `TypeError`, `RangeError`, `ReferenceError`, `SyntaxError`, `EvalError` and `URIError`
//! differ in exactly two things: the name on their prototype, and which prototype a new instance
//! gets. So there are seven one line functions here and one shared body, and the thing that tells
//! them apart is the [`ErrorKind`] each one passes down. That is the same shape the specification
//! has, where the six subclasses are defined as "the same as `Error`, with these two substitutions".
//!
//! # What the engine throws
//!
//! The interpreter builds its own errors from the prototypes registered here, so
//! `null.x` throws something that is `instanceof TypeError` and prints the way Node prints it. Before
//! this file, an engine error was a plain object with `name` and `message` on it, and that is still
//! what a realm gets if it installs `console` without installing this.
//!
//! # What is here
//!
//! The seven constructors, their prototypes and the chain between them,
//! `Error.prototype.toString`, `message`, and `cause` from the options argument.
//!
//! # What is not here
//!
//! `stack`. Node puts it on the instance before `message`, so
//! `Object.getOwnPropertyNames(new Error('x'))` is `['stack', 'message']` there and `['message']`
//! here. A stack needs frames that survive the throw, which is its own piece of work, and a `stack`
//! that answered with a plausible wrong trace would be worse than one that is absent.
//!
//! `Object.getPrototypeOf(TypeError) === Error`. A function written in Rust sits on
//! `Function.prototype` like any other function, and the subclass links between the constructors
//! themselves need the constructors to be objects that can have their prototype set. The link that
//! matters to a program, `Object.getPrototypeOf(TypeError.prototype) === Error.prototype`, is here,
//! so `new TypeError('x') instanceof Error` is true.
//!
//! `captureStackTrace`, `prepareStackTrace` and `stackTraceLimit`, which are V8 extensions rather
//! than language, and `AggregateError`, which needs arrays.

use katsu_vm::{
    Attributes, ErrorKind, Interpreter, NativeFn, RuntimeError, Value, arg, this_value,
};

/// Put the seven error constructors in the global scope.
///
/// Runs after `Object` and `Function`, because every prototype here inherits from
/// `Object.prototype` and every constructor here is a function.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the fourteen objects this
/// makes, which at startup means the heap is far too small rather than that anything went wrong
/// here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let object_prototype = interpreter.object_prototype()?;
    let base = install_kind(interpreter, ErrorKind::Error, error, object_prototype)?;
    // Only on `Error.prototype`. The six subclasses inherit it rather than each carrying a copy,
    // which is both what Node reports for their own property names and the reason `toString` reads
    // `name` off the receiver instead of knowing its own.
    let to_string = interpreter.native_function("toString", to_string)?;
    interpreter.define_property(base, "toString", to_string, Attributes::BUILTIN)?;
    for (kind, call) in [
        (ErrorKind::Type, type_error as NativeFn),
        (ErrorKind::Range, range_error),
        (ErrorKind::Reference, reference_error),
        (ErrorKind::Syntax, syntax_error),
        (ErrorKind::Eval, eval_error),
        (ErrorKind::Uri, uri_error),
    ] {
        install_kind(interpreter, kind, call, base)?;
    }
    Ok(())
}

/// One constructor, its prototype, the links between the two, and the global name.
///
/// Hands back the prototype, because `Error`'s is what the other six inherit from and what
/// `toString` gets defined on.
///
/// The three properties go on in the order Node reports them, `constructor` then `name` then
/// `message`, because a `for in` over a prototype with something enumerable added to it later walks
/// them in insertion order and there is no reason to differ.
fn install_kind(
    interpreter: &mut Interpreter,
    kind: ErrorKind,
    call: NativeFn,
    parent: Value,
) -> Result<Value, RuntimeError> {
    let name = kind.name();
    let prototype = interpreter.new_object_with_prototype(parent)?;
    let constructor = interpreter.native_constructor(name, call)?;
    // Nothing can rewrite `prototype`, exactly as on `Object` and `Function`, because `new` reads it
    // to build the instance and moving it would move every error the engine throws afterwards.
    interpreter.define_property(constructor, "prototype", prototype, Attributes::NONE)?;
    interpreter.define_property(prototype, "constructor", constructor, Attributes::BUILTIN)?;
    let text = interpreter.new_string(name)?;
    interpreter.define_property(prototype, "name", text, Attributes::BUILTIN)?;
    // The empty string and not the absence of a property. `new Error().message` is "" in Node and it
    // is inherited from here, which is what makes `String(new Error())` answer "Error" with no colon.
    let empty = interpreter.new_string("")?;
    interpreter.define_property(prototype, "message", empty, Attributes::BUILTIN)?;
    // What the interpreter builds its own errors from, so this has to happen even for a kind no
    // program ever names.
    interpreter.set_error_prototype(kind, prototype)?;
    interpreter.define_global(name, constructor)?;
    Ok(prototype)
}

/// `Error(message, options)`, and the other six with two substitutions.
///
/// `new Error('x')` and `Error('x')` do the same thing, which is unusual and is the specification's
/// own rule rather than a shortcut here. The difference is only where the object comes from: `new`
/// has already made one out of the constructor's `prototype` and handed it over as the receiver, and
/// a plain call has to make its own out of the intrinsic. Reading the intrinsic rather than the
/// global matters when a program has reassigned `Error`, because `Error('x')` inside the old
/// function still has to make an old style error.
fn build(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
    kind: ErrorKind,
) -> Result<Value, RuntimeError> {
    let object = if interpreter.called_with_new() {
        this_value(receiver, kind.name())?
    } else {
        let Some(prototype) = interpreter.error_prototype(kind) else {
            return Err(RuntimeError::Unsupported(format!(
                "{}() needs the error prototypes, which this realm was built without",
                kind.name()
            )));
        };
        interpreter.new_object_with_prototype(prototype)?
    };
    // Defined only when there is one. `new Error()` has no own `message` at all and reads the empty
    // string off its prototype, and a program can tell the two apart with `hasOwnProperty`.
    let message = arg(args, 0);
    if !message.is_undefined() {
        let text = interpreter.to_text(message)?;
        let text = interpreter.new_string(&text)?;
        interpreter.define_property(object, "message", text, Attributes::BUILTIN)?;
    }
    // `cause` is present when the options bag has one anywhere on its chain, including when its
    // value is `undefined`. That is why this asks whether the property is there rather than whether
    // it is something, and why `new Error('x', {})` has no `cause`.
    let options = arg(args, 1);
    if interpreter.is_ordinary_object(options) || interpreter.is_callable(options) {
        let found = interpreter.lookup(options, "cause")?;
        if let Some(cause) = found {
            interpreter.define_property(object, "cause", cause, Attributes::BUILTIN)?;
        }
    }
    Ok(object)
}

/// `Error.prototype.toString`, which is what `String(err)` and `'' + err` go through.
///
/// It reads `name` and `message` off the receiver rather than knowing either, so a program that sets
/// `err.name = 'Boom'` gets "Boom: message" and a subclass gets its own name from its own prototype.
/// The two empty cases are the specification's: no name is "Error", no message is "", and an empty
/// half is dropped along with the colon rather than printed as "Error: " or ": message".
fn to_string(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = this_value(receiver, "Error.prototype.toString")?;
    if !interpreter.is_ordinary_object(value) && !interpreter.is_callable(value) {
        let what = interpreter.display(value);
        return Err(RuntimeError::Type(format!(
            "Error.prototype.toString called on {what}, which is not an object"
        )));
    }
    let name = interpreter
        .lookup(value, "name")?
        .unwrap_or(Value::UNDEFINED);
    let name = if name.is_undefined() {
        "Error".to_owned()
    } else {
        interpreter.to_text(name)?
    };
    let message = interpreter
        .lookup(value, "message")?
        .unwrap_or(Value::UNDEFINED);
    let message = if message.is_undefined() {
        String::new()
    } else {
        interpreter.to_text(message)?
    };
    let text = if name.is_empty() {
        message
    } else if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    };
    interpreter.new_string(&text)
}

/// `Error`.
fn error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Error)
}

/// `TypeError`, thrown when a value is the wrong sort of thing.
fn type_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Type)
}

/// `RangeError`, thrown when a value is the right sort of thing and out of bounds.
fn range_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Range)
}

/// `ReferenceError`, thrown when a name is not bound.
fn reference_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Reference)
}

/// `SyntaxError`, which a program can make even though only the parser throws one on its own.
fn syntax_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Syntax)
}

/// `EvalError`, which nothing in the language throws any more and which exists because code that
/// catches it still exists.
fn eval_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Eval)
}

/// `URIError`, thrown by the four URI encoding functions.
fn uri_error(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    build(interpreter, receiver, args, ErrorKind::Uri)
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with the object model, the error constructors, `String` and `console` in it and
    /// hand back what it printed.
    #[track_caller]
    fn printed(source: &str) -> Result<String, String> {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        crate::globals::install(&mut interpreter).expect("should install");
        crate::object::install(&mut interpreter).expect("should install");
        crate::function::install(&mut interpreter).expect("should install");
        super::install(&mut interpreter).expect("should install");
        crate::string::install(&mut interpreter).expect("should install");
        crate::json::install(&mut interpreter).expect("should install");
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
    fn all_seven_are_functions_in_the_global_scope() {
        assert_eq!(
            logged(
                "console.log(typeof Error, typeof TypeError, typeof RangeError, typeof ReferenceError, typeof SyntaxError, typeof EvalError, typeof URIError);"
            ),
            "function function function function function function function"
        );
    }

    #[test]
    fn a_subclass_prototype_inherits_from_the_error_prototype() {
        // The link that makes `catch (e) { if (e instanceof Error) }` work for every kind, which is
        // how nearly all real code looks at an error.
        assert_eq!(
            logged(
                "var e = new TypeError('x'); console.log(e instanceof TypeError, e instanceof Error, Object.getPrototypeOf(TypeError.prototype) === Error.prototype);"
            ),
            "true true true"
        );
    }

    #[test]
    fn new_and_a_plain_call_build_the_same_thing() {
        // Unusual, and the specification's own rule rather than a shortcut here.
        assert_eq!(
            logged(
                "var a = new RangeError('x'); var b = RangeError('x'); console.log(b instanceof RangeError, a.message === b.message, Object.getPrototypeOf(a) === Object.getPrototypeOf(b));"
            ),
            "true true true"
        );
    }

    #[test]
    fn name_and_message_come_off_the_prototype_until_there_is_one_of_its_own() {
        assert_eq!(
            logged(
                "var e = new Error(); console.log(e.name, JSON.stringify(e.message), e.hasOwnProperty('message'), new Error('x').hasOwnProperty('message'));"
            ),
            "Error \"\" false true"
        );
    }

    #[test]
    fn the_message_is_converted_the_way_every_other_conversion_is() {
        assert_eq!(
            logged("console.log(new Error(1.5).message, new Error(null).message);"),
            "1.5 null"
        );
    }

    #[test]
    fn a_cause_is_defined_only_when_the_options_bag_has_one() {
        assert_eq!(
            logged(
                "console.log(new Error('x', { cause: 7 }).cause, new Error('x', {}).hasOwnProperty('cause'), new Error('x').hasOwnProperty('cause'), new Error('x', 7).hasOwnProperty('cause'));"
            ),
            "7 false false false"
        );
    }

    #[test]
    fn to_string_drops_the_half_that_is_empty() {
        assert_eq!(
            logged(
                "console.log(String(new Error()), '|', String(new TypeError('m')), '|', Error.prototype.toString());"
            ),
            "Error | TypeError: m | Error"
        );
    }

    #[test]
    fn to_string_reads_the_name_the_program_put_there() {
        assert_eq!(
            logged("var e = new Error('m'); e.name = 'Boom'; console.log(String(e), '' + e);"),
            "Boom: m Boom: m"
        );
    }

    #[test]
    fn a_prototype_carries_what_node_says_it_carries() {
        // Node's own property names are `constructor,name,message,toString` on `Error.prototype` and
        // `constructor,name,message` on every subclass. `Object.getOwnPropertyNames` does not exist
        // yet, so the same fact is asked one name at a time.
        assert_eq!(
            logged(
                "var e = Error.prototype, t = TypeError.prototype;\n\
                 console.log(e.hasOwnProperty('constructor'), e.hasOwnProperty('name'), e.hasOwnProperty('message'), e.hasOwnProperty('toString'));\n\
                 console.log(t.hasOwnProperty('constructor'), t.hasOwnProperty('name'), t.hasOwnProperty('message'), t.hasOwnProperty('toString'));"
            ),
            "true true true true\ntrue true true false"
        );
    }

    #[test]
    fn the_link_between_a_constructor_and_its_prototype_cannot_be_rewritten() {
        assert_eq!(
            logged(
                "var d = Object.getOwnPropertyDescriptor(TypeError, 'prototype'); console.log(d.writable, d.enumerable, d.configurable, TypeError.prototype.constructor === TypeError);"
            ),
            "false false false true"
        );
    }

    #[test]
    fn everything_on_a_prototype_is_hidden_the_way_node_hides_it() {
        assert_eq!(
            logged(
                "var d = Object.getOwnPropertyDescriptor(Error.prototype, 'name'); console.log(d.value, d.writable, d.enumerable, d.configurable);"
            ),
            "Error true false true"
        );
    }

    #[test]
    fn an_error_the_engine_throws_is_an_instance_of_the_right_one() {
        // The reason the interpreter knows about these prototypes at all. Before this file an engine
        // error was a plain object and `e instanceof TypeError` was false.
        assert_eq!(
            logged(
                "try { null.x; } catch (e) { console.log(e instanceof TypeError, e instanceof Error, e.name); }"
            ),
            "true true TypeError"
        );
    }

    #[test]
    fn a_thrown_error_prints_the_way_node_prints_it() {
        assert_eq!(
            logged("try { throw new TypeError('bad'); } catch (e) { console.log(e); }"),
            "[TypeError: bad]"
        );
    }

    #[test]
    fn printing_an_error_says_the_name_once() {
        // Node drops a `name` or a `message` that the first line already says, so an error that was
        // renamed prints as `[Boom: m] { code: 42 }` and not with the name repeated after it.
        assert_eq!(
            logged("var e = new Error('m'); e.code = 42; e.name = 'Boom'; console.log(e);"),
            "[Boom: m] { code: 42 }"
        );
    }

    #[test]
    fn printing_an_error_shows_the_cause_the_constructor_hid() {
        // The one property node prints even though it is not enumerable, with the brackets that say
        // so. An error that says what it was caused by is saying the most useful thing it has.
        assert_eq!(
            logged(
                "console.log(new RangeError('r', { cause: 'why' }));\n\
                 var own = new Error('m'); own.cause = 'set';\n\
                 console.log(own);"
            ),
            "[RangeError: r] { [cause]: 'why' }\n[Error: m] { cause: 'set' }"
        );
    }

    #[test]
    fn converting_an_error_goes_through_the_to_string_it_inherits() {
        // `'' + e` is how a great deal of real code prints the error it caught, and before the
        // conversion learned to call a `toString` written in Rust it answered `[object Object]`.
        assert_eq!(
            logged("console.log('caught: ' + new TypeError('m'), String(new EvalError()));"),
            "caught: TypeError: m EvalError"
        );
    }

    #[test]
    fn a_conversion_that_never_ends_is_the_error_node_gives_for_one() {
        // An error whose `name` is itself converts by converting itself, which recurses in Rust
        // rather than on the interpreter's stack, so the frame limit never sees it.
        assert_eq!(
            logged(
                "var e = new Error('m'); e.name = e;\n\
                 try { String(e); } catch (x) { console.log(x instanceof RangeError, x.message); }"
            ),
            "true Maximum call stack size exceeded"
        );
    }
}
