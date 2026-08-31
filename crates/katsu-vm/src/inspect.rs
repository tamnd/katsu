//! The rules `console.log` formats by, which are Node's rules rather than ones we chose.
//!
//! `console.log` does not convert its argument to a string, it inspects it, and what that looks
//! like is not specified anywhere. It is whatever `util.inspect` does, and every one of the rules in
//! this file was read off Node by running it rather than remembered. They are here rather than in
//! the interpreter because they are text formatting with no heap in them, which makes them testable
//! on their own, and because the interpreter's job is walking objects rather than deciding where a
//! line breaks.
//!
//! The differential harness compares our output with Node's byte for byte, so a rule that is nearly
//! right here is a failing test rather than a cosmetic difference. That is the point of writing them
//! down this precisely.

use std::fmt::Write;

/// How deep `console.log` prints before it gives up and writes `[Object]`.
///
/// Node's default is two, counted from the value's own children, so three levels of braces come out
/// and the fourth is elided. Counting the way this file counts, where zero is the elision, that is
/// three.
pub(crate) const DEPTH: u32 = 3;

/// The width Node tries to keep a printed object inside before breaking it over several lines.
const BREAK_LENGTH: usize = 80;

/// How far each nested level is indented once an object has been broken over several lines.
const INDENT: usize = 2;

/// A property name as it appears in printed output.
///
/// Node prints a name bare when it is a plain word and quotes it otherwise. The rule is narrower
/// than the language's own idea of an identifier: `$` is not in it, so `{$: 1}` prints as `{ '$': 1
/// }`, and neither is anything outside ASCII. Matching the language here instead of matching Node
/// would be a difference on real programs.
pub(crate) fn key(text: &str) -> String {
    let mut characters = text.chars();
    let plain = match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        }
        _ => false,
    };
    if plain { text.to_owned() } else { quote(text) }
}

/// A string value as it appears inside a printed object, quoted and escaped as Node does it.
///
/// The quote is chosen rather than fixed: a single quote normally, a double quote for a string with
/// a single quote in it, a backtick for one with both, and only when a string manages to contain all
/// three does the quote itself get escaped. That is three tests to avoid a backslash, and it is what
/// Node does, so it is what makes the output comparable.
pub(crate) fn quote(text: &str) -> String {
    let quote = if !text.contains('\'') {
        '\''
    } else if !text.contains('"') {
        '"'
    } else if !text.contains('`') {
        '`'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            _ if character == quote => {
                out.push('\\');
                out.push(character);
            }
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            // The C1 range is escaped as well as the C0 one, which is easy to miss because it is
            // invisible in a terminal either way, and `\u{a0}` immediately above it is not.
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {
                let _ = write!(out, "\\x{:02X}", character as u32);
            }
            _ => out.push(character),
        }
    }
    out.push(quote);
    out
}

/// What goes in front of an object that something inside it points back at.
///
/// The number is what ties the two halves of a cycle together, so `<ref *1>` on the object and
/// `[Circular *1]` where the walk found its way back to it are the same number by construction.
pub(crate) fn reference(number: usize) -> String {
    format!("<ref *{number}>")
}

/// What is printed where the walk arrives back at an object it is already inside.
pub(crate) fn circular(number: usize) -> String {
    format!("[Circular *{number}]")
}

/// What goes in front of an object that inherits from nothing.
///
/// Node says this because a bare object behaves differently from an ordinary one in ways its
/// contents do not show. It has no `toString`, no `hasOwnProperty` and nothing else, so printing it
/// as `{}` would be printing two different kinds of thing the same way. It is also what the depth
/// limit prints instead of `[Object]`, for the same reason.
pub(crate) const NULL_PROTOTYPE: &str = "[Object: null prototype]";

/// Wrap the printed properties of one object in braces, on one line if they fit.
///
/// Node's width rule is not "is the line under eighty characters", it is a count with a constant of
/// ten in it and the number of entries added twice, which means an object with many short properties
/// breaks earlier than its printed width suggests. The arithmetic is reproduced rather than
/// approximated, because an approximation is a diff against Node on some object nobody thought of.
///
/// `base` is the `<ref *1>` an object in a cycle carries and the `[Function: f]` a function with
/// properties carries, and empty for everything else. `tag` is the `Foo` in front of an instance and
/// the `[Object: null prototype]` in front of a bare object. Both are pasted on the front and both
/// count towards the width, so an object carrying one breaks earlier than the same object without
/// one, and that was measured rather than assumed.
///
/// They are separate arguments because Node charges them differently by one character. Node keeps
/// the reference in a variable of its own and builds the tag into the opening brace, space and all,
/// so `Foo {` is five characters against the one that a bare `{` costs while `<ref *1>` is charged
/// its own eight. Joining the two here and charging the result once is off by one on exactly one
/// object width, which is a diff against Node rather than a rounding difference.
pub(crate) fn braces(entries: &[String], indent: usize, base: &str, tag: &str) -> String {
    let mut prefix = String::new();
    for part in [base, tag] {
        if !part.is_empty() {
            prefix.push_str(part);
            prefix.push(' ');
        }
    }
    if entries.is_empty() {
        return format!("{prefix}{{}}");
    }
    // The opening brace on its own, or the tag and the space and the brace together.
    let open = if tag.is_empty() {
        "{".len()
    } else {
        width(tag) + "{ ".len()
    };
    if fits(entries, indent, width(base) + open)
        && !entries.iter().any(|entry| entry.contains('\n'))
    {
        return format!("{prefix}{{ {} }}", entries.join(", "));
    }
    let pad = " ".repeat(indent + INDENT);
    let separator = format!(",\n{pad}");
    format!(
        "{prefix}{{\n{pad}{}\n{}}}",
        entries.join(&separator),
        " ".repeat(indent)
    )
}

/// How far a value nested inside a printed object is indented.
pub(crate) const fn nested(indent: usize) -> usize {
    indent + INDENT
}

/// Whether these entries go on one line, by Node's arithmetic.
///
/// `prefix` is everything in front of the first entry, counted the way Node counts it.
fn fits(entries: &[String], indent: usize, prefix: usize) -> bool {
    let start = entries.len() + indent + prefix + 10;
    let mut total = entries.len() + start;
    if total + entries.len() > BREAK_LENGTH {
        return false;
    }
    for entry in entries {
        total += width(entry);
        if total > BREAK_LENGTH {
            return false;
        }
    }
    true
}

/// How long Node thinks this text is.
///
/// UTF-16 code units and not characters or bytes, because Node is counting the length of a
/// JavaScript string, and an emoji is two of those.
fn width(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

#[cfg(test)]
mod tests {
    use super::{braces, key, nested, quote};

    fn entries(count: usize, width: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("k{index}{}: 1", "x".repeat(width)))
            .collect()
    }

    #[test]
    fn a_plain_word_is_printed_without_quotes_and_everything_else_with_them() {
        assert_eq!(key("x"), "x");
        assert_eq!(key("_private"), "_private");
        assert_eq!(key("class"), "class");
        assert_eq!(key("a9"), "a9");
        assert_eq!(key("a-b"), "'a-b'");
        assert_eq!(key(""), "''");
        assert_eq!(key("9a"), "'9a'");
        // Node's rule is narrower than the language's, and these two are the whole difference.
        assert_eq!(key("$_9"), "'$_9'");
        assert_eq!(key("é"), "'é'");
    }

    #[test]
    fn the_quote_is_chosen_so_that_it_almost_never_has_to_be_escaped() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "\"it's\"");
        assert_eq!(quote("both ' and \""), "`both ' and \"`");
        assert_eq!(quote("all ' and \" and `"), "'all \\' and \" and `'");
    }

    #[test]
    fn the_escapes_are_the_ones_node_writes_and_the_hex_is_uppercase() {
        assert_eq!(quote("a\nb"), "'a\\nb'");
        assert_eq!(quote("a\tb"), "'a\\tb'");
        assert_eq!(quote("a\\b"), "'a\\\\b'");
        assert_eq!(quote("\u{8}\u{c}\r"), "'\\b\\f\\r'");
        assert_eq!(quote("\u{0}\u{b}\u{1a}"), "'\\x00\\x0B\\x1A'");
        // The C1 range is escaped and the character just past it is not.
        assert_eq!(quote("\u{7f}\u{9f}\u{a0}"), "'\\x7F\\x9F\u{a0}'");
    }

    #[test]
    fn an_object_with_nothing_in_it_has_no_space_between_its_braces() {
        assert_eq!(braces(&[], 0, "", ""), "{}");
        assert_eq!(braces(&[], 0, "", "Foo"), "Foo {}");
    }

    #[test]
    fn a_short_object_goes_on_one_line() {
        assert_eq!(
            braces(&["a: 1".to_owned(), "b: 2".to_owned()], 0, "", ""),
            "{ a: 1, b: 2 }"
        );
        assert_eq!(
            braces(&["a: 1".to_owned()], 0, "<ref *1>", "Foo"),
            "<ref *1> Foo { a: 1 }"
        );
    }

    #[test]
    fn the_line_breaks_where_node_breaks_it() {
        // Measured against Node rather than derived: an entry of exactly this width is where each
        // of these counts flips, and one character narrower still fits.
        for (count, width) in [(1, 63), (2, 28), (3, 17), (4, 11), (5, 7), (6, 5), (7, 3)] {
            assert!(
                braces(&entries(count, width), 0, "", "").contains('\n'),
                "{count} entries {width} wide should have broken"
            );
            assert!(
                !braces(&entries(count, width - 1), 0, "", "").contains('\n'),
                "{count} entries {} wide should have fitted",
                width - 1
            );
        }
    }

    #[test]
    fn a_constructor_name_costs_one_more_than_its_own_length() {
        // Measured against Node with `new Foo()` carrying a single long property: the name and the
        // space and the brace are counted together, so a three character name moves the boundary by
        // four characters and not by three. Off by one here is a diff on real output rather than a
        // rounding difference, which is why the two boundaries either side of it are pinned.
        let entry = |width: usize| vec![format!("{}: 1", "k".repeat(width))];
        assert!(braces(&entry(65), 0, "", "").contains('\n'));
        assert!(!braces(&entry(64), 0, "", "").contains('\n'));
        assert!(braces(&entry(61), 0, "", "Foo").contains('\n'));
        assert!(!braces(&entry(60), 0, "", "Foo").contains('\n'));
        // The same boundary for the longest tag Node writes, which is the bare object one.
        assert!(braces(&entry(40), 0, "", super::NULL_PROTOTYPE).contains('\n'));
        assert!(!braces(&entry(39), 0, "", super::NULL_PROTOTYPE).contains('\n'));
    }

    #[test]
    fn a_reference_goes_in_front_of_the_braces_and_counts_towards_the_width() {
        let entry = |width: usize| {
            vec![
                format!("{}: 1", "k".repeat(width)),
                "s: [Circular *1]".to_owned(),
            ]
        };
        assert_eq!(
            braces(&["s: [Circular *1]".to_owned()], 0, "<ref *1>", ""),
            "<ref *1> { s: [Circular *1] }"
        );
        // Measured against Node: the same object breaks at a name of thirty nine characters with the
        // reference on it and would fit to forty seven without, so the eight characters of
        // `<ref *1>` are being charged even though they are not inside the braces.
        assert!(braces(&entry(39), 0, "<ref *1>", "").contains('\n'));
        assert!(!braces(&entry(38), 0, "<ref *1>", "").contains('\n'));
        assert!(braces(&entry(47), 0, "", "").contains('\n'));
        assert!(!braces(&entry(46), 0, "", "").contains('\n'));
    }

    #[test]
    fn a_broken_object_indents_by_two_and_puts_the_closing_brace_back_at_the_start() {
        let text = braces(&entries(7, 3), 0, "", "");
        assert_eq!(
            text,
            "{\n  k0xxx: 1,\n  k1xxx: 1,\n  k2xxx: 1,\n  k3xxx: 1,\n  k4xxx: 1,\n  k5xxx: 1,\n  k6xxx: 1\n}"
        );
    }

    #[test]
    fn an_entry_that_is_already_broken_breaks_the_object_around_it() {
        // One long property is enough to put every other property on its own line, which is why the
        // newline test is separate from the width test rather than folded into it.
        let inner = braces(&entries(7, 3), nested(0), "", "");
        assert!(braces(&[format!("a: {inner}")], 0, "", "").starts_with("{\n  a: {\n    k0xxx"));
    }
}
