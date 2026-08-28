//! Building programs that this engine can actually run, out of the parts most likely to disagree.
//!
//! The grammar is deliberately small and it is not small because generating more would be hard. It
//! is small because a generated program that stops at the first construct we have not implemented
//! tests nothing, and right now that is almost every construct. So the grammar is the subset the
//! interpreter runs today, and it grows one production at a time as the engine does.
//!
//! # What the literals are chosen for
//!
//! Every value in the tables below is there because it is a known place where two implementations
//! of the same specification stop agreeing. `1e21` is where number to string switches to
//! exponential notation and `1e-7` is where it switches at the other end. `-0` prints as `-0` and
//! compares equal to `0`. `2147483648` is one past what the bitwise operators truncate to.
//! `9007199254740993` is the first integer a double cannot hold. `"10"` against `"9"` is the string
//! comparison that catches an implementation comparing numerically. Random digits would find none
//! of these, because the interesting inputs are a vanishingly small part of the space.
//!
//! Programs are guaranteed to terminate. Loops are emitted with their own counter and a literal
//! bound, rather than with a generated condition, because a generator that can emit an endless loop
//! spends its time being killed by a timeout instead of finding disagreements.

use std::fmt::Write as _;

use crate::random::Random;

/// Values whose printed form or arithmetic is a known disagreement site.
///
/// Negative zero is written with its own parentheses, and it is the only entry that needs them,
/// because it is the only one that is a unary operator applied to a literal rather than a literal.
/// Bare, it turned `-` applied to it into `--0`, which is a decrement of a literal and a syntax
/// error, and it turned it into the left operand of `**`, which is also a syntax error. Both were
/// found by running the generator against node and reading what came back.
const NUMBERS: [&str; 20] = [
    "0",
    "(-0)",
    "1",
    "2",
    "3",
    "10",
    "0.1",
    "0.5",
    "1.5",
    "100",
    "255",
    "2147483647",
    "2147483648",
    "4294967295",
    "9007199254740993",
    "1e21",
    "1e-7",
    "1e308",
    "5e-324",
    "0.30000000000000004",
];

/// Strings chosen for how they behave when something coerces them.
const STRINGS: [&str; 10] = ["", "a", "b", "0", "1", "10", "9", " ", "NaN", "Infinity"];

/// The rest of the literals, kept apart because they are the coercion edge cases.
const OTHERS: [&str; 4] = ["true", "false", "null", "undefined"];

/// Binary operators the interpreter runs today.
///
/// `**` is here and it is the one that needs care when emitting, because an unparenthesised unary
/// on its left is a syntax error rather than a value. Every operand is parenthesised for that one
/// reason, which also removes precedence from the list of things a divergence could be about.
///
/// The comma operator is deliberately absent. The parser does not accept it yet, so every program
/// containing one would stop at the same unimplemented construct and report nothing about anything
/// else in the program. It goes in the day the parser takes it.
const BINARY: [&str; 23] = [
    "+", "-", "*", "/", "%", "**", "&", "|", "^", "<<", ">>", ">>>", "<", "<=", ">", ">=", "==",
    "!=", "===", "!==", "&&", "||", "??",
];

/// Unary operators the interpreter runs today.
const UNARY: [&str; 5] = ["-", "+", "!", "~", "typeof "];

/// Property names that can be written after a dot, which is the only member syntax the parser takes.
///
/// `$k` is in the table for one reason: the rule node uses to decide whether to leave a name unquoted
/// when it prints an object is narrower than the language's rule for an identifier, and the dollar
/// sign is exactly where the two part company. `$k` is a perfectly ordinary name to write and it
/// comes back quoted.
const NAMES: [&str; 6] = ["a", "b", "c", "x0", "_k", "$k"];

/// Property names that can only be written as a string inside a literal, because they are not
/// identifiers and there is no computed member syntax yet to read them back with.
///
/// No integer-like name is here, and that is a gap being left open on purpose rather than an
/// oversight. `{ b: 1, '1': 2 }` enumerates the integer first in node, this engine has no elements
/// yet and enumerates in insertion order, so every seed that drew one would report the same known
/// difference and drown out anything new. It goes in the day elements do.
const QUOTED_NAMES: [&str; 4] = ["a-b", "", "9a", "a b"];

/// A generated program and the seed it came from.
#[derive(Clone, Debug)]
pub(crate) struct Program {
    /// The seed, so a divergence report is one number somebody can rerun.
    pub(crate) seed: u64,
    /// The statements, kept apart rather than joined, because the shrinker removes them one at a
    /// time and a string would have to be re-split to do that.
    pub(crate) statements: Vec<String>,
}

impl Program {
    /// The source, which is the statements and nothing else.
    pub(crate) fn source(&self) -> String {
        let mut source = String::new();
        for statement in &self.statements {
            source.push_str(statement);
            source.push('\n');
        }
        source
    }

    /// The same program with statement `index` removed.
    pub(crate) fn without(&self, index: usize) -> Program {
        let mut smaller = self.clone();
        smaller.statements.remove(index);
        smaller
    }
}

/// Build one program from one seed.
///
/// The last statement always prints every variable in scope. A program that computes something and
/// never prints it is a program whose result no oracle can see, and two engines agree perfectly on
/// output nobody looked at.
pub(crate) fn program(seed: u64, statements: usize) -> Program {
    let mut random = Random::new(seed);
    let mut builder = Builder {
        random: &mut random,
        live: Vec::new(),
        mutable: Vec::new(),
        objects: Vec::new(),
        next: 0,
        loops: 0,
    };

    let mut body = Vec::new();
    for _ in 0..statements.max(1) {
        body.push(builder.statement(0));
    }
    if builder.live.is_empty() {
        // Nothing was declared, which happens when every draw came up a bare expression. Print
        // something rather than emitting a program with no observable behaviour at all.
        body.push("console.log('nothing was declared');".to_owned());
    } else {
        let names = builder.live.join(", ");
        body.push(format!("console.log({names});"));
    }

    Program {
        seed,
        statements: body,
    }
}

/// The state a program is built with: what is in scope and what to call the next thing.
struct Builder<'a> {
    random: &'a mut Random,
    /// Every name in scope, which is what the final print statement names.
    live: Vec<String>,
    /// The subset of `live` declared with `let`, which is what an assignment is allowed to target.
    mutable: Vec<String>,
    /// The subset of `live` that is known to be holding an object, which is what a property read or
    /// a property store is allowed to use as its receiver.
    ///
    /// Reading a property of a number or a string is `undefined` and reading one of `null` or
    /// `undefined` is a `TypeError` that ends the program, so the receiver cannot be drawn from
    /// `live`. Every name in here is declared `const`, which is what keeps the claim true for the
    /// rest of the program without following what each assignment did.
    objects: Vec<String>,
    next: usize,
    /// How many loops are open around the statement being built.
    ///
    /// A `break` or a `continue` with nothing around it is an early error rather than a program
    /// that runs, and a generator that emitted one would spend its run comparing two engines'
    /// syntax error messages instead of comparing what they compute.
    loops: usize,
}

impl Builder<'_> {
    /// One statement, at the given nesting depth.
    fn statement(&mut self, depth: usize) -> String {
        // Nested blocks stop producing more blocks, because the interesting part of a control
        // structure is the first level and the rest is width the generator pays for in program size.
        // A jump is drawn before the rest, because it is only legal some of the time and folding it
        // into the main draw would change what every other seed produces depending on where it was.
        if self.loops > 0 && self.random.chance(8) {
            return if self.random.chance(2) {
                "break;".to_owned()
            } else {
                "continue;".to_owned()
            };
        }

        let choice = if depth >= 2 {
            self.random.below(4)
        } else {
            self.random.below(9)
        };
        match choice {
            0 | 1 => self.declaration(),
            2 => self.object_declaration(),
            3 => self.property_store(),
            4 => self.assignment(),
            5 => self.branch(depth),
            6 => self.switch_statement(depth),
            7 => self.try_statement(depth),
            _ => self.loop_statement(depth),
        }
    }

    /// `let v3 = <expr>;`, and the name becomes visible to everything after it.
    ///
    /// A `const` goes into `live` but not into `mutable`, which is the whole reason those are two
    /// lists. Assigning to a `const` throws a `TypeError` that every engine agrees on, and a program
    /// that throws on its second line has stopped testing every line after it. Two lists is cheaper
    /// than losing the tail of every program that happened to draw a `const` early.
    fn declaration(&mut self) -> String {
        let name = format!("v{}", self.next);
        self.next += 1;
        let constant = self.random.chance(4);
        let value = self.expression(0);
        self.live.push(name.clone());
        if constant {
            return format!("const {name} = {value};");
        }
        self.mutable.push(name.clone());
        format!("let {name} = {value};")
    }

    /// `const v3 = { a: 1, 'a-b': <expr> };`, and the name is remembered as holding an object.
    ///
    /// Always `const`, and that is what makes every other object production safe rather than being a
    /// stylistic choice. A property read or a store needs a receiver that is not `null` or
    /// `undefined`, and a name an assignment can reach is a name that could be holding either by the
    /// time the store runs, which is a `TypeError` that ends the program and stops it testing
    /// everything after that line. A binding nothing can write costs less than following what every
    /// assignment did to it.
    fn object_declaration(&mut self) -> String {
        let name = format!("v{}", self.next);
        self.next += 1;
        let literal = self.object_literal(0);
        self.live.push(name.clone());
        self.objects.push(name.clone());
        format!("const {name} = {literal};")
    }

    /// `v3.a = <expr>;` or a compound form, and a declaration when there is nothing to store into.
    ///
    /// This is where a property that was not in the literal gets added, so it is the production that
    /// makes a generated program walk past the room its object was built with and into an overflow
    /// array, and the one that makes a name arrive after the ones the object was made with and have
    /// to enumerate there.
    fn property_store(&mut self) -> String {
        if self.objects.is_empty() {
            return self.object_declaration();
        }
        let receiver = self.random.pick(&self.objects.clone()).clone();
        let name = *self.random.pick(&NAMES);
        let operator = self.random.pick(&["=", "+=", "-=", "*=", "|=", "&="]);
        let value = self.expression(0);
        format!("{receiver}.{name} {operator} {value};")
    }

    /// `{ a: 1, b: <expr> }`, sometimes empty and sometimes with the same name twice.
    ///
    /// A duplicate name is not filtered out, because it is three rules at once and each of them is
    /// somewhere an engine can be wrong: the property is stored twice, the last value wins, and it
    /// keeps the position the first store gave it rather than moving to the end.
    fn object_literal(&mut self, depth: usize) -> String {
        let count = self.random.below(5);
        let mut properties = Vec::new();
        for _ in 0..count {
            let name = if self.random.chance(4) {
                format!("'{}'", self.random.pick(&QUOTED_NAMES))
            } else {
                (*self.random.pick(&NAMES)).to_owned()
            };
            let value = self.expression(depth + 1);
            properties.push(format!("{name}: {value}"));
        }
        if properties.is_empty() {
            return "{}".to_owned();
        }
        format!("{{ {} }}", properties.join(", "))
    }

    /// `v3.a`, which is `undefined` as often as not and is meant to be.
    ///
    /// Not chained. `v3.a.b` throws whenever `v3.a` has not been stored yet, which for a generated
    /// program is most of the time, and a production that usually ends the program is a production
    /// that tests the lines before it and nothing else.
    fn property_read(&mut self) -> String {
        let receiver = self.random.pick(&self.objects.clone()).clone();
        let name = *self.random.pick(&NAMES);
        format!("{receiver}.{name}")
    }

    /// `v1 = <expr>;` or a compound form, or a declaration when nothing is assignable yet.
    fn assignment(&mut self) -> String {
        if self.mutable.is_empty() {
            return self.declaration();
        }
        let name = self.random.pick(&self.mutable.clone()).clone();
        let operator = self.random.pick(&["=", "+=", "-=", "*=", "|=", "&="]);
        let value = self.expression(0);
        format!("{name} {operator} {value};")
    }

    /// `if (<expr>) { ... } else { ... }`, with the else half most of the time.
    fn branch(&mut self, depth: usize) -> String {
        let condition = self.expression(0);
        let taken = self.block(depth + 1);
        if self.random.chance(3) {
            return format!("if ({condition}) {{ {taken} }}");
        }
        let other = self.block(depth + 1);
        format!("if ({condition}) {{ {taken} }} else {{ {other} }}")
    }

    /// A loop with its own counter and a literal bound, so it cannot fail to terminate.
    ///
    /// The counter is not added to the live set, because it is left holding the bound in every run
    /// and printing it says nothing that the bound in the source does not already say.
    ///
    /// The increment is the first statement of the body rather than the last, and that is what makes
    /// a generated `continue` safe. With the increment at the bottom, a `continue` skips it and the
    /// loop never ends, so the generator would be back to being killed by timeouts.
    fn loop_statement(&mut self, depth: usize) -> String {
        let name = format!("i{}", self.next);
        self.next += 1;
        let bound = self.random.below(4) + 1;
        self.loops += 1;
        let body = self.block(depth + 1);
        self.loops -= 1;
        format!("let {name} = 0; while ({name} < {bound}) {{ {name}++; {body} }}")
    }

    /// `switch (<expr>) { case <literal>: ... }`, with a break in some clauses and not in others.
    ///
    /// Fallthrough is the reason this production exists. A clause with no break runs the next
    /// clause's body as well, and an engine that lowered every clause as if it ended in a break
    /// would pass every test written with the breaks in place. The default is put at a random
    /// position rather than last, because it is compared after every case wherever it is written
    /// and an engine that treated it as an else would still get the last position right.
    fn switch_statement(&mut self, depth: usize) -> String {
        let subject = self.expression(0);
        let mut clauses: Vec<String> = Vec::new();
        for _ in 0..=self.random.below(3) {
            let value = self.case_value();
            let body = self.block(depth + 1);
            let leave = if self.random.chance(2) { " break;" } else { "" };
            clauses.push(format!("case {value}: {body}{leave}"));
        }
        if self.random.chance(2) {
            let body = self.block(depth + 1);
            let at = self.random.below(clauses.len() + 1);
            clauses.insert(at, format!("default: {body}"));
        }
        format!("switch ({subject}) {{ {} }}", clauses.join(" "))
    }

    /// `let c3 = 0; try { ... throw <expr>; } catch (e3) { c3 = e3; ... }`.
    ///
    /// The extra binding is what makes the whole thing observable. A generated handler mentions the
    /// caught name only by accident, so without somewhere to put it the program would run both
    /// paths and print the same thing either way, and a difference in where a throw landed would
    /// never reach the output the harness compares.
    ///
    /// The throw is drawn rather than always emitted, because a `try` that always fires never runs
    /// the path where the protected block finishes, and that is the path almost every real `try`
    /// takes. The name goes out of scope at the closing brace like any other block binding, which
    /// is why it is pushed and truncated by hand here instead of going through `block`.
    fn try_statement(&mut self, depth: usize) -> String {
        let held = format!("c{}", self.next);
        let caught = format!("e{}", self.next);
        self.next += 1;
        self.live.push(held.clone());
        self.mutable.push(held.clone());

        let protected = self.block(depth + 1);
        let throw = if self.random.chance(2) {
            let value = self.expression(0);
            format!(" throw {value};")
        } else {
            String::new()
        };

        let live = self.live.len();
        let mutable = self.mutable.len();
        let objects = self.objects.len();
        self.live.push(caught.clone());
        let handler = self.statement(depth + 1);
        self.live.truncate(live);
        self.mutable.truncate(mutable);
        self.objects.truncate(objects);

        format!(
            "let {held} = 0; try {{ {protected}{throw} }} catch ({caught}) {{ {held} = {caught}; \
             {handler} }}"
        )
    }

    /// What to write after `case`.
    ///
    /// Half the draws are a small integer, and that is on purpose. The comparison is strict, so a
    /// clause whose value is `5e-324` is a clause that never runs, and a switch where nothing ever
    /// matches tests one jump and none of the bodies. Drawing from the literal tables the rest of
    /// the time keeps the cases where a coercion an engine should not be doing would make something
    /// match that should not have.
    fn case_value(&mut self) -> String {
        if self.random.chance(2) {
            return self.random.below(4).to_string();
        }
        match self.random.below(3) {
            0 => (*self.random.pick(&NUMBERS)).to_owned(),
            1 => format!("'{}'", self.random.pick(&STRINGS)),
            _ => (*self.random.pick(&OTHERS)).to_owned(),
        }
    }

    /// One statement inside braces, with everything it declares going out of scope after it.
    ///
    /// This is the part that is easy to leave out and expensive to leave out. A `let` inside an
    /// `if` block is visible only until the closing brace, so a builder that kept those names in
    /// the live set would end every such program with a print statement naming a variable that is
    /// no longer in scope. Every engine reports the same `ReferenceError` for that, so the run
    /// would agree with node on thousands of programs while testing none of them.
    fn block(&mut self, depth: usize) -> String {
        let live = self.live.len();
        let mutable = self.mutable.len();
        let objects = self.objects.len();
        let body = self.statement(depth);
        self.live.truncate(live);
        self.mutable.truncate(mutable);
        self.objects.truncate(objects);
        body
    }

    /// One expression, at the given depth.
    fn expression(&mut self, depth: usize) -> String {
        if depth >= 3 || self.random.chance(3) {
            return self.atom(depth);
        }
        match self.random.below(6) {
            0..=2 => {
                let left = self.expression(depth + 1);
                let right = self.expression(depth + 1);
                let operator = self.random.pick(&BINARY);
                // Both operands parenthesised, always. It is required for `**`, whose left operand
                // cannot be an unparenthesised unary, and it removes precedence from the set of
                // things a disagreement could be about, which is worth more than the tidier output.
                format!("({left} {operator} {right})")
            }
            3 => {
                let operand = self.expression(depth + 1);
                let operator = self.random.pick(&UNARY);
                format!("({operator}{operand})")
            }
            4 => {
                let condition = self.expression(depth + 1);
                let taken = self.expression(depth + 1);
                let other = self.expression(depth + 1);
                format!("({condition} ? {taken} : {other})")
            }
            _ => self.atom(depth),
        }
    }

    /// A literal or a variable that is in scope.
    ///
    /// An object literal is drawn here rather than given a production of its own, so that it turns
    /// up as an operand of whatever the expression above it happened to be. That is the point: an
    /// object on either side of `+` converts to text and joins, an object anywhere else in the
    /// arithmetic is NaN, and the two look nothing alike, so the operator has to be the one thing
    /// varying around it.
    fn atom(&mut self, depth: usize) -> String {
        if !self.objects.is_empty() && self.random.chance(5) {
            return self.property_read();
        }
        if depth < 2 && self.random.chance(8) {
            return self.object_literal(depth);
        }
        if !self.live.is_empty() && self.random.chance(2) {
            return self.random.pick(&self.live.clone()).clone();
        }
        match self.random.below(4) {
            0 | 1 => (*self.random.pick(&NUMBERS)).to_owned(),
            2 => {
                let mut quoted = String::from("\"");
                let text = *self.random.pick(&STRINGS);
                let _ = write!(quoted, "{text}\"");
                quoted
            }
            _ => (*self.random.pick(&OTHERS)).to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BINARY, NAMES, NUMBERS, QUOTED_NAMES, program};

    #[test]
    fn the_same_seed_gives_the_same_program() {
        // Without this a divergence report is a screenshot rather than a reproduction.
        assert_eq!(program(1234, 6).source(), program(1234, 6).source());
    }

    #[test]
    fn different_seeds_give_different_programs() {
        assert_ne!(program(1, 6).source(), program(2, 6).source());
    }

    #[test]
    fn every_program_prints_something() {
        // A program whose result nobody looks at is a program two engines agree on perfectly.
        for seed in 0..200 {
            assert!(
                program(seed, 5).source().contains("console.log("),
                "seed {seed} produced nothing observable"
            );
        }
    }

    #[test]
    fn every_program_ends_its_last_statement() {
        for seed in 0..200 {
            let source = program(seed, 5).source();
            let last = source.trim_end().lines().last().unwrap_or("").to_owned();
            assert!(last.ends_with(';'), "seed {seed}: {last}");
        }
    }

    #[test]
    fn a_program_can_have_a_statement_taken_out_of_it() {
        // What the shrinker does, so that a failure arrives as three lines rather than as forty.
        let full = program(77, 8);
        let smaller = full.without(2);
        assert_eq!(smaller.statements.len(), full.statements.len() - 1);
        assert_eq!(smaller.seed, full.seed);
    }

    #[test]
    fn a_constant_is_never_assigned_to() {
        // `const v0 = 1; v0 = 2;` throws a TypeError on line two, which every engine agrees on, so
        // the whole rest of the program stops being tested. Checked over enough seeds that the one
        // in four chance of drawing a const has come up many times.
        for seed in 0..500 {
            let source = program(seed, 8).source();
            let constants: Vec<String> = source
                .match_indices("const ")
                .map(|(at, _)| {
                    source[at + "const ".len()..]
                        .split(' ')
                        .next()
                        .unwrap_or("")
                        .to_owned()
                })
                .collect();
            for name in constants {
                for operator in ["=", "+=", "-=", "*=", "|=", "&="] {
                    let assignment = format!("{name} {operator} ");
                    for (at, _) in source.match_indices(&assignment) {
                        // The declaration itself matches the plain `=` form, so the one thing that
                        // makes this an assignment rather than a declaration is what comes before.
                        let before = &source[..at];
                        assert!(
                            before.ends_with("const ") || before.ends_with("let "),
                            "seed {seed} assigns to the constant {name}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn nothing_declared_inside_a_block_escapes_it() {
        // The final print names every live variable, so a name that went out of scope at a closing
        // brace would make the program end in a ReferenceError. Every engine reports that the same
        // way, which is agreement on a program that tested nothing.
        for seed in 0..500 {
            let full = program(seed, 8);
            let last = full.statements.last().cloned().unwrap_or_default();
            let Some(names) = last
                .strip_prefix("console.log(")
                .and_then(|rest| rest.strip_suffix(");"))
            else {
                continue;
            };
            // The program that declared nothing at all prints a string instead of a list of names,
            // which is a literal and not a name that could be out of scope.
            if names.starts_with('\'') {
                continue;
            }
            for name in names.split(", ").filter(|name| !name.is_empty()) {
                // A name is in scope at the end only if it was declared at the outermost level,
                // which for this generator means its declaration is a whole statement of its own.
                let declared = full.statements.iter().any(|statement| {
                    statement.starts_with(&format!("let {name} = "))
                        || statement.starts_with(&format!("const {name} = "))
                });
                assert!(declared, "seed {seed}: {name} is printed but out of scope");
            }
        }
    }

    #[test]
    fn a_property_is_only_reached_through_a_name_that_holds_an_object() {
        // Reading or writing a property of `null` or `undefined` is a TypeError that ends the
        // program, so a receiver drawn from every live name would end most programs part way
        // through and leave every line after that untested. A receiver has to be a name that was
        // declared with an object literal and cannot be assigned to since.
        for seed in 0..500 {
            let source = program(seed, 8).source();
            for (at, _) in source.match_indices('.') {
                let mut receiver: Vec<char> = source[..at]
                    .chars()
                    .rev()
                    .take_while(|letter| letter.is_ascii_alphanumeric() || *letter == '_')
                    .collect();
                receiver.reverse();
                let receiver: String = receiver.into_iter().collect();
                // Everything else with a dot in it is a number from the literal table or the
                // `console.log` at the end, and neither is a generated name.
                let generated = receiver.starts_with('v')
                    && receiver.len() > 1
                    && receiver[1..].chars().all(|letter| letter.is_ascii_digit());
                if !generated {
                    continue;
                }
                assert!(
                    source.contains(&format!("const {receiver} = {{")),
                    "seed {seed}: {receiver} is a receiver without holding an object"
                );
            }
        }
    }

    #[test]
    fn objects_turn_up_often_enough_to_be_worth_generating() {
        // A production that fires once in a thousand seeds is a production nobody is testing. This
        // is the check that a later change to the draw weights cannot quietly switch objects off.
        let built = (0..200)
            .filter(|&seed| program(seed, 8).source().contains(": "))
            .count();
        assert!(built > 100, "only {built} programs of 200 built an object");
    }

    #[test]
    fn no_generated_property_name_is_an_array_index() {
        // `{ b: 1, '1': 2 }` enumerates the integer first in node, and this engine has no elements
        // yet and enumerates in insertion order. Generating one would report the same known
        // difference on every seed that drew it and bury everything else under it.
        for name in NAMES.iter().chain(QUOTED_NAMES.iter()) {
            assert!(name.parse::<u32>().is_err(), "{name} is an array index");
        }
    }

    #[test]
    fn the_literals_are_the_disagreement_sites_rather_than_random_digits() {
        // Asserted rather than left to a comment, because somebody tidying this table into
        // `0..100` would remove the entire reason the generator finds anything.
        for wanted in ["(-0)", "1e21", "1e-7", "2147483648", "9007199254740993"] {
            assert!(NUMBERS.contains(&wanted), "{wanted} went missing");
        }
    }

    #[test]
    fn the_exponent_operator_never_gets_a_bare_unary_on_its_left() {
        // `-2 ** 2` is a syntax error rather than a value, so a generator that emits one spends its
        // run reporting our parser as correct and node as correct and nothing as tested. The first
        // version of this test only looked at the character before the operator and missed
        // `(-0 ** true)`, where the unary is part of a literal from the table, so it checks the
        // whole token now.
        assert!(BINARY.contains(&"**"));
        for seed in 0..500 {
            let source = program(seed, 6).source();
            assert!(!source.contains("--"), "seed {seed}: a decrement appeared");
            for (at, _) in source.match_indices("**") {
                let before = source[..at].trim_end();
                // A parenthesised left operand is always fine, which is the whole reason every
                // operand is parenthesised. Anything else is a single token from one of the tables
                // and the question is whether that token begins with a unary operator.
                if before.ends_with(')') {
                    continue;
                }
                let token = before.rsplit(['(', ' ']).next().unwrap_or("");
                let start = token.chars().next().unwrap_or(')');
                assert!(
                    !matches!(start, '-' | '+' | '!' | '~'),
                    "seed {seed}: a bare unary before ** in {token}"
                );
            }
        }
    }
}
