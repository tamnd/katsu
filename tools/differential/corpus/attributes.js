// Property attributes and Object.defineProperty. What a property is allowed to do, and the rules
// for changing it, both of which have to agree with node exactly rather than approximately.

// A defined property gets nothing it was not asked for, and an assigned one gets all three.
var defined = {};
Object.defineProperty(defined, 'a', { value: 1 });
console.log(Object.getOwnPropertyDescriptor(defined, 'a'));
console.log(Object.getOwnPropertyDescriptor({ a: 1 }, 'a'));

// The three places a hidden property does not show, which is what lets a prototype carry a method.
var hidden = { x: 1, y: 2 };
Object.defineProperty(hidden, 'x', { enumerable: false });
console.log(hidden, JSON.stringify(hidden), hidden.x, hidden.y);

// Defining answers with the object, so it can be used where the object would be.
console.log(Object.defineProperty({}, 'z', { value: 5 }).z);

// A read only property ignores a write in sloppy mode and refuses one in strict mode.
var locked = {};
Object.defineProperty(locked, 'a', { value: 1 });
locked.a = 2;
console.log(locked.a);
try {
  (function () {
    'use strict';
    locked.a = 3;
  })();
} catch (error) {
  console.log(error.name + ': ' + error.message);
}

// The chain decides, not the object being written to. This object does not have the property.
var above = {};
Object.defineProperty(above, 'a', { value: 1 });
var below = Object.create(above);
below.a = 2;
console.log(below.a, Object.getOwnPropertyDescriptor(below, 'a'));

// An object with no prototype is named differently in the same message.
try {
  (function () {
    'use strict';
    var bare = Object.create(null);
    Object.defineProperty(bare, 'r', { value: 1 });
    bare.r = 2;
  })();
} catch (error) {
  console.log(error.message);
}

// Configurable means the property can be redefined into anything at all.
var loose = { a: 1 };
Object.defineProperty(loose, 'a', {
  value: 2,
  writable: false,
  enumerable: false,
  configurable: false,
});
console.log(Object.getOwnPropertyDescriptor(loose, 'a'));

// And once it is not configurable, every direction that asks for more is refused.
var tight = {};
Object.defineProperty(tight, 'a', { value: 1 });
try {
  Object.defineProperty(tight, 'a', { configurable: true });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(tight, 'a', { enumerable: true });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(tight, 'a', { writable: true });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(tight, 'a', { value: 2 });
} catch (error) {
  console.log(error.message);
}

// Asking for what is already true is not a change, however locked down the property is.
Object.defineProperty(tight, 'a', {});
Object.defineProperty(tight, 'a', { value: 1 });
Object.defineProperty(tight, 'a', { writable: false, enumerable: false, configurable: false });
console.log(tight.a);

// The comparison is SameValue and not strict equality, which NaN and negative zero both show.
var same = {};
Object.defineProperty(same, 'n', { value: NaN });
Object.defineProperty(same, 'n', { value: NaN });
console.log(same.n);
Object.defineProperty(same, 'z', { value: 0 });
try {
  Object.defineProperty(same, 'z', { value: -0 });
} catch (error) {
  console.log(error.message);
}

// Writable but not configurable is the one case where the value can still change.
var writable = {};
Object.defineProperty(writable, 'w', { value: 1, writable: true });
Object.defineProperty(writable, 'w', { value: 2 });
console.log(writable.w);

// A flag is converted rather than having to be a boolean, and a descriptor is read through its own
// prototype chain like any other object.
var converted = {};
Object.defineProperty(converted, 'k', { value: 1, writable: 0, enumerable: 'yes' });
console.log(Object.getOwnPropertyDescriptor(converted, 'k'));
console.log(Object.defineProperty({}, 'x', Object.create({ value: 7 })).x);

// The refusals, each of which names the builtin that refused or the value that was wrong.
try {
  Object.defineProperty(1, 'x', {});
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperties(1, {});
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty({}, 'x', 1);
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty({}, 'x', { get: function () {}, value: 1 });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperties({}, null);
} catch (error) {
  console.log(error.message);
}
try {
  Object.getOwnPropertyDescriptor(null, 'x');
} catch (error) {
  console.log(error.message);
}

// Every descriptor is read before any of them is applied, so a bad one later leaves nothing behind.
var partial = {};
try {
  Object.defineProperties(partial, { a: { value: 1 }, b: 2 });
} catch (error) {
  console.log(error.message);
}
console.log(partial.a);

// And a good set applies all of them.
var many = Object.defineProperties({}, { a: { value: 1, enumerable: true }, b: { value: 2 } });
console.log(many, many.b);

// Object.create takes the same descriptors, because it is the same operation.
var created = Object.create(null, { x: { value: 1, enumerable: true } });
console.log(created, created.x, Object.getPrototypeOf(created));

// Describing asks about the object and not about its chain, and a number has nothing to describe.
var inherits = Object.create({ up: 1 });
console.log(Object.getOwnPropertyDescriptor(inherits, 'up'));
console.log(Object.getOwnPropertyDescriptor(inherits, 'nope'));
console.log(Object.getOwnPropertyDescriptor(1, 'x'));

// The statics on a namespace object are hidden, so JSON serialises as empty rather than as its two
// functions. Object is left out of this line on purpose: it is a function in node and an ordinary
// object here, so JSON.stringify(Object) is undefined there and {} here, and that difference is the
// known typeof gap rather than anything to do with attributes.
console.log(JSON.stringify(JSON));
console.log(Object.getOwnPropertyDescriptor(Object, 'prototype').writable);
console.log(Object.getOwnPropertyDescriptor(Object, 'prototype').configurable);
console.log(Object.getOwnPropertyDescriptor(Object, 'defineProperty').enumerable);
