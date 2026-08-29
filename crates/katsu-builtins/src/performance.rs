//! `performance`, which is how a program times itself.
//!
//! Not part of ECMAScript. It is the W3C High Resolution Time specification, which browsers, Node,
//! Deno and Bun all implement, and it is here for the same reason `console` is: from a program's
//! point of view it is simply there, and a program that cannot find it does not fall back to
//! anything, it throws.
//!
//! # Why this one and not `Date.now`
//!
//! `Date.now()` is whole milliseconds off the wall clock. Timing a piece of work with it gives an
//! integer number of milliseconds that can be negative if the system clock is corrected while the
//! work is running, and a workload that finishes in under a millisecond measures as zero. Every
//! benchmark worth reading uses `performance.now()` instead, including all six compute workloads in
//! tamnd/katsu-bench, which is what made this the piece of the standard library to build next: it is
//! the first thing between katsu and the first compute number it will ever publish about itself.
//!
//! # What is here
//!
//! `now()` and `timeOrigin`. Between them they are the whole of what a program timing itself needs
//! and they are the two that every runtime agrees on.
//!
//! # What is not here
//!
//! `mark`, `measure`, `getEntries`, `clearMarks` and the rest of the User Timing interface, which is
//! a named list of timestamps and the queries over it. Nothing about it is hard and none of it is
//! useful without somewhere to send the entries, which in Node is the observer interface and here
//! would be an event loop that does not exist yet.
//!
//! `performance.timerify`, `performance.eventLoopUtilization` and `performance.nodeTiming`, which
//! are Node's own additions and all three of them describe an event loop.
//!
//! `Performance` as a constructor, and `performance` being an instance of it. Node prints
//! `[object Performance]` for it and answers `Performance` for `performance.constructor.name`, and
//! both of those need prototype chains. Until then this is a plain object holding a function, and
//! the observable difference is confined to reflection over it rather than to using it.
//!
//! The specification marks `now` writable, enumerable and configurable, and `timeOrigin` read only
//! and non configurable, and both go in that way.

use katsu_vm::{Attributes, Interpreter, RuntimeError, Value};

/// Put `performance` in the global scope.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the function or the object,
/// which at startup means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let now = interpreter.native_function("now", now)?;
    // Read once here rather than through a getter on every access, which is not a shortcut around a
    // missing feature: `timeOrigin` is a constant for the life of the process by definition, so a
    // getter would be a function that returns the same number forever.
    let origin = Value::from_f64(katsu_vm::origin_ms());
    let performance = interpreter.host_object(&[("now", now)])?;
    // Read only and not configurable, which is what the specification says and what a number that
    // cannot change for the life of the process should be anyway.
    interpreter.define_property(
        performance,
        "timeOrigin",
        origin,
        Attributes::new(false, true, false),
    )?;
    interpreter.define_global("performance", performance)
}

/// Milliseconds since this process's runtime began, fractional and monotonic.
///
/// Takes no arguments and ignores any it is given, which is what every other implementation does and
/// what the specification says: extra arguments to a builtin are dropped rather than being an error.
#[allow(clippy::unnecessary_wraps)]
fn now(
    _interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    Ok(Value::from_f64(katsu_vm::now_ms()))
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with `performance` and `console` in it and hand back everything it printed.
    ///
    /// Through what it printed because a script has no completion value in this build yet, so every
    /// program answers `undefined` and asserting on the return value would assert on nothing.
    #[track_caller]
    fn printed(source: &str) -> String {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        super::install(&mut interpreter).expect("should install");
        crate::console::install(&mut interpreter).expect("should install");
        let recorder = Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        let blueprint = katsu_vm::compile("t.js", source).expect("should compile");
        interpreter.run(&blueprint).expect("should not throw");
        recorder.text()
    }

    /// The one number a program printed, as an `f64`.
    #[track_caller]
    fn number(source: &str) -> f64 {
        let text = printed(source);
        text.trim()
            .parse()
            .unwrap_or_else(|_| panic!("expected one number, got {text:?}"))
    }

    #[test]
    fn performance_now_is_a_function_that_answers_a_number() {
        assert_eq!(printed("console.log(typeof performance)"), "object\n");
        assert_eq!(printed("console.log(typeof performance.now)"), "function\n");
        assert_eq!(printed("console.log(typeof performance.now())"), "number\n");
    }

    #[test]
    fn time_moves_forward_across_work_the_program_actually_did() {
        // A busy loop rather than a pair of adjacent calls, because two calls with nothing between
        // them can legitimately return the same number on a coarse clock and the assertion would be
        // flaky in exactly the way a timing test must not be.
        assert_eq!(
            printed(
                "const a = performance.now();
                 let sum = 0;
                 for (let i = 0; i < 200000; i++) { sum += i; }
                 const b = performance.now();
                 console.log(b > a);"
            ),
            "true\n"
        );
    }

    #[test]
    fn two_readings_with_nothing_between_them_never_go_backwards() {
        // The weaker property, which has to hold for every pair including the adjacent ones. This is
        // the one a benchmark relies on, because a negative elapsed time is the failure everybody
        // who has ever timed anything with a wall clock has seen.
        assert_eq!(
            printed(
                "let ok = true;
                 let last = performance.now();
                 for (let i = 0; i < 500; i++) {
                   const next = performance.now();
                   if (next < last) { ok = false; }
                   last = next;
                 }
                 console.log(ok);"
            ),
            "true\n"
        );
    }

    #[test]
    fn the_unit_is_milliseconds_and_not_seconds_or_nanoseconds() {
        // Reachable from inside the language because the loop below takes a measurable but not
        // enormous amount of time. Seconds would put this under 1 and nanoseconds would put it over
        // a million, and the band between catches both without depending on how fast the machine is.
        let elapsed = number(
            "const a = performance.now();
             let sum = 0;
             for (let i = 0; i < 3000000; i++) { sum += i; }
             console.log(performance.now() - a);",
        );
        assert!(
            (0.01..10000.0).contains(&elapsed),
            "three million additions measured as {elapsed}, which is the wrong unit"
        );
    }

    #[test]
    fn time_origin_is_a_date_and_the_two_add_up_to_now() {
        // The relationship between the two, which is the reason both exist. It is also the assertion
        // that catches an origin taken from the monotonic clock, whose zero is the boot time on
        // Linux and something undocumented on macOS, rather than from the wall clock.
        let origin = number("console.log(performance.timeOrigin)");
        let now = number("console.log(performance.now())");
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock should be after 1970")
            .as_secs_f64()
            * 1000.0;
        assert!(
            (origin + now - wall).abs() < 1000.0,
            "timeOrigin {origin} plus now {now} is not the current date {wall}"
        );
    }

    #[test]
    fn time_origin_is_the_same_number_every_time_it_is_read() {
        assert_eq!(
            printed(
                "const a = performance.timeOrigin;
                 let sum = 0;
                 for (let i = 0; i < 200000; i++) { sum += i; }
                 console.log(a === performance.timeOrigin);"
            ),
            "true\n"
        );
    }

    #[test]
    fn extra_arguments_are_ignored_rather_than_being_an_error() {
        // What every implementation does with a builtin called with more than it takes. Worth a test
        // because the native calling convention hands over exactly what the call site passed, so a
        // native that indexed instead of ignoring would panic here.
        assert_eq!(
            printed("console.log(typeof performance.now(1, 'two', true))"),
            "number\n"
        );
    }

    #[test]
    fn the_shape_of_a_timing_harness_runs_end_to_end() {
        // The pattern every one of the six compute workloads in tamnd/katsu-bench is built around,
        // which is the reason this builtin was written. Not a number assertion, because the answer
        // depends on the machine. The assertion is that the whole shape runs at all.
        assert_eq!(
            printed(
                "function work() { let n = 0; for (let i = 0; i < 100000; i++) { n += i; } return n; }
                 const started = performance.now();
                 const checksum = work();
                 const elapsed = performance.now() - started;
                 console.log(checksum === 4999950000 && elapsed >= 0);"
            ),
            "true\n"
        );
    }
}
