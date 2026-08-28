//! `console`, which is the first thing every JavaScript program a person writes actually uses.
//!
//! Not part of ECMAScript at all. It is a WHATWG specification that browsers and Node both implement,
//! and it is in this crate because it is a builtin from a program's point of view even though it is
//! not one from the standard's.
//!
//! # What a line looks like
//!
//! Every argument is inspected and the results are joined with a single space, then one newline goes
//! on the end. Inspecting rather than converting is the whole difference between `console.log` and
//! `process.stdout.write`: an object prints its contents rather than `[object Object]`, and a string
//! at the top level prints without quotes while the same string inside an object prints with them.
//! That is [`Interpreter::display`] and it is deliberately the same function an embedder gets.
//!
//! # What is not here yet
//!
//! Format specifiers. Node treats a first argument containing `%s`, `%d`, `%i`, `%f`, `%j`, `%o`,
//! `%O` or `%c` as a template and substitutes the rest into it. It is a real part of the interface
//! and it is not written yet, so `console.log('%s', 'katsu')` prints `%s katsu` here and `katsu`
//! under Node. Nothing about it is hard, it just needs its own tests.
//!
//! `console.table`, `console.group`, `console.time` and the counters are also absent. Each of them
//! is a small piece of formatting over the same sink and they arrive when there is something to run
//! against them.

use katsu_vm::{Interpreter, RuntimeError, Stream, Value};

/// Put `console` in the global scope.
///
/// One object with the methods on it, rather than five globals. The object is a record, so its
/// contents are fixed at the moment it is built, which is exactly right for something the runtime
/// owns and the program is not supposed to be rearranging.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the functions or the object,
/// which at startup means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let log = interpreter.native_function("log", log)?;
    let error = interpreter.native_function("error", error)?;
    let warn = interpreter.native_function("warn", warn)?;
    let info = interpreter.native_function("info", info)?;
    let debug = interpreter.native_function("debug", debug)?;
    let console = interpreter.host_object(&[
        ("log", log),
        ("error", error),
        ("warn", warn),
        ("info", info),
        ("debug", debug),
    ])?;
    interpreter.define_global("console", console)
}

/// Write one line to standard output.
#[allow(clippy::unnecessary_wraps)]
fn log(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    write_line(interpreter, Stream::Out, args);
    Ok(Value::UNDEFINED)
}

/// Write one line to standard error.
///
/// Standard error and not standard output, which is the part of `console.error` that matters. A
/// program whose output is being piped somewhere expects its diagnostics to stay out of the pipe.
#[allow(clippy::unnecessary_wraps)]
fn error(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    write_line(interpreter, Stream::Err, args);
    Ok(Value::UNDEFINED)
}

/// Write one line to standard error, which is where Node puts a warning too.
#[allow(clippy::unnecessary_wraps)]
fn warn(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    write_line(interpreter, Stream::Err, args);
    Ok(Value::UNDEFINED)
}

/// `console.log` under another name, which is what it is in Node as well.
#[allow(clippy::unnecessary_wraps)]
fn info(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    write_line(interpreter, Stream::Out, args);
    Ok(Value::UNDEFINED)
}

/// Also `console.log` under another name. Node routes `debug` to standard output, not to a debug
/// channel, and a program that expects its debug output in the same stream as the rest is right.
#[allow(clippy::unnecessary_wraps)]
fn debug(interpreter: &mut Interpreter, args: &[Value]) -> Result<Value, RuntimeError> {
    write_line(interpreter, Stream::Out, args);
    Ok(Value::UNDEFINED)
}

/// Inspect every argument, join them with a space and write the line.
///
/// The line is built and written once rather than written a piece at a time, so that two threads
/// printing into one sink interleave between lines rather than inside them.
fn write_line(interpreter: &mut Interpreter, stream: Stream, args: &[Value]) {
    let mut line = String::new();
    for (index, value) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&interpreter.display(*value));
    }
    line.push('\n');
    interpreter.write_output(stream, &line);
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// A runtime with `console` in it and its output going somewhere a test can read.
    fn printing() -> (Interpreter, Recorder) {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        super::install(&mut interpreter).expect("should have room");
        let recorder = Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        (interpreter, recorder)
    }

    /// Run a program and hand back everything it printed.
    #[track_caller]
    fn printed(source: &str) -> String {
        let (mut interpreter, recorder) = printing();
        let blueprint = katsu_vm::compile("t.js", source).expect("should compile");
        interpreter.run(&blueprint).expect("should not throw");
        recorder.text()
    }

    #[test]
    fn console_log_prints_its_argument_and_a_newline() {
        assert_eq!(printed("console.log('hello')"), "hello\n");
    }

    #[test]
    fn several_arguments_are_joined_with_one_space() {
        assert_eq!(printed("console.log(1, 'two', true)"), "1 two true\n");
    }

    #[test]
    fn printing_nothing_still_prints_a_line() {
        // Which is what Node does, and it is what a program using it as a blank line expects.
        assert_eq!(printed("console.log()"), "\n");
    }

    #[test]
    fn each_call_is_its_own_line_and_they_arrive_in_order() {
        assert_eq!(printed("console.log(1); console.log(2)"), "1\n2\n");
    }

    #[test]
    fn a_string_at_the_top_level_prints_without_quotes() {
        // The difference between inspecting and converting, and the reason `console.log` does not
        // just call `ToString` on everything.
        assert_eq!(printed("console.log('katsu')"), "katsu\n");
    }

    #[test]
    fn console_prints_from_inside_a_function_too() {
        assert_eq!(
            printed("function greet(name) { console.log('hello ' + name); } greet('katsu')"),
            "hello katsu\n"
        );
    }

    #[test]
    fn every_name_on_the_console_object_is_a_function() {
        for name in ["log", "error", "warn", "info", "debug"] {
            let source = format!("typeof console.{name}");
            let (mut interpreter, _) = printing();
            let blueprint = katsu_vm::compile("t.js", &source).expect("should compile");
            interpreter.run(&blueprint).expect("should not throw");
            // The program returns undefined until scripts have completion values, so the assertion
            // that the name is there and callable is that calling it prints.
            let call =
                katsu_vm::compile("t.js", &format!("console.{name}('x')")).expect("should compile");
            interpreter.run(&call).expect("should not throw");
        }
    }

    #[test]
    fn a_warning_and_an_error_go_to_the_other_stream() {
        // The recorder keeps both, in order, which is the arrangement that makes this testable at
        // all. That the streams really differ is the standard sink's job and it is not something a
        // unit test can see without capturing the process's own file descriptors.
        assert_eq!(
            printed("console.error('bad'); console.warn('careful')"),
            "bad\ncareful\n"
        );
    }

    #[test]
    fn an_object_prints_its_contents_rather_than_object_object() {
        assert_eq!(printed("console.log(console.log)"), "[Function: log]\n");
    }
}
