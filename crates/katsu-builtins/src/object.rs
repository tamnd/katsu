//! `Object`, and the statics of it that describe and reach a prototype chain.
//!
//! Every object a program makes now inherits from `Object.prototype`, and this is where a program
//! can see that: `Object.getPrototypeOf({})` is that object, `Object.create(p)` makes something that
//! inherits from `p`, and a property that is not on an object is looked for above it.
//!
//! # What is here
//!
//! `Object` itself as a callable function, `Object.prototype`, `Object.create`,
//! `Object.getPrototypeOf`, `Object.defineProperty`, `Object.defineProperties` and
//! `Object.getOwnPropertyDescriptor`, and on the prototype itself `constructor`, `hasOwnProperty`,
//! `isPrototypeOf`, `propertyIsEnumerable`, `toString` and `valueOf`.
//!
//! # Why the prototype was empty until now
//!
//! Every method on `Object.prototype` needed two things that arrived separately. It needed
//! attributes, because a method at the top of nearly every chain in the realm has to be hidden or it
//! shows up in every object printed, everything serialised and every `for in`. And it needed
//! receivers, because each of these methods is a question about the object it was called on and
//! until a call site kept that object there was nothing to ask about. Both are here, so they are.
//!
//! # Defining a property is not assigning to one
//!
//! They look alike and almost nothing about them is the same. Assignment asks the prototype chain
//! whether the write is allowed and a definition does not, assignment cannot change what a property
//! is allowed to do and a definition can, assignment to a read only property fails where a
//! definition on a configurable one succeeds, and a definition leaves out any flag it is not given
//! rather than defaulting it the way a fresh property does. Both eventually put a value in a slot,
//! which is the only part they have in common.
//!
//! The rules for whether a definition is allowed are in [`apply`], written out as the specification
//! writes them and measured against Node case by case rather than remembered. The short version is
//! that a configurable property can be redefined however the caller likes, and a non configurable
//! one can only have its value changed, only if it is writable, and can only ever become less
//! permissive.
//!
//! # `Object` is a function
//!
//! It answers `function` to `typeof`, it prints as `[Function: Object]`, and `Object.getPrototypeOf`
//! finds the same `Function.prototype` above it that it finds above any function a program writes.
//! For a while it was a namespace object with the right names and the wrong type tag, because a
//! function written in Rust had nowhere to keep `create` and `prototype`. A function carries its own
//! properties now, so it does not have to be one thing or the other.
//!
//! `Object.prototype.constructor` came with it and points back here, which is what makes
//! `({}).constructor === Object` true.
//!
//! # What is not here
//!
//! `new Object()`, which needs `new`. `Object(x)` as a call is here, and the two are the same
//! operation with the same answer for every argument, so the second one arriving is mostly a matter
//! of routing.
//!
//! `Object(1)` and the other primitives, which box into a wrapper that does not exist yet.
//!
//! `Object.create(f)`, meaning an object that inherits from a function. A prototype link points at an
//! ordinary object and a function keeps its properties beside it, so the link would point at the side
//! object and `Object.getPrototypeOf` would answer with something that is not the function.
//!
//! Accessors, meaning `get` and `set` in a descriptor. There is now a receiver to call a getter
//! with, and what is left is the storage: a slot has to be able to hold a pair of functions instead
//! of a value, and a shape node has to say that it does. A descriptor carrying either one refuses by
//! name rather than being read as a data descriptor and silently defining the wrong thing. The one
//! case that is a real `TypeError` rather than a gap, a descriptor with both an accessor and a
//! value, still throws what Node throws.
//!
//! `Object.prototype.toLocaleString`, which is defined as calling `this.toString()`. Calling a value
//! from Rust is not something a native can do yet, and writing the same answer out twice would give
//! the wrong one for any object that overrides `toString`.
//!
//! `__defineGetter__` and the three others like it, which are accessors under an older spelling.
//!
//! Calling one of these methods on a primitive. `ToObject` boxes a number or a string into a wrapper
//! and there are no wrapper prototypes, so anything that is not an ordinary object refuses by name.
//! No program can reach that today, because reaching a method through a primitive needs the same
//! wrapper prototypes, and it is written down so that it stays true when they arrive.
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
//! `Object.freeze`, `Object.seal` and the two questions that go with them. Making every existing
//! property non writable and non configurable is a loop over what is already here, but the other
//! half of freezing is that the object stops accepting new properties, and there is nowhere to
//! record that yet. Where extensibility lives is a decision about the object model rather than a few
//! lines in this file, so the whole group waits for it.

use katsu_vm::{Attributes, Interpreter, NativeFn, RuntimeError, Value, arg, this_value};

/// Put `Object` in the global scope.
///
/// # Errors
///
/// Returns [`RuntimeError::OutOfMemory`] if the heap has no room for the object, its functions or
/// `Object.prototype`, which at startup means the heap is far too small rather than that anything
/// went wrong here.
pub fn install(interpreter: &mut Interpreter) -> Result<(), RuntimeError> {
    let prototype = interpreter.object_prototype()?;
    let create = interpreter.native_function("create", create)?;
    let get_prototype_of = interpreter.native_function("getPrototypeOf", get_prototype_of)?;
    let define_property = interpreter.native_function("defineProperty", define_property)?;
    let define_properties = interpreter.native_function("defineProperties", define_properties)?;
    let describe = interpreter.native_function("getOwnPropertyDescriptor", describe)?;
    // A function, which is what `Object` is, and not a namespace object that happens to hold the
    // same names. That is what makes `typeof Object` answer "function", and it is only possible now
    // that a function written in Rust can carry properties.
    let object = interpreter.native_function("Object", call)?;
    // Non enumerable, like every static in the language. `Object.keys(Object)` is empty in Node and
    // it is empty here, and a `for in` over `Object` walks nothing.
    for (name, value) in [
        ("create", create),
        ("getPrototypeOf", get_prototype_of),
        ("defineProperty", define_property),
        ("defineProperties", define_properties),
        ("getOwnPropertyDescriptor", describe),
    ] {
        interpreter.define_property(object, name, value, Attributes::BUILTIN)?;
    }
    // `Object.prototype` is the one property here that is none of the three things. Nothing can
    // rewrite it, hide it or remove it, because the top of every prototype chain in the realm moving
    // out from under running code is not something the language is willing to allow.
    interpreter.define_property(object, "prototype", prototype, Attributes::NONE)?;
    install_prototype(interpreter, prototype)?;
    // The other half of that link, which had to wait for `Object` to be something worth pointing at.
    // Every object in the realm inherits `constructor` from here, so `({}).constructor === Object`.
    interpreter.define_property(prototype, "constructor", object, Attributes::BUILTIN)?;
    interpreter.define_global("Object", object)
}

/// `Object(value)`, which is `ToObject` with the empty case filled in.
///
/// `Object()` and `Object(undefined)` and `Object(null)` all make a new empty object, and anything
/// that is already an object, function included, is handed straight back rather than copied. What is
/// left is a primitive, which boxes into a wrapper that does not exist yet and refuses by name.
///
/// `new Object()` is the same operation and is not here, because `new` is not here.
fn call(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = arg(args, 0);
    if value.is_undefined() || value.is_null() {
        let prototype = interpreter.object_prototype()?;
        return interpreter.new_object_with_prototype(prototype);
    }
    if interpreter.is_ordinary_object(value) || interpreter.is_callable(value) {
        return Ok(value);
    }
    let what = interpreter.display(value);
    Err(RuntimeError::Unsupported(format!(
        "Object({what}) is not supported yet, because boxing a primitive needs the wrapper prototypes"
    )))
}

/// Put the methods every object inherits onto `Object.prototype`.
///
/// All of them non enumerable, writable and configurable, which is what Node reports for each one
/// and what [`Attributes::BUILTIN`] means. That is not a detail: these live at the top of nearly
/// every prototype chain in the realm, so an enumerable one would appear in every `for in` over
/// every object in the program.
fn install_prototype(interpreter: &mut Interpreter, prototype: Value) -> Result<(), RuntimeError> {
    for (name, call) in [
        ("hasOwnProperty", has_own_property as NativeFn),
        ("isPrototypeOf", is_prototype_of),
        ("propertyIsEnumerable", property_is_enumerable),
        ("toString", to_string),
        ("valueOf", value_of),
    ] {
        let function = interpreter.native_function(name, call)?;
        interpreter.define_property(prototype, name, function, Attributes::BUILTIN)?;
    }
    Ok(())
}

/// `Object.prototype.hasOwnProperty(key)`.
///
/// Own and not inherited, which is the whole reason the method exists. `({}).hasOwnProperty(
/// 'toString')` is false even though `({}).toString` is a function, because the function is on the
/// prototype and the question is about the object.
fn has_own_property(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let key = interpreter.to_text(arg(args, 0))?;
    let object = called_on(interpreter, receiver, "Object.prototype.hasOwnProperty")?;
    Ok(Value::from_bool(
        interpreter.own_descriptor(object, &key).is_some(),
    ))
}

/// `Object.prototype.isPrototypeOf(value)`.
///
/// Anywhere on the chain rather than immediately above, which is the difference between this and
/// comparing against `Object.getPrototypeOf`. Anything that is not an object answers false rather
/// than throwing, because a primitive has no chain to be on and that is an answer rather than a
/// mistake.
fn is_prototype_of(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let object = called_on(interpreter, receiver, "Object.prototype.isPrototypeOf")?;
    let mut walking = arg(args, 0);
    // The value itself does not count, so the walk starts one above it. `o.isPrototypeOf(o)` is
    // false in Node and the loop below is what makes it false here.
    while let Some(above) = interpreter.prototype_of(walking) {
        if above == object {
            return Ok(Value::TRUE);
        }
        walking = above;
    }
    Ok(Value::FALSE)
}

/// `Object.prototype.propertyIsEnumerable(key)`.
///
/// False for a name the object does not have of its own, which folds two different answers into
/// one: a property that is hidden and a property that is not there both say false, and the way to
/// tell them apart is `Object.getOwnPropertyDescriptor`.
fn property_is_enumerable(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let key = interpreter.to_text(arg(args, 0))?;
    let object = called_on(
        interpreter,
        receiver,
        "Object.prototype.propertyIsEnumerable",
    )?;
    let enumerable = interpreter
        .own_descriptor(object, &key)
        .is_some_and(|(_, attributes)| attributes.is_enumerable());
    Ok(Value::from_bool(enumerable))
}

/// `Object.prototype.toString()`, which is where `[object Object]` comes from.
///
/// This one answers for every value rather than going through [`called_on`], because that is what
/// the specification asks for: `undefined` and `null` have their own tags here instead of throwing
/// the way every other method on this prototype does. They are unreachable until `call` exists,
/// since a plain `x.toString()` on either throws before the method is found, and they are written
/// out anyway because the alternative is a method that is right about the values it can see today
/// and wrong about the ones it will see next.
fn to_string(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = this_value(receiver, "Object.prototype.toString")?;
    let tag = if value.is_undefined() {
        "Undefined"
    } else if value.is_null() {
        "Null"
    } else if interpreter.is_callable(value) {
        "Function"
    } else if interpreter.is_ordinary_object(value) {
        "Object"
    } else {
        // A primitive, which boxes into a wrapper whose tag is the wrapper's name. There are no
        // wrapper prototypes to reach this through yet, so `Symbol.toStringTag` and the exotic tags
        // arrive with them rather than being guessed at here.
        return Err(RuntimeError::Unsupported(
            "Object.prototype.toString is not supported yet for a primitive, because it needs the wrapper prototypes".to_owned(),
        ));
    };
    interpreter.new_string(&format!("[object {tag}]"))
}

/// `Object.prototype.valueOf()`, which for an ordinary object is the object.
///
/// It exists so that a conversion has something to call and so that a type which does have a
/// primitive value has something to override. Answering with the receiver unchanged is the whole
/// implementation for everything that does not.
fn value_of(
    interpreter: &mut Interpreter,
    receiver: Option<Value>,
    _args: &[Value],
) -> Result<Value, RuntimeError> {
    called_on(interpreter, receiver, "Object.prototype.valueOf")
}

/// The object a method on `Object.prototype` was called on, or why there is not one.
///
/// Every one of these methods begins with `ToObject(this)`, and that step has three outcomes.
/// `undefined` and `null` throw, in the words Node uses. An ordinary object is itself. Everything
/// else boxes into a wrapper, and there are no wrapper prototypes yet, so it refuses by name rather
/// than answering about a box that was never made.
fn called_on(
    interpreter: &Interpreter,
    receiver: Option<Value>,
    name: &str,
) -> Result<Value, RuntimeError> {
    let value = this_value(receiver, name)?;
    if value.is_undefined() || value.is_null() {
        return Err(RuntimeError::Type(
            "Cannot convert undefined or null to object".to_owned(),
        ));
    }
    // A function is an object and these methods work on one, which is how `hasOwnProperty.call(f,
    // 'x')` answers. What is left here is a primitive, which needs a wrapper to be asked at all.
    if !interpreter.is_ordinary_object(value) && !interpreter.is_callable(value) {
        return Err(RuntimeError::Unsupported(format!(
            "{name} is not supported yet for a value that is not an ordinary object, because it needs the wrapper prototypes"
        )));
    }
    Ok(value)
}

/// `Object.create(prototype, descriptors)`.
///
/// The second argument is `Object.defineProperties` on the new object, and it is the same code
/// rather than a second copy of the rules, because the specification defines it that way and two
/// copies of `ValidateAndApplyPropertyDescriptor` would eventually disagree.
///
/// `undefined` is not the same as absent for the first argument. `Object.create()` is a `TypeError`
/// in Node, because the argument is required to be an object or `null` and `undefined` is neither,
/// so the missing argument falls into the same message rather than being defaulted. It is the other
/// way around for the second, where absent means there is nothing to define.
///
/// Inheriting from a function is legal and refuses by name. A prototype link lives in the shape and
/// points at an object, and a function keeps its properties in a side object, so the link would have
/// to point at that side object instead. Every property lookup through it would find the right
/// answer and `Object.getPrototypeOf` would hand back something that is not the function, which is a
/// wrong answer rather than a missing one.
fn create(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    if interpreter.is_callable(arg(args, 0)) {
        return Err(RuntimeError::Unsupported(
            "Object.create is not supported yet for a function, because a prototype link can only point at an ordinary object".to_owned(),
        ));
    }
    let object = interpreter.new_object_with_prototype(arg(args, 0))?;
    let descriptors = arg(args, 1);
    if descriptors.is_undefined() {
        return Ok(object);
    }
    define_properties(interpreter, None, &[object, descriptors])
}

/// `Object.getPrototypeOf(value)`.
///
/// Three outcomes and they are three different things. An object answers with its prototype or with
/// `null`. `undefined` and `null` throw, in the words Node uses, because there is no object to ask.
/// Every other primitive has an answer in the specification, which is the prototype of the wrapper
/// it would be converted to, and this build has no wrapper prototypes, so it refuses by name instead
/// of saying `null` and being believed.
fn get_prototype_of(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = arg(args, 0);
    if value.is_undefined() || value.is_null() {
        return Err(RuntimeError::Type(
            "Cannot convert undefined or null to object".to_owned(),
        ));
    }
    if let Some(prototype) = interpreter.prototype_of(value) {
        return Ok(prototype);
    }
    // A function inherits from `Function.prototype` whether or not it has been given a properties
    // object of its own, so this is answered from the realm rather than from the function.
    if interpreter.is_callable(value) {
        return interpreter.function_prototype();
    }
    Err(RuntimeError::Unsupported(
        "Object.getPrototypeOf is not supported yet for a primitive, because it needs the wrapper prototypes".to_owned(),
    ))
}

/// A property descriptor as it was written, with the fields that were left out still left out.
///
/// Every field is an option and that is the whole point of the type. `{}` and `{value: undefined}`
/// are different descriptors, `{writable: false}` against an existing property changes one flag and
/// leaves two alone, and the same descriptor against a name that does not exist yet creates a
/// property that is not writable, not enumerable and not configurable. None of that survives reading
/// a descriptor into four booleans and a value.
#[derive(Clone, Copy, Default)]
struct Descriptor {
    value: Option<Value>,
    getter: Option<Value>,
    setter: Option<Value>,
    writable: Option<bool>,
    enumerable: Option<bool>,
    configurable: Option<bool>,
}

impl Descriptor {
    /// Whether this descriptor describes an accessor.
    ///
    /// Mentioning either half is enough, and mentioning it as `undefined` still counts.
    /// `{get: undefined}` makes an accessor with no getter rather than a data property, which is
    /// what stops `Object.defineProperty(o, 'x', {get: undefined})` from quietly making `o.x` a
    /// property holding `undefined`.
    fn is_accessor(self) -> bool {
        self.getter.is_some() || self.setter.is_some()
    }
}

/// `Object.defineProperty(target, key, descriptor)`.
///
/// Answers with the target, which is what makes `Object.defineProperty(o, 'x', d).x` work and is the
/// only reason it returns anything at all.
fn define_property(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let target = arg(args, 0);
    // A function counts, and it is the reason this asks two questions instead of one. Defining a
    // static on a constructor is how half of the standard library is written.
    if !interpreter.is_ordinary_object(target) && !interpreter.is_callable(target) {
        return Err(RuntimeError::Type(
            "Object.defineProperty called on non-object".to_owned(),
        ));
    }
    let key = interpreter.to_text(arg(args, 1))?;
    let descriptor = read_descriptor(interpreter, arg(args, 2))?;
    apply(interpreter, target, &key, descriptor)?;
    Ok(target)
}

/// `Object.defineProperties(target, descriptors)`.
///
/// Every descriptor is read before any of them is applied, which is the specification's order and is
/// observable: `Object.defineProperties({}, {a: {value: 1}, b: 2})` throws over `b` and leaves `a`
/// undefined rather than half done. That was measured against Node rather than assumed, because a
/// loop that reads and applies one at a time is the obvious way to write this and it is wrong.
///
/// A primitive other than `undefined` and `null` has no own properties to read, so it defines
/// nothing and does not complain, which is also what Node does.
fn define_properties(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let target = arg(args, 0);
    if !interpreter.is_ordinary_object(target) && !interpreter.is_callable(target) {
        return Err(RuntimeError::Type(
            "Object.defineProperties called on non-object".to_owned(),
        ));
    }
    let source = arg(args, 1);
    if source.is_undefined() || source.is_null() {
        return Err(RuntimeError::Type(
            "Cannot convert undefined or null to object".to_owned(),
        ));
    }
    let entries = interpreter.own_properties(source).unwrap_or_default();
    let mut read = Vec::with_capacity(entries.len());
    for (key, value, attributes) in entries {
        // A descriptors object that keeps its descriptors behind getters, which is legal and rare.
        // Reading one means calling it, so this refuses by name rather than handing `read_descriptor`
        // a pair of functions and letting it report that the descriptor is not an object.
        if attributes.is_accessor() {
            return Err(Interpreter::native_met_an_accessor(&key));
        }
        read.push((key, read_descriptor(interpreter, value)?));
    }
    for (key, descriptor) in read {
        apply(interpreter, target, &key, descriptor)?;
    }
    Ok(target)
}

/// `Object.getOwnPropertyDescriptor(object, key)`.
///
/// `undefined` for a name the object does not have of its own, including one it inherits, because
/// this question is about the object and not about its chain.
fn describe(
    interpreter: &mut Interpreter,
    _receiver: Option<Value>,
    args: &[Value],
) -> Result<Value, RuntimeError> {
    let value = arg(args, 0);
    if value.is_undefined() || value.is_null() {
        return Err(RuntimeError::Type(
            "Cannot convert undefined or null to object".to_owned(),
        ));
    }
    let key = interpreter.to_text(arg(args, 1))?;
    let callable = interpreter.is_callable(value);
    if !interpreter.is_ordinary_object(value) && !callable {
        return unwrappable(interpreter, value);
    }
    let Some((value, attributes)) = interpreter.own_descriptor(value, &key) else {
        // A function has `name` and `length` in Node and does not have them here, so answering
        // `undefined` for those two would be a wrong answer rather than a missing one. Every other
        // name really is absent.
        if callable && (key == "name" || key == "length") {
            return Err(RuntimeError::Unsupported(format!(
                "Object.getOwnPropertyDescriptor is not supported yet for '{key}' on a function, because a function does not carry that property yet"
            )));
        }
        return Ok(Value::UNDEFINED);
    };
    let enumerable = Value::from_bool(attributes.is_enumerable());
    let configurable = Value::from_bool(attributes.is_configurable());
    // An accessor descriptor has four fields and not five, and the two it does not share with a data
    // descriptor come first for the same reason `value` does. Reporting a getter is not calling it,
    // so this is a question a builtin can answer about an accessor even though reading the property
    // is not.
    if attributes.is_accessor() {
        let (getter, setter) = interpreter.accessor_halves(value);
        return interpreter.host_object(&[
            ("get", getter.unwrap_or(Value::UNDEFINED)),
            ("set", setter.unwrap_or(Value::UNDEFINED)),
            ("enumerable", enumerable),
            ("configurable", configurable),
        ]);
    }
    let writable = Value::from_bool(attributes.is_writable());
    // In this order, because it is the order Node prints and the order `JSON.stringify` writes, and
    // a descriptor is a thing people read rather than a thing programs mostly index into.
    interpreter.host_object(&[
        ("value", value),
        ("writable", writable),
        ("enumerable", enumerable),
        ("configurable", configurable),
    ])
}

/// What to say about a primitive that was asked to describe a property of itself.
///
/// A number, a boolean or a symbol boxes into a wrapper that never has own properties, so the honest
/// answer is `undefined` and it happens to be the same answer Node gives. A string is different: its
/// wrapper carries `length` and one property per character, so answering `undefined` would be a
/// wrong answer rather than a missing one, and it refuses by name instead.
fn unwrappable(interpreter: &Interpreter, value: Value) -> Result<Value, RuntimeError> {
    if interpreter.as_text(value).is_some() {
        return Err(RuntimeError::Unsupported(
            "Object.getOwnPropertyDescriptor is not supported yet for a string, because it needs the wrapper prototypes".to_owned(),
        ));
    }
    Ok(Value::UNDEFINED)
}

/// Read a descriptor object into the fields it actually mentions.
///
/// The reads walk the prototype chain, because the specification uses `Get` and not a own property
/// lookup, so `Object.create({value: 7})` is a descriptor that says `value` is seven. That is not a
/// hypothetical: it is how a program shares one descriptor between many definitions.
fn read_descriptor(
    interpreter: &mut Interpreter,
    value: Value,
) -> Result<Descriptor, RuntimeError> {
    if !interpreter.is_ordinary_object(value) {
        let what = interpreter.display(value);
        return Err(RuntimeError::Type(format!(
            "Property description must be an object: {what}"
        )));
    }
    let getter = interpreter.lookup(value, "get")?;
    let setter = interpreter.lookup(value, "set")?;
    let held = interpreter.lookup(value, "value")?;
    let writable = interpreter.lookup(value, "writable")?;
    let enumerable = interpreter.lookup(value, "enumerable")?;
    let configurable = interpreter.lookup(value, "configurable")?;
    // A flag is whatever it is, converted. `{enumerable: 'yes'}` makes an enumerable property and
    // `{writable: 0}` makes a read only one, because the specification says `ToBoolean` and not
    // "must be a boolean".
    let descriptor = Descriptor {
        value: held,
        getter,
        setter,
        writable: writable.map(|flag| interpreter.is_truthy(flag)),
        enumerable: enumerable.map(|flag| interpreter.is_truthy(flag)),
        configurable: configurable.map(|flag| interpreter.is_truthy(flag)),
    };
    if descriptor.is_accessor() && (descriptor.value.is_some() || descriptor.writable.is_some()) {
        return Err(RuntimeError::Type(
            "Invalid property descriptor. Cannot both specify accessors and a value or writable attribute, #<Object>".to_owned(),
        ));
    }
    // A half that is present has to be callable or `undefined`, and the two halves are checked in
    // the order they are written above because a descriptor with both wrong reports the getter.
    callable_or_absent(interpreter, descriptor.getter, "Getter")?;
    callable_or_absent(interpreter, descriptor.setter, "Setter")?;
    Ok(descriptor)
}

/// Reject a `get` or a `set` that is neither a function nor `undefined`.
///
/// `undefined` is allowed and means the half is absent, which is not the same as the field being
/// left out: `{get: undefined}` is still an accessor descriptor. `null` is not allowed, which looks
/// arbitrary and is what the specification says and what Node does.
fn callable_or_absent(
    interpreter: &Interpreter,
    half: Option<Value>,
    which: &str,
) -> Result<(), RuntimeError> {
    let Some(half) = half else { return Ok(()) };
    if half.is_undefined() || interpreter.is_callable(half) {
        return Ok(());
    }
    let what = interpreter.display(half);
    Err(RuntimeError::Type(format!(
        "{which} must be a function: {what}"
    )))
}

/// A half of an accessor descriptor as something to store, where `undefined` means absent.
///
/// The descriptor keeps `Some(undefined)` and `None` apart because whether the field was written
/// decides whether the descriptor is an accessor at all. Once that question is settled, a half
/// written as `undefined` and a half left out mean the same thing to the pair, which is that there
/// is no function on that side.
fn half(value: Option<Value>) -> Option<Value> {
    value.filter(|value| !value.is_undefined())
}

/// The specification's `ValidateAndApplyPropertyDescriptor`, on an object that is always extensible.
///
/// A name that is not there yet is created, and every flag the descriptor does not mention is false
/// rather than true. That asymmetry against plain assignment catches people out and it is the rule:
/// `o.x = 1` makes a property that can do everything and `Object.defineProperty(o, 'x', {value: 1})`
/// makes one that can do nothing.
///
/// A name that is there and is configurable can be redefined into anything, because configurable
/// means exactly that, and that includes turning a data property into an accessor or back. A name
/// that is there and is not configurable is nearly frozen: it cannot become configurable again, its
/// enumerability cannot change, it cannot change which kind of property it is, and beyond that the
/// two kinds are frozen differently. A non writable data property cannot become writable and its
/// value cannot change to a different value, where writing the same value back is allowed, which is
/// why this needs `SameValue` and not `===` and why a non writable `NaN` can be redefined to `NaN`
/// while a positive zero cannot be redefined to a negative one. A non configurable accessor cannot
/// have either half changed at all, because there is no writable flag standing between them and a
/// caller.
fn apply(
    interpreter: &mut Interpreter,
    target: Value,
    key: &str,
    descriptor: Descriptor,
) -> Result<(), RuntimeError> {
    let Some((current, attributes)) = interpreter.own_descriptor(target, key) else {
        if descriptor.is_accessor() {
            let pair =
                interpreter.accessor_pair(half(descriptor.getter), half(descriptor.setter))?;
            let attributes = Attributes::accessor(
                descriptor.enumerable.unwrap_or(false),
                descriptor.configurable.unwrap_or(false),
            );
            return interpreter.define_property(target, key, pair, attributes);
        }
        let value = descriptor.value.unwrap_or(Value::UNDEFINED);
        let attributes = Attributes::new(
            descriptor.writable.unwrap_or(false),
            descriptor.enumerable.unwrap_or(false),
            descriptor.configurable.unwrap_or(false),
        );
        return interpreter.define_property(target, key, value, attributes);
    };
    if !attributes.is_configurable() {
        let asks_for_more = descriptor.configurable == Some(true)
            || descriptor
                .enumerable
                .is_some_and(|wanted| wanted != attributes.is_enumerable())
            || changes_kind(descriptor, attributes)
            || frozen_accessor_moves(interpreter, descriptor, current, attributes)
            || (!attributes.is_accessor()
                && !attributes.is_writable()
                && (descriptor.writable == Some(true)
                    || descriptor
                        .value
                        .is_some_and(|wanted| !interpreter.same_value(wanted, current))));
        if asks_for_more {
            return Err(RuntimeError::Type(format!(
                "Cannot redefine property: {key}"
            )));
        }
    }
    if descriptor.is_accessor() || (attributes.is_accessor() && descriptor.value.is_none()) {
        return redefine_accessor(interpreter, target, key, descriptor, current, attributes);
    }
    let value = descriptor.value.unwrap_or(current);
    let attributes = Attributes::new(
        descriptor.writable.unwrap_or(attributes.is_writable()),
        descriptor.enumerable.unwrap_or(attributes.is_enumerable()),
        descriptor
            .configurable
            .unwrap_or(attributes.is_configurable()),
    );
    interpreter.define_property(target, key, value, attributes)
}

/// Whether this descriptor would turn a data property into an accessor or the other way round.
///
/// A descriptor that mentions neither kind is not asking for either, which is what makes
/// `Object.defineProperty(o, 'x', {enumerable: false})` legal against an accessor. Mentioning
/// `writable` counts as asking for a data property even with no `value`, because a data property is
/// the only kind that has one.
fn changes_kind(descriptor: Descriptor, attributes: Attributes) -> bool {
    let wants_data = descriptor.value.is_some() || descriptor.writable.is_some();
    (descriptor.is_accessor() && !attributes.is_accessor())
        || (wants_data && attributes.is_accessor())
}

/// Whether this descriptor would move either half of a non configurable accessor.
///
/// An accessor has no writable flag, so a non configurable one is frozen outright and redefining
/// either half is refused. Writing the same function back is allowed, the way writing the same value
/// back to a non writable data property is, which is the same `SameValue` rule reaching the same
/// place from the other kind of property.
fn frozen_accessor_moves(
    interpreter: &Interpreter,
    descriptor: Descriptor,
    current: Value,
    attributes: Attributes,
) -> bool {
    if !attributes.is_accessor() || !descriptor.is_accessor() {
        return false;
    }
    let (getter, setter) = interpreter.accessor_halves(current);
    let moved = |wanted: Option<Value>, have: Option<Value>| {
        wanted
            .is_some_and(|wanted| !interpreter.same_value(wanted, have.unwrap_or(Value::UNDEFINED)))
    };
    moved(descriptor.getter, getter) || moved(descriptor.setter, setter)
}

/// Store an accessor, keeping whatever halves and flags the descriptor did not mention.
///
/// Keeping the halves is what lets a getter and a setter be defined in two separate calls, which was
/// measured against node rather than reasoned about: the second call mentions only `set` and the
/// getter from the first call is still there afterwards. A property that was a data property has no
/// halves to keep, and its value is dropped rather than carried, because it has just stopped being
/// the kind of property that has one.
fn redefine_accessor(
    interpreter: &mut Interpreter,
    target: Value,
    key: &str,
    descriptor: Descriptor,
    current: Value,
    attributes: Attributes,
) -> Result<(), RuntimeError> {
    let (getter, setter) = if attributes.is_accessor() {
        interpreter.accessor_halves(current)
    } else {
        (None, None)
    };
    let getter = if descriptor.getter.is_some() {
        half(descriptor.getter)
    } else {
        getter
    };
    let setter = if descriptor.setter.is_some() {
        half(descriptor.setter)
    } else {
        setter
    };
    let pair = interpreter.accessor_pair(getter, setter)?;
    let attributes = Attributes::accessor(
        descriptor.enumerable.unwrap_or(attributes.is_enumerable()),
        descriptor
            .configurable
            .unwrap_or(attributes.is_configurable()),
    );
    interpreter.define_property(target, key, pair, attributes)
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

    /// What a program failed with, for a program that should fail.
    #[track_caller]
    fn refused(source: &str) -> String {
        printed(source).expect_err("the program should fail")
    }

    #[test]
    fn every_object_has_the_methods_on_object_prototype() {
        assert_eq!(
            logged(
                "var o = {a: 1}; console.log(o.toString(), o.valueOf() === o, typeof o.hasOwnProperty);"
            ),
            "[object Object] true function"
        );
    }

    #[test]
    fn has_own_property_asks_about_the_object_and_not_about_its_chain() {
        assert_eq!(
            logged(
                "var p = {a: 1}; var o = Object.create(p);\n\
                 console.log(o.a, o.hasOwnProperty('a'), p.hasOwnProperty('a'), o.hasOwnProperty('toString'));"
            ),
            "1 false true false"
        );
    }

    #[test]
    fn has_own_property_converts_its_key_the_way_a_property_name_is_converted() {
        assert_eq!(
            logged(
                "console.log(({undefined: 1}).hasOwnProperty(undefined), ({}).hasOwnProperty());"
            ),
            "true false"
        );
    }

    #[test]
    fn is_prototype_of_walks_the_whole_chain_and_leaves_the_object_itself_out() {
        assert_eq!(
            logged(
                "var top = {}; var middle = Object.create(top); var bottom = Object.create(middle);\n\
                 console.log(top.isPrototypeOf(bottom), bottom.isPrototypeOf(top), top.isPrototypeOf(top));"
            ),
            "true false false"
        );
    }

    #[test]
    fn is_prototype_of_answers_false_for_something_that_has_no_chain_rather_than_throwing() {
        assert_eq!(
            logged(
                "console.log(Object.prototype.isPrototypeOf(1), Object.prototype.isPrototypeOf(Object.create(null)));"
            ),
            "false false"
        );
    }

    #[test]
    fn a_hidden_property_is_still_an_own_property() {
        // Where the two questions come apart. `hasOwnProperty` is about whether the property is
        // there and `propertyIsEnumerable` is about whether it shows, and a defined property is
        // there and does not show.
        assert_eq!(
            logged(
                "var o = {shown: 1}; Object.defineProperty(o, 'kept', {value: 2});\n\
                 console.log(o.hasOwnProperty('kept'), o.propertyIsEnumerable('kept'), o.propertyIsEnumerable('shown'));"
            ),
            "true false true"
        );
    }

    #[test]
    fn property_is_enumerable_says_false_for_a_name_that_is_not_there_at_all() {
        assert_eq!(
            logged(
                "console.log(({}).propertyIsEnumerable('nope'), ({}).propertyIsEnumerable('toString'));"
            ),
            "false false"
        );
    }

    #[test]
    fn the_methods_on_object_prototype_are_hidden_from_everything_that_walks_an_object() {
        // The reason they could not be installed before attributes. An enumerable `toString` at the
        // top of every chain would appear in every object printed and everything serialised.
        assert_eq!(
            logged("var o = {a: 1}; console.log(o, JSON.stringify(o));"),
            "{ a: 1 } {\"a\":1}"
        );
    }

    #[test]
    fn the_methods_on_object_prototype_carry_the_attributes_node_reports_for_them() {
        assert_eq!(
            logged(
                "var d = Object.getOwnPropertyDescriptor(Object.prototype, 'hasOwnProperty');\n\
                 console.log(d.writable, d.enumerable, d.configurable);"
            ),
            "true false true"
        );
    }

    #[test]
    fn an_object_with_no_prototype_has_none_of_them() {
        assert_eq!(
            logged(
                "var bare = Object.create(null); console.log(bare.toString, bare.hasOwnProperty);"
            ),
            "undefined undefined"
        );
    }

    #[test]
    fn a_method_on_object_prototype_called_on_nothing_refuses_by_name() {
        // A plain call needs `globalThis` to know what it was called on, and there is not one. The
        // only way to make this call today is to read the method out and call it, which is what the
        // program below does.
        let error = refused("var f = ({}).hasOwnProperty; f('x');");
        assert!(error.contains("globalThis"), "{error}");
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
    fn the_descriptors_argument_defines_properties_on_the_object_that_was_made() {
        assert_eq!(
            logged(
                "var o = Object.create(null, {x: {value: 1}}); console.log(o, o.x, Object.getPrototypeOf(o));"
            ),
            "[Object: null prototype] {} 1 null"
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
    fn every_function_in_the_realm_answers_with_one_function_prototype() {
        // Node says true to both of these, and the second is the one that says `Object` is a
        // function rather than a namespace object with the same names on it.
        assert_eq!(
            logged(
                "function f() {} function g() {}\n\
                 console.log(Object.getPrototypeOf(f) === Object.getPrototypeOf(g));\n\
                 console.log(Object.getPrototypeOf(Object) === Object.getPrototypeOf(f));"
            ),
            "true\ntrue"
        );
    }

    #[test]
    fn a_defined_property_gets_nothing_it_was_not_asked_for() {
        // The asymmetry against assignment. `o.a = 1` makes a property that can do all three things
        // and this makes one that can do none of them.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'a', {value: 1}); console.log(Object.getOwnPropertyDescriptor(o, 'a'));"
            ),
            "{ value: 1, writable: false, enumerable: false, configurable: false }"
        );
    }

    #[test]
    fn an_assigned_property_gets_all_three() {
        assert_eq!(
            logged("var o = {a: 1}; console.log(Object.getOwnPropertyDescriptor(o, 'a'));"),
            "{ value: 1, writable: true, enumerable: true, configurable: true }"
        );
    }

    #[test]
    fn a_hidden_property_is_still_there_and_is_not_printed_or_serialised() {
        // The three places enumerability shows, all of which a method on a prototype has to be
        // invisible to before it can be installed.
        assert_eq!(
            logged(
                "var o = {x: 1}; Object.defineProperty(o, 'x', {enumerable: false}); console.log(o, JSON.stringify(o), o.x);"
            ),
            "{} {} 1"
        );
    }

    #[test]
    fn defining_answers_with_the_object_so_it_can_be_used_in_place() {
        assert_eq!(
            logged("console.log(Object.defineProperty({}, 'z', {value: 5}).z);"),
            "5"
        );
    }

    #[test]
    fn a_read_only_property_refuses_a_write_in_strict_mode_and_ignores_one_otherwise() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'a', {value: 1}); o.a = 2; console.log(o.a);"
            ),
            "1"
        );
        let error = printed(
            "'use strict'; var o = {}; Object.defineProperty(o, 'a', {value: 1}); o.a = 2;",
        )
        .expect_err("should throw");
        assert!(
            error.contains("Cannot assign to read only property 'a' of object '#<Object>'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_computed_key_gets_the_same_refusal_a_dotted_one_does() {
        // From the point where there is a name there is nothing left that says how the program
        // spelled it, so `o['a']` has to say exactly what `o.a` says.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'a', {value: 1}); o['a'] = 2; console.log(o['a']);"
            ),
            "1"
        );
        let error = printed(
            "'use strict'; var o = {}; Object.defineProperty(o, 'a', {value: 1}); o['a'] = 2;",
        )
        .expect_err("should throw");
        assert!(
            error.contains("Cannot assign to read only property 'a' of object '#<Object>'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_computed_key_finds_what_the_prototype_chain_holds() {
        assert_eq!(
            logged(
                "var p = {a: 'above'}; var o = Object.create(p); console.log(o['a']); o['a'] = 'own'; console.log(o.a, p.a);"
            ),
            "above\nown above"
        );
    }

    #[test]
    fn a_key_is_named_in_the_message_only_when_naming_it_runs_nothing() {
        // Node's rule, measured rather than assumed, and narrower than the obvious guess. The name
        // comes from `constructor` and is offered only when the `toString` the object reaches is the
        // one on `Object.prototype`, so anything that would have called the program's own code is
        // left out of the sentence rather than run to build it.
        for (source, message) in [
            (
                "var u; u[{}];",
                "Cannot read properties of undefined (reading '#<Object>')",
            ),
            (
                "function Weird() {} var u; u[new Weird()];",
                "Cannot read properties of undefined (reading '#<Weird>')",
            ),
            (
                "var u; u[Object.create({constructor: function Weird() {}})];",
                "Cannot read properties of undefined (reading '#<Weird>')",
            ),
            (
                "var u; u[Object.create(null)];",
                "Cannot read properties of undefined",
            ),
            (
                "var u; u[{toString: function () { throw new Error('should not run'); }}];",
                "Cannot read properties of undefined",
            ),
            (
                "var u; u[Object.create(Object.prototype, {constructor: {value: 1}})];",
                "Cannot read properties of undefined",
            ),
        ] {
            let error = printed(source).expect_err("should throw");
            assert!(error.contains(message), "{source} said {error}");
        }
    }

    #[test]
    fn a_read_only_property_on_a_prototype_stops_a_write_to_everything_below_it() {
        // The part that is easy to get wrong. The object being written to does not have the
        // property at all, and the answer still comes from the chain.
        assert_eq!(
            logged(
                "var p = {}; Object.defineProperty(p, 'a', {value: 1}); var o = Object.create(p); o.a = 2; console.log(o.a, Object.getOwnPropertyDescriptor(o, 'a'));"
            ),
            "1 undefined"
        );
    }

    #[test]
    fn an_object_with_no_prototype_is_named_differently_in_the_same_refusal() {
        let error = printed(
            "'use strict'; var o = Object.create(null); Object.defineProperty(o, 'r', {value: 1}); o.r = 2;",
        )
        .expect_err("should throw");
        assert!(
            error.contains("Cannot assign to read only property 'r' of object '[object Object]'"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_configurable_property_can_be_redefined_into_anything() {
        assert_eq!(
            logged(
                "var o = {a: 1}; Object.defineProperty(o, 'a', {value: 2, writable: false, enumerable: false, configurable: false}); console.log(Object.getOwnPropertyDescriptor(o, 'a'));"
            ),
            "{ value: 2, writable: false, enumerable: false, configurable: false }"
        );
    }

    #[test]
    fn a_non_configurable_property_can_only_ever_become_less_permissive() {
        for source in [
            "Object.defineProperty(o, 'a', {configurable: true});",
            "Object.defineProperty(o, 'a', {enumerable: true});",
            "Object.defineProperty(o, 'a', {writable: true});",
            "Object.defineProperty(o, 'a', {value: 2});",
        ] {
            let program =
                format!("var o = {{}}; Object.defineProperty(o, 'a', {{value: 1}}); {source}");
            let error = printed(&program).expect_err("should throw");
            assert!(
                error.contains("Cannot redefine property: a"),
                "unexpected error for {source}: {error}"
            );
        }
    }

    #[test]
    fn a_redefinition_that_changes_nothing_is_allowed_however_locked_down_it_is() {
        // Every one of these asks for exactly what is already true, and asking for what is already
        // true is not a change. An empty descriptor asks for nothing at all.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'a', {value: 1}); Object.defineProperty(o, 'a', {}); Object.defineProperty(o, 'a', {value: 1}); Object.defineProperty(o, 'a', {enumerable: false, writable: false, configurable: false}); console.log(o.a);"
            ),
            "1"
        );
    }

    #[test]
    fn writing_the_same_value_back_is_the_same_value_and_not_strict_equality() {
        // `NaN === NaN` is false and `SameValue(NaN, NaN)` is true, so a non writable `NaN` can be
        // redefined to `NaN`. Negative zero is the case that goes the other way.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'a', {value: NaN}); Object.defineProperty(o, 'a', {value: NaN}); console.log(o.a);"
            ),
            "NaN"
        );
        let error = printed(
            "var o = {}; Object.defineProperty(o, 'a', {value: 0}); Object.defineProperty(o, 'a', {value: -0});",
        )
        .expect_err("should throw");
        assert!(
            error.contains("Cannot redefine property: a"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_writable_property_that_cannot_be_configured_can_still_change_its_value() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'w', {value: 1, writable: true}); Object.defineProperty(o, 'w', {value: 2}); console.log(o.w);"
            ),
            "2"
        );
    }

    #[test]
    fn a_flag_is_converted_rather_than_having_to_be_a_boolean() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'k', {value: 1, writable: 0, enumerable: 'yes'}); console.log(Object.getOwnPropertyDescriptor(o, 'k'));"
            ),
            "{ value: 1, writable: false, enumerable: true, configurable: false }"
        );
    }

    #[test]
    fn a_descriptor_is_read_through_its_prototype_chain_like_any_other_object() {
        assert_eq!(
            logged("console.log(Object.defineProperty({}, 'x', Object.create({value: 7})).x);"),
            "7"
        );
    }

    #[test]
    fn defining_on_something_that_is_not_an_object_names_the_builtin_that_refused() {
        let error = printed("Object.defineProperty(1, 'x', {});").expect_err("should throw");
        assert!(
            error.contains("Object.defineProperty called on non-object"),
            "unexpected error: {error}"
        );
        let error = printed("Object.defineProperties(1, {});").expect_err("should throw");
        assert!(
            error.contains("Object.defineProperties called on non-object"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_descriptor_that_is_not_an_object_says_what_was_passed_instead() {
        let error = printed("Object.defineProperty({}, 'x', 1);").expect_err("should throw");
        assert!(
            error.contains("Property description must be an object: 1"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_descriptor_with_an_accessor_and_a_value_is_a_type_error_and_not_a_gap() {
        // This one does not need receivers to be answered correctly, so it is answered rather than
        // refused, and in the words node uses.
        let error = printed("Object.defineProperty({}, 'x', {get: function () {}, value: 1});")
            .expect_err("should throw");
        assert!(
            error.contains(
                "Cannot both specify accessors and a value or writable attribute, #<Object>"
            ),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_getter_is_defined_and_called_by_reading_the_property() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 41 + 1; }}); console.log(o.x);"
            ),
            "42"
        );
    }

    #[test]
    fn a_setter_is_defined_and_called_by_writing_the_property() {
        // The value written reaches the setter and the setter's `this` is the object, which is the
        // whole reason a setter is worth having over a plain property.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {set: function (v) { this.seen = v * 2; }}); o.x = 21; console.log(o.seen);"
            ),
            "42"
        );
    }

    #[test]
    fn an_accessor_prints_as_what_it_is_rather_than_as_what_it_would_say() {
        // Node's exact output, measured. Printing an object does not run the program's code.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'g', {get: function () { return 1; }, enumerable: true}); Object.defineProperty(o, 's', {set: function (v) {}, enumerable: true}); Object.defineProperty(o, 'b', {get: function () { return 1; }, set: function (v) {}, enumerable: true}); console.log(o);"
            ),
            "{ g: [Getter], s: [Setter], b: [Getter/Setter] }"
        );
    }

    #[test]
    fn a_descriptor_reports_the_two_halves_instead_of_a_value() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 1; }, enumerable: true, configurable: true}); var d = Object.getOwnPropertyDescriptor(o, 'x'); console.log(typeof d.get, typeof d.set, d.enumerable, d.configurable);"
            ),
            "function undefined true true"
        );
    }

    #[test]
    fn a_half_the_descriptor_does_not_mention_is_kept() {
        // Two calls, one naming the getter and one naming the setter, leave a property with both.
        // Measured against node rather than reasoned about.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 1; }, configurable: true, enumerable: true}); Object.defineProperty(o, 'x', {set: function (v) {}}); var d = Object.getOwnPropertyDescriptor(o, 'x'); console.log(typeof d.get, typeof d.set, d.enumerable, d.configurable);"
            ),
            "function function true true"
        );
    }

    #[test]
    fn a_getter_with_no_setter_is_silent_in_sloppy_mode_and_throws_in_strict_mode() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 7; }}); o.x = 9; console.log(o.x);"
            ),
            "7"
        );
        assert_eq!(
            logged(
                "'use strict'; var o = {}; Object.defineProperty(o, 'x', {get: function () { return 7; }}); try { o.x = 9; } catch (e) { console.log(e.message); }"
            ),
            "Cannot set property x of #<Object> which has only a getter"
        );
    }

    #[test]
    fn a_setter_on_a_prototype_receives_the_writes_of_everything_below_it() {
        // The mechanism that makes one accessor on a prototype answer for every instance. The write
        // goes to the setter rather than adding an own property, and `this` inside it is the object
        // that was written to and not the prototype the setter was found on.
        assert_eq!(
            logged(
                "var proto = {}; Object.defineProperty(proto, 'p', {set: function (v) { this.stored = v; }, get: function () { return this.stored; }}); var child = Object.create(proto); child.p = 42; console.log(child.p, child);"
            ),
            "42 { stored: 42 }"
        );
    }

    #[test]
    fn a_half_that_is_not_a_function_is_refused_by_the_half_it_is() {
        let error = printed("Object.defineProperty({}, 'x', {get: 5});").expect_err("should throw");
        assert!(
            error.contains("Getter must be a function: 5"),
            "unexpected error: {error}"
        );
        let error =
            printed("Object.defineProperty({}, 'x', {set: 'no'});").expect_err("should throw");
        assert!(
            error.contains("Setter must be a function: no"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_non_configurable_accessor_cannot_have_either_half_moved() {
        let error = printed(
            "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 1; }}); Object.defineProperty(o, 'x', {get: function () { return 2; }});",
        )
        .expect_err("should throw");
        assert!(
            error.contains("Cannot redefine property: x"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_configurable_accessor_can_become_a_data_property_and_back() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 1; }, configurable: true}); Object.defineProperty(o, 'x', {value: 9}); var d = Object.getOwnPropertyDescriptor(o, 'x'); console.log(d.value, d.writable, d.enumerable, d.configurable);"
            ),
            "9 false false true"
        );
    }

    #[test]
    fn stringifying_an_accessor_refuses_by_name_because_it_would_have_to_call_it() {
        let error = printed(
            "var o = {}; Object.defineProperty(o, 'x', {get: function () { return 1; }, enumerable: true}); JSON.stringify(o);",
        )
        .expect_err("should throw");
        assert!(
            error.contains("means calling a getter"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn every_descriptor_is_read_before_any_of_them_is_applied() {
        // Measured against node rather than assumed. A loop that reads and applies one at a time is
        // the obvious way to write this and it leaves the object half changed.
        assert_eq!(
            logged(
                "var o = {}; try { Object.defineProperties(o, {a: {value: 1}, b: 2}); } catch (e) { console.log(e.message); } console.log(o.a);"
            ),
            "Property description must be an object: 2\nundefined"
        );
    }

    #[test]
    fn defining_many_properties_at_once_applies_all_of_them() {
        assert_eq!(
            logged(
                "var o = Object.defineProperties({}, {a: {value: 1, enumerable: true}, b: {value: 2}}); console.log(o, o.b);"
            ),
            "{ a: 1 } 2"
        );
    }

    #[test]
    fn describing_a_name_the_object_does_not_have_of_its_own_is_undefined() {
        assert_eq!(
            logged(
                "var o = Object.create({up: 1}); console.log(Object.getOwnPropertyDescriptor(o, 'up'), Object.getOwnPropertyDescriptor(o, 'nope'));"
            ),
            "undefined undefined"
        );
    }

    #[test]
    fn describing_undefined_or_null_throws_and_describing_a_number_does_not() {
        let error =
            printed("Object.getOwnPropertyDescriptor(null, 'x');").expect_err("should throw");
        assert!(
            error.contains("Cannot convert undefined or null to object"),
            "unexpected error: {error}"
        );
        // A number boxes into a wrapper that never has own properties, so `undefined` is the honest
        // answer as well as node's.
        assert_eq!(
            logged("console.log(Object.getOwnPropertyDescriptor(1, 'x'));"),
            "undefined"
        );
    }

    #[test]
    fn describing_a_string_refuses_rather_than_answering_undefined() {
        // A string wrapper really does have `length` and one property per character, so `undefined`
        // would be a wrong answer rather than a missing one.
        let error =
            printed("Object.getOwnPropertyDescriptor('ab', 'length');").expect_err("should throw");
        assert!(
            error.contains("it needs the wrapper prototypes"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_statics_on_object_are_hidden_the_way_every_namespace_object_is() {
        // `Object` is a function and prints as one now, and neither it nor `JSON` shows a single one
        // of the names on it, because every static is non enumerable. Node writes `[Function:
        // Object] Object [JSON] {}`, where the tag in front of the empty braces comes from
        // `Symbol.toStringTag` and there are no symbols yet.
        assert_eq!(
            logged("console.log(Object, JSON);"),
            "[Function: Object] {}"
        );
    }

    #[test]
    fn object_prototype_cannot_be_moved_out_from_under_running_code() {
        assert_eq!(
            logged(
                "console.log(Object.getOwnPropertyDescriptor(Object, 'prototype').writable, Object.getOwnPropertyDescriptor(Object, 'prototype').configurable);"
            ),
            "false false"
        );
    }

    #[test]
    fn object_is_a_function_and_says_so() {
        // The whole point of the change. It used to answer "object" here and "function" in node.
        assert_eq!(
            logged("console.log(typeof Object, typeof Object.create, Object);"),
            "function function [Function: Object]"
        );
    }

    #[test]
    fn every_object_in_the_realm_knows_the_constructor_that_would_have_made_it() {
        assert_eq!(
            logged(
                "console.log(({}).constructor === Object, Object.prototype.constructor === Object, Object.create(null).constructor);"
            ),
            "true true undefined"
        );
    }

    #[test]
    fn constructor_is_hidden_the_way_node_hides_it() {
        // Node breaks this one over five lines too, because the single line would be too long, and
        // the four flags read the same either way.
        assert_eq!(
            logged(
                "console.log(Object.getOwnPropertyDescriptor(Object.prototype, 'constructor'));"
            ),
            "{\n  value: [Function: Object],\n  writable: true,\n  enumerable: false,\n  configurable: true\n}"
        );
    }

    #[test]
    fn calling_object_makes_something_out_of_nothing_and_hands_everything_else_back() {
        assert_eq!(
            logged(
                "var o = {}; var f = function () {};\n\
                 console.log(typeof Object(), Object(undefined).a, Object(null) !== o, Object(o) === o, Object(f) === f);"
            ),
            "object undefined true true true"
        );
    }

    #[test]
    fn calling_object_on_a_primitive_refuses_rather_than_handing_the_primitive_back() {
        // Node answers with a `Number` wrapper here. Handing back the number itself would be a wrong
        // answer that a program could not tell from the right one until it tried to add a property.
        let error = printed("Object(1);").expect_err("should throw");
        assert!(
            error.contains("boxing a primitive needs the wrapper prototypes"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_function_answers_the_same_questions_about_its_own_properties_that_an_object_does() {
        assert_eq!(
            logged(
                "function Foo() {} Foo.bar = 1;\n\
                 console.log(Foo.hasOwnProperty('bar'), Foo.hasOwnProperty('prototype'), Foo.hasOwnProperty('nope'), Foo.propertyIsEnumerable('bar'), Foo.propertyIsEnumerable('prototype'));"
            ),
            "true true false true false"
        );
    }

    #[test]
    fn a_static_can_be_defined_on_a_function_the_way_the_standard_library_defines_them() {
        assert_eq!(
            logged(
                "function Foo() {} Object.defineProperty(Foo, 'hidden', {value: 2});\n\
                 console.log(Foo.hidden, Object.getOwnPropertyDescriptor(Foo, 'hidden'), Foo.propertyIsEnumerable('hidden'));"
            ),
            "2 { value: 2, writable: false, enumerable: false, configurable: false } false"
        );
    }

    #[test]
    fn describing_a_function_answers_for_what_it_carries_and_for_what_it_was_born_with() {
        // Every one of these was measured against node. A written static is an ordinary property, and
        // `prototype` is the one a function is born with and cannot lose.
        assert_eq!(
            logged(
                "function Foo() {} Foo.bar = 1;\n\
                 console.log(Object.getOwnPropertyDescriptor(Foo, 'bar'), Object.getOwnPropertyDescriptor(Foo, 'prototype'), Object.getOwnPropertyDescriptor(Foo, 'nope'));"
            ),
            "{ value: 1, writable: true, enumerable: true, configurable: true } { value: {}, writable: true, enumerable: false, configurable: false } undefined"
        );
    }

    #[test]
    fn one_function_prototype_sits_above_every_function_including_object() {
        assert_eq!(
            logged(
                "function a() {} function b() {}\n\
                 console.log(Object.getPrototypeOf(a) === Object.getPrototypeOf(b), Object.getPrototypeOf(Object) === Object.getPrototypeOf(a), Object.getPrototypeOf(Object.getPrototypeOf(a)) === Object.prototype);"
            ),
            "true true true"
        );
    }

    #[test]
    fn inheriting_from_a_function_refuses_rather_than_inheriting_from_its_properties() {
        let error = printed("Object.create(function () {});").expect_err("should throw");
        assert!(
            error.contains("a prototype link can only point at an ordinary object"),
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

    #[test]
    fn an_index_found_on_a_prototype_is_reached_from_below() {
        // The read that goes through the element storage misses, and the walk that follows has to
        // ask each level for its elements as well as for its names. A walk that only asks for names
        // answers undefined here, which is what this build did before the chain learned about them.
        assert_eq!(
            logged(
                "var p = {}; p[0] = 'proto'; var c = Object.create(p); console.log(c[0], c['0']);"
            ),
            "proto proto"
        );
    }

    #[test]
    fn an_index_written_below_shadows_the_one_above_and_leaves_it_alone() {
        assert_eq!(
            logged(
                "var p = {}; p[0] = 'proto'; var c = Object.create(p); c[0] = 'own'; console.log(c[0], p[0]);"
            ),
            "own proto"
        );
    }

    #[test]
    fn an_index_is_serialised_by_json_the_way_a_name_is() {
        assert_eq!(
            logged("var o = {}; o[0] = 1; console.log(JSON.stringify(o));"),
            "{\"0\":1}"
        );
    }

    #[test]
    fn the_indices_come_before_the_names_whatever_order_they_arrived_in() {
        // Measured against Node. This is the language's enumeration order and not a choice, and it
        // falls out of the storage for free: the elements are a flat array so they are already
        // ascending, and the names are a shape chain so they are already in insertion order.
        assert_eq!(
            logged(
                "var o = {}; o.x = 1; o[2] = 2; o[0] = 3; o.a = 4; console.log(JSON.stringify(o));"
            ),
            "{\"0\":3,\"2\":2,\"x\":1,\"a\":4}"
        );
    }

    #[test]
    fn an_index_has_the_descriptor_an_assignment_makes() {
        assert_eq!(
            logged("var o = {}; o[0] = 1; console.log(Object.getOwnPropertyDescriptor(o, '0'));"),
            "{ value: 1, writable: true, enumerable: true, configurable: true }"
        );
    }

    #[test]
    fn the_methods_that_ask_about_own_properties_all_see_an_index() {
        assert_eq!(
            logged(
                "var o = {}; o[0] = 1; console.log(o.hasOwnProperty('0'), o.hasOwnProperty(0), o.hasOwnProperty('00'), o.propertyIsEnumerable('0'));"
            ),
            "true true false true"
        );
    }

    #[test]
    fn an_index_too_sparse_for_an_array_is_still_one_property_once_the_array_grows_past_it() {
        // The first write is too far past the end to be worth an array, so it is stored as a name.
        // Filling in everything below it then grows the array out past that index, and the write
        // that follows has to find the name and stay with it. Storing it in the array instead would
        // leave the same property in two places, and which one answered would depend on the reader.
        assert_eq!(
            logged(
                "var o = {}; o[3000] = 'named'; for (var i = 0; i < 3000; i++) o[i] = i; o[3000] = 'again'; console.log(o[3000], o['3000']);"
            ),
            "again again"
        );
    }

    #[test]
    fn defining_an_index_with_the_default_flags_puts_it_where_an_assignment_would() {
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, '0', {value: 7, writable: true, enumerable: true, configurable: true}); console.log(o[0], JSON.stringify(o));"
            ),
            "7 {\"0\":7}"
        );
    }

    #[test]
    fn defining_an_index_with_anything_else_puts_it_under_the_name_and_leaves_it_there() {
        // The element storage holds values and not flags, so a hidden or a read only index has to be
        // an ordinary property. What matters is that it is only one property afterwards: the slot it
        // would have used is marked, so the assignment below finds the name rather than writing an
        // element beside it and leaving the object answering two ways.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, '0', {value: 7}); console.log(o[0], JSON.stringify(o), Object.getOwnPropertyDescriptor(o, '0'));"
            ),
            "7 {} { value: 7, writable: false, enumerable: false, configurable: false }"
        );
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, '0', {value: 7, writable: true}); o[0] = 9; console.log(o[0], o['0'], JSON.stringify(o));"
            ),
            "9 9 {}"
        );
    }

    #[test]
    fn an_index_that_started_as_a_name_stays_one_after_the_array_grows_over_it() {
        // The element storage is asked to cover index 8 by the loop, and index 3 is already a name
        // by then. Without the mark the write would land in the array and the object would hold `3`
        // twice, answering `9` to one reader and `7` to another.
        assert_eq!(
            logged(
                "var o = {}; Object.defineProperty(o, '3', {value: 7, writable: true}); for (var i = 0; i < 9; i++) if (i !== 3) o[i] = i; o[3] = 9; console.log(o[3], o['3'], JSON.stringify(o));"
            ),
            "9 9 {\"0\":0,\"1\":1,\"2\":2,\"4\":4,\"5\":5,\"6\":6,\"7\":7,\"8\":8}"
        );
    }

    #[test]
    fn negative_zero_is_index_zero_because_that_is_how_it_spells() {
        assert_eq!(
            logged("var o = {}; o[0] = 'zero'; console.log(o[-0], o[0.0], o['0']);"),
            "zero zero zero"
        );
    }

    #[test]
    fn only_the_canonical_spelling_of_a_number_is_an_index() {
        assert_eq!(
            logged(
                "var o = {}; o[1] = 'a'; console.log(o[1.0], o['1'], o['01'], o['+1'], o[' 1']);"
            ),
            "a a undefined undefined undefined"
        );
    }
}
