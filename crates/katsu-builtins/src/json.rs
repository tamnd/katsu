//! `JSON`, which today means `JSON.stringify`.
//!
//! Two functions hang off this object in the language and only one of them is here. `stringify` is
//! written because it is what a program uses to print a result it computed, which is the last thing
//! standing between the `fib` workload in tamnd/katsu-bench and the first compute number this project
//! ever publishes about itself. `parse` is not, and it says so out loud rather than being absent, for
//! the reason in [`parse`].
//!
//! # The shape of the output
//!
//! Every rule below was run under node v26.8.1 and the answer copied down, rather than remembered.
//! The ones that surprise people are the first four.
//!
//! `JSON.stringify(undefined)` is the value `undefined` and not the text `"undefined"`, because
//! `undefined` has no JSON spelling. The same is true of a function. Inside an object those two are
//! not written as anything either, the property is dropped, so `{a: 1, b: undefined}` serializes to
//! `{"a":1}` and a reader cannot tell the difference between a property that was undefined and one
//! that was never there.
//!
//! `NaN` and both infinities serialize as `null`, because JSON has no spelling for them and the
//! committee chose a wrong answer over an exception. Negative zero serializes as `0`, so a round trip
//! through JSON turns it into positive zero and nothing warns you.
//!
//! Text is emitted as UTF-8 rather than escaped. `é` and `😀` come out as themselves and only the
//! quote, the backslash and the C0 control characters are escaped. That is what every implementation
//! does and it is what makes the output smaller than people expect.
//!
//! A circular object is a `TypeError` with node's exact sentence, "Converting circular structure to
//! JSON". It is a real exception rather than one of our refusals, because a program that builds a
//! cycle and hands it to `JSON.stringify` has a bug that node reports the same way.
//!
//! # What is not here
//!
//! Arrays, which do not exist in this build yet. When they do, this file gains a branch and the
//! `strings` workload gains a checksum.
//!
//! `toJSON`, which is the hook `Date` uses to serialize itself and which needs prototype chains to
//! find. There is nothing in this build with a `toJSON` on it, so nothing is being ignored yet, but
//! whoever adds `Date` has to come here.
//!
//! The replacer function, the second argument. It is refused rather than ignored, because ignoring it
//! turns a program that asked for a filtered object into a program that gets an unfiltered one and no
//! indication anything happened. An array replacer cannot arrive yet for the same reason arrays
//! cannot, and a replacer that is neither is ignored by node too, so that case needs nothing.
//!
//! Escaping a lone surrogate as `\udXXX`, which the well formed stringify rules ask for. A string
//! reaches Rust through a lossy conversion that has already replaced it with U+FFFD, so by the time
//! this code sees one it is gone. Fixing it means serializing from the UTF-16 code units directly
//! rather than from Rust text, and that is worth doing when there is a rope to serialize out of.

use std::fmt::Write as _;

use katsu_vm::{Attributes, Interpreter, RuntimeError, Value, arg};

/// The largest indent node will use, whether it was asked for in spaces or in text.
///
/// Not a limit we chose. `JSON.stringify({a: 1}, null, 20)` indents by ten under node, and a string
/// longer than ten characters is cut to its first ten.
const MAX_INDENT: u8 = 10;

/// Put `JSON` in the global scope.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the functions or the object,
/// which at startup means the heap is far too small rather than that anything went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let stringify = interpreter.native_function("stringify", stringify)?;
    let parse = interpreter.native_function("parse", parse)?;
    // Non enumerable, like every namespace object in the language. `console.log(JSON)` prints an
    // empty object in Node rather than listing the two functions, and `Object.keys(JSON)` is empty.
    let json = interpreter.host_object_with(
        &[("stringify", stringify), ("parse", parse)],
        Attributes::BUILTIN,
    )?;
    interpreter.define_global("JSON", json)
}

/// `JSON.stringify(value, replacer, space)`.
fn stringify(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let replacer = arg(args, 1);
    if interpreter.is_callable(replacer) {
        return Err(RuntimeError::Unsupported(
            "JSON.stringify does not support a replacer function yet".to_owned(),
        ));
    }

    let indent = indent_from(interpreter, arg(args, 2));
    let mut out = String::new();
    let mut open = Vec::new();
    if write(interpreter, arg(args, 0), &indent, 0, &mut open, &mut out)? {
        interpreter.new_string(&out)
    } else {
        // The value had no JSON spelling at all, which at the top level answers `undefined` rather
        // than the text "undefined" and rather than throwing.
        Ok(Value::UNDEFINED)
    }
}

/// `JSON.parse(text)`, which refuses by name.
///
/// Present and refusing rather than absent, which is a deliberate choice and not a placeholder. A
/// missing method arrives as `JSON.parse is not a function`, an ordinary JavaScript error that a
/// program feature detects around and a reader takes for a bug in their own code. Refusing says whose
/// gap it is.
///
/// The refusal is not catchable, for a sharper reason. Wrapping `JSON.parse` in a `try` is the normal
/// way to write it, because malformed input is the expected case. If our gap were catchable, every
/// one of those programs would take "katsu has not written this yet" for "the input was not JSON",
/// and would go down its error path with an answer that looks reasonable and is wrong.
#[allow(clippy::unnecessary_wraps)]
fn parse(
    _interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    Err(RuntimeError::Unsupported(
        "JSON.parse is not supported yet, because it needs arrays".to_owned(),
    ))
}

/// The text one level of nesting is indented by, from the third argument.
///
/// A number is floored and clamped, a string is cut to its first ten, and anything else at all is
/// ignored rather than being an error. An empty answer means the whole output is written on one line,
/// which is both what `space` of zero asks for and what leaving it out asks for.
fn indent_from(interpreter: &Interpreter, space: Value) -> String {
    if let Some(number) = space.as_f64() {
        // Counted up to rather than rounded down and cast, because turning a float into an integer
        // has three separate ways to go wrong and this only ever has to answer a number between zero
        // and ten. NaN fails every comparison there is, so it leaves the count at zero, which is the
        // answer node gives for it too.
        let mut count = 0u8;
        while count < MAX_INDENT && f64::from(count + 1) <= number {
            count += 1;
        }
        return " ".repeat(usize::from(count));
    }
    if let Some(text) = interpreter.as_text(space) {
        // By characters rather than by UTF-16 code units, which differ only for a string whose first
        // ten characters include one outside the basic plane. Indenting with emoji is not a case
        // worth carrying a second length rule for.
        return text.chars().take(usize::from(MAX_INDENT)).collect();
    }
    String::new()
}

/// Write one value, and say whether it had a JSON spelling at all.
///
/// `false` means it did not, which is what `undefined` and a function are. The caller decides what to
/// do with that, and the two callers decide differently: at the top level it becomes the value
/// `undefined`, and inside an object the property is dropped. Nothing is written to `out` when this
/// answers `false`.
///
/// `open` is the objects currently being written, innermost last, which is how a cycle is caught. It
/// is a list and not a set because it is only ever a few deep and a scan of a few machine words beats
/// hashing every object on the way in.
fn write(
    interpreter: &Interpreter,
    value: Value,
    indent: &str,
    depth: usize,
    open: &mut Vec<u64>,
    out: &mut String,
) -> Result<bool, RuntimeError> {
    if value.is_undefined() || interpreter.is_callable(value) {
        return Ok(false);
    }
    if value.is_null() {
        out.push_str("null");
        return Ok(true);
    }
    if let Some(boolean) = value.as_bool() {
        out.push_str(if boolean { "true" } else { "false" });
        return Ok(true);
    }
    if let Some(number) = value.as_f64() {
        // The one place JSON is narrower than the language. There is no spelling for NaN or for an
        // infinity, so both become null and the program is not told.
        if number.is_finite() {
            out.push_str(&interpreter.to_text(value)?);
        } else {
            out.push_str("null");
        }
        return Ok(true);
    }
    if let Some(text) = interpreter.as_text(value) {
        quote(&text, out);
        return Ok(true);
    }
    if let Some(properties) = interpreter.own_properties(value) {
        return object(interpreter, value, properties, indent, depth, open, out).map(|()| true);
    }
    // Nothing else exists in this build. When arrays do, they branch above this line.
    Ok(false)
}

/// Write an object's braces and the properties between them.
fn object(
    interpreter: &Interpreter,
    value: Value,
    properties: Vec<(String, Value)>,
    indent: &str,
    depth: usize,
    open: &mut Vec<u64>,
    out: &mut String,
) -> Result<(), RuntimeError> {
    let identity = value.to_bits();
    if open.contains(&identity) {
        // Node's exact sentence, because a program that catches this and matches on the message is a
        // program that should not be able to tell which engine it is running under.
        return Err(RuntimeError::Type(
            "Converting circular structure to JSON".to_owned(),
        ));
    }
    open.push(identity);

    let inner = depth + 1;
    let mut written = 0usize;
    out.push('{');
    for (name, property) in properties {
        // Written to a scratch buffer first because a property whose value has no JSON spelling
        // leaves nothing behind at all, not even its name, and by the time that is known the name and
        // the separator would already be in the output.
        let mut piece = String::new();
        if !write(interpreter, property, indent, inner, open, &mut piece)? {
            continue;
        }
        if written > 0 {
            out.push(',');
        }
        newline_and_indent(out, indent, inner);
        quote(&name, out);
        out.push(':');
        if !indent.is_empty() {
            out.push(' ');
        }
        out.push_str(&piece);
        written += 1;
    }
    if written > 0 {
        newline_and_indent(out, indent, depth);
    }
    // An object with nothing in it is `{}` on one line even when an indent was asked for, and an
    // object whose every property was undefined is the same `{}`, which is why this counts what was
    // written rather than what was offered.
    out.push('}');

    open.pop();
    Ok(())
}

/// Break the line and indent to `depth`, or do nothing at all if there is no indent.
fn newline_and_indent(out: &mut String, indent: &str, depth: usize) {
    if indent.is_empty() {
        return;
    }
    out.push('\n');
    for _ in 0..depth {
        out.push_str(indent);
    }
}

/// Write text as a JSON string literal, quotes included.
///
/// Only the quote, the backslash and the C0 controls are escaped. Everything else goes out as itself,
/// so the output is UTF-8 and an accented letter costs two bytes rather than the six an escape would.
fn quote(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // The five controls with a short spelling. Every other one below U+0020 has to be written
            // the long way, and there is no short spelling for anything above it.
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character < '\u{20}' => {
                // Writing into the buffer rather than building a four character string and copying
                // it in. Control characters are rare enough that it makes no measurable difference
                // and it is the shape the rest of the loop already has.
                let _ = write!(out, "\\u{:04x}", u32::from(character));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use katsu_vm::{Interpreter, Recorder};

    /// Run a program with `JSON`, `String` and `console` in it and hand back what it printed.
    #[track_caller]
    fn printed(source: &str) -> Result<String, String> {
        let mut interpreter = Interpreter::new().expect("should reserve a stack");
        crate::globals::install(&mut interpreter).expect("should install");
        crate::string::install(&mut interpreter).expect("should install");
        crate::performance::install(&mut interpreter).expect("should install");
        super::install(&mut interpreter).expect("should install");
        crate::console::install(&mut interpreter).expect("should install");
        let recorder = Recorder::new();
        interpreter.set_output(Box::new(recorder.clone()));
        let blueprint = katsu_vm::compile("t.js", source).map_err(|error| error.to_string())?;
        interpreter
            .run(&blueprint)
            .map_err(|error| error.to_string())?;
        Ok(recorder.text())
    }

    /// What one expression printed, with the trailing newline off.
    #[track_caller]
    fn logged(expression: &str) -> String {
        let text = printed(&format!("console.log({expression})")).expect("should not throw");
        text.trim_end_matches('\n').to_owned()
    }

    #[test]
    fn every_primitive_serializes_the_way_node_serializes_it() {
        // `String(...)` around the call so that the answer `undefined` prints as a word rather than
        // going through the console's own idea of how to show it.
        for (source, expected) in [
            ("String(JSON.stringify(undefined))", "undefined"),
            ("JSON.stringify(null)", "null"),
            ("JSON.stringify(true)", "true"),
            ("JSON.stringify(false)", "false"),
            ("JSON.stringify(0)", "0"),
            ("JSON.stringify(1.5)", "1.5"),
            ("JSON.stringify('hi')", "\"hi\""),
            ("JSON.stringify(1e21)", "1e+21"),
            ("JSON.stringify(1e-7)", "1e-7"),
        ] {
            assert_eq!(logged(source), expected, "{source}");
        }
    }

    #[test]
    fn the_numbers_json_has_no_spelling_for_become_null() {
        // The lossy corner of the format, and the reason a round trip is not a round trip. All three
        // of these were checked under node rather than remembered.
        assert_eq!(logged("JSON.stringify(NaN)"), "null");
        assert_eq!(logged("JSON.stringify(Infinity)"), "null");
        assert_eq!(logged("JSON.stringify(-Infinity)"), "null");
        assert_eq!(
            logged("JSON.stringify({n: NaN, i: Infinity})"),
            "{\"n\":null,\"i\":null}"
        );
    }

    #[test]
    fn negative_zero_serializes_as_zero_and_the_sign_is_gone() {
        // Worth its own test because it is the one value that survives `console.log` and does not
        // survive JSON, so a program that prints it and a program that serializes it disagree.
        assert_eq!(logged("JSON.stringify(-0)"), "0");
        assert_eq!(logged("JSON.stringify({z: -0})"), "{\"z\":0}");
    }

    #[test]
    fn a_value_with_no_json_spelling_disappears_rather_than_becoming_null() {
        // Dropped inside an object and `undefined` at the top level, which is two different answers
        // for the same absence and both of them are node's.
        assert_eq!(
            logged("JSON.stringify({a: 1, b: undefined, c: 2})"),
            "{\"a\":1,\"c\":2}"
        );
        assert_eq!(
            logged("JSON.stringify({f: function () {}, u: undefined})"),
            "{}"
        );
        assert_eq!(
            logged("String(JSON.stringify(function () {}))"),
            "undefined"
        );
    }

    #[test]
    fn objects_nest_and_keep_the_order_the_properties_were_added_in() {
        assert_eq!(logged("JSON.stringify({})"), "{}");
        assert_eq!(
            logged("JSON.stringify({a: 1, b: 'x', c: true, d: null, f: {g: 2}})"),
            "{\"a\":1,\"b\":\"x\",\"c\":true,\"d\":null,\"f\":{\"g\":2}}"
        );
        assert_eq!(
            logged("JSON.stringify({z: 1, a: 2, m: 3})"),
            "{\"z\":1,\"a\":2,\"m\":3}"
        );
    }

    #[test]
    fn a_key_is_quoted_and_escaped_the_same_way_a_value_is() {
        assert_eq!(logged("JSON.stringify({'k e y': 1})"), "{\"k e y\":1}");
        assert_eq!(logged("JSON.stringify({'\"q\"': 2})"), "{\"\\\"q\\\"\":2}");
    }

    #[test]
    fn only_the_quote_the_backslash_and_the_controls_are_escaped() {
        // The rule that keeps the output small. An accented letter is two bytes of UTF-8 rather than
        // six characters of escape, and node does not escape it either.
        assert_eq!(logged("JSON.stringify('a\"b')"), "\"a\\\"b\"");
        assert_eq!(logged("JSON.stringify('a\\\\b')"), "\"a\\\\b\"");
        assert_eq!(logged("JSON.stringify('a\\nb')"), "\"a\\nb\"");
        assert_eq!(logged("JSON.stringify('a\\tb')"), "\"a\\tb\"");
        assert_eq!(logged("JSON.stringify('a\\u0001b')"), "\"a\\u0001b\"");
        assert_eq!(logged("JSON.stringify('café 😀')"), "\"café 😀\"");
    }

    #[test]
    fn a_number_for_space_indents_by_that_many_and_never_by_more_than_ten() {
        assert_eq!(
            logged("JSON.stringify({a: 1, b: {c: 2}}, null, 2)"),
            "{\n  \"a\": 1,\n  \"b\": {\n    \"c\": 2\n  }\n}"
        );
        // Clamped at ten, floored to a whole number, and anything at or below zero puts it all back
        // on one line. Every one of these four was read off node rather than reasoned about.
        assert_eq!(
            logged("JSON.stringify({a: 1}, null, 20)"),
            "{\n          \"a\": 1\n}"
        );
        assert_eq!(
            logged("JSON.stringify({a: 1}, null, 2.9)"),
            "{\n  \"a\": 1\n}"
        );
        assert_eq!(logged("JSON.stringify({a: 1}, null, 0)"), "{\"a\":1}");
        assert_eq!(logged("JSON.stringify({a: 1}, null, -1)"), "{\"a\":1}");
    }

    #[test]
    fn a_string_for_space_indents_with_that_text() {
        assert_eq!(
            logged("JSON.stringify({a: 1}, null, 'ab')"),
            "{\nab\"a\": 1\n}"
        );
        assert_eq!(logged("JSON.stringify({a: 1}, null, '')"), "{\"a\":1}");
        assert_eq!(
            logged("JSON.stringify({a: 1}, null, '0123456789abc')"),
            "{\n0123456789\"a\": 1\n}"
        );
    }

    #[test]
    fn anything_else_for_space_is_ignored_rather_than_being_an_error() {
        assert_eq!(logged("JSON.stringify({a: 1}, null, true)"), "{\"a\":1}");
        assert_eq!(logged("JSON.stringify({a: 1}, null, {})"), "{\"a\":1}");
        assert_eq!(
            logged("JSON.stringify({a: 1}, null, undefined)"),
            "{\"a\":1}"
        );
    }

    #[test]
    fn an_empty_object_stays_on_one_line_even_when_an_indent_was_asked_for() {
        assert_eq!(logged("JSON.stringify({}, null, 2)"), "{}");
        assert_eq!(
            logged("JSON.stringify({a: {}}, null, 2)"),
            "{\n  \"a\": {}\n}"
        );
    }

    #[test]
    fn a_cycle_is_a_type_error_in_the_same_words_node_uses() {
        // A real exception rather than one of our refusals, so a program can catch it, which is what
        // the second half of this test checks. The message is node's exactly, because a program that
        // matches on it should not be able to tell which engine it is running under.
        let error = printed("const a = {}; a.self = a; JSON.stringify(a);")
            .expect_err("a cycle should throw");
        assert!(
            error.contains("Converting circular structure to JSON"),
            "wrong message: {error}"
        );
        assert_eq!(
            printed(
                "const a = {}; a.self = a;
                 try { JSON.stringify(a); } catch (e) { console.log('caught'); }"
            ),
            Ok("caught\n".to_owned())
        );
    }

    #[test]
    fn the_same_object_twice_side_by_side_is_not_a_cycle() {
        // The bug every cycle check written with a set instead of a stack has. Seeing an object
        // twice is fine, seeing it while it is still being written is not.
        assert_eq!(
            printed(
                "const shared = {v: 1};
                 console.log(JSON.stringify({a: shared, b: shared}));"
            ),
            Ok("{\"a\":{\"v\":1},\"b\":{\"v\":1}}\n".to_owned())
        );
    }

    #[test]
    fn a_replacer_function_is_refused_rather_than_ignored() {
        // Ignoring it would hand back an unfiltered object to a program that asked for a filtered
        // one, with nothing to say that happened.
        let error = printed("JSON.stringify({a: 1}, function (k, v) { return v; });")
            .expect_err("a replacer should be refused");
        assert!(error.contains("replacer"), "wrong message: {error}");
    }

    #[test]
    fn parse_refuses_by_name_and_cannot_be_caught() {
        // Not catchable on purpose. Wrapping `JSON.parse` in a `try` is how everybody writes it,
        // because bad input is the expected case, so a catchable gap would be read as bad input.
        let error = printed("JSON.parse('{}')").expect_err("parse should refuse");
        assert!(error.contains("JSON.parse"), "wrong message: {error}");
        let escaped = printed("try { JSON.parse('{}'); } catch (e) { console.log('caught'); }")
            .expect_err("our own gap should not be catchable");
        assert!(escaped.contains("JSON.parse"), "wrong message: {escaped}");
    }

    #[test]
    fn json_is_an_object_holding_two_functions() {
        assert_eq!(logged("typeof JSON"), "object");
        assert_eq!(logged("typeof JSON.stringify"), "function");
        assert_eq!(logged("typeof JSON.parse"), "function");
    }

    #[test]
    fn the_shape_of_a_benchmark_result_runs_end_to_end() {
        // The reason both of these builtins exist, written the way the workloads in
        // tamnd/katsu-bench write it. Not a timing assertion, because the number depends on the
        // machine. The assertion is that the whole shape produces the line a harness reads.
        assert_eq!(
            printed(
                "function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
                 const started = performance.now();
                 const checksum = fib(20);
                 const elapsed = performance.now() - started;
                 console.log(JSON.stringify({name: 'fib', checksum: checksum, ok: elapsed >= 0}));"
            ),
            Ok("{\"name\":\"fib\",\"checksum\":6765,\"ok\":true}\n".to_owned())
        );
    }
}
