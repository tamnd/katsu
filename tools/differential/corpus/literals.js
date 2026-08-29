// Object literals. What the notation builds, which is not always what the equivalent sequence of
// assignments would have built, and the accessor syntax that has no assignment equivalent at all.

// A getter written in the literal rather than defined afterwards. Reading calls it and `this` is
// the object, exactly as it would be if `Object.defineProperty` had put it there.
var basic = {
  get x() {
    return 42;
  },
};
console.log(basic.x);

// Both halves under one name. They are two entries in the source and one property in the result,
// which is only visible if the second joins the first rather than replacing it.
var pair = {
  v: 0,
  get value() {
    return this.v * 2;
  },
  set value(n) {
    this.v = n;
  },
};
pair.value = 21;
console.log(pair.value, pair.v);

// The other order, and with a property in between, because nothing says the two halves have to be
// adjacent or that the getter has to come first.
var apart = {
  set both(n) {
    this.n = n;
  },
  middle: 'here',
  get both() {
    return this.n;
  },
};
apart.both = 7;
console.log(apart.both, apart.middle);

// Printing. node names the halves rather than calling them, so this runs no user code.
console.log(basic, pair, apart);
console.log({
  set writeOnly(n) {},
});

// The flags. This is where the notation and `Object.defineProperty` disagree: a literal makes both
// true and a descriptor defaults each to false.
var flags = Object.getOwnPropertyDescriptor(basic, 'x');
console.log(typeof flags.get, flags.set, flags.enumerable, flags.configurable);
console.log(flags.value, flags.writable);

// A half on its own leaves the other half missing rather than filling it in with something.
var readOnly = {
  get r() {
    return 1;
  },
};
var writeOnly = {
  set w(n) {
    this.seen = n;
  },
};
var half = Object.getOwnPropertyDescriptor(writeOnly, 'w');
console.log(half.get, typeof half.set);
// Reading a property that has only a setter is undefined, because there is nothing to call and
// nothing stored either.
console.log(writeOnly.w);
writeOnly.w = 5;
console.log(writeOnly.seen, writeOnly.w);

// Writing to one that has only a getter is quiet outside strict mode. The value goes nowhere.
readOnly.r = 99;
console.log(readOnly.r);

// The last mention of a name wins, and the position is the first mention's. That holds whichever
// kind each mention is, so all four combinations are worth writing down.
console.log({
  get x() {
    return 1;
  },
  y: 2,
  x: 5,
});
console.log({
  x: 5,
  y: 2,
  get x() {
    return 1;
  },
});
console.log({ x: 1, y: 2, x: 3 });
var replaced = {
  get x() {
    return 1;
  },
  x: 5,
};
var after = Object.getOwnPropertyDescriptor(replaced, 'x');
console.log(after.value, after.writable, after.enumerable, after.configurable);

// A string key, which is a name known at compile time just as much as an identifier is.
var quoted = {
  get 'a-b'() {
    return 3;
  },
};
var dashed = Object.getOwnPropertyDescriptor(quoted, 'a-b');
console.log(typeof dashed.get, dashed.enumerable, dashed.configurable);
console.log(quoted);

// Enumeration. An accessor written in a literal is enumerable, so it shows up where a value would.
var listed = {
  a: 1,
  get b() {
    return 2;
  },
  c: 3,
};
console.log(Object.getOwnPropertyDescriptor(listed, 'b').enumerable);
console.log(listed.a, listed.b, listed.c);

// A getter that throws throws out of the read, not out of the definition.
var angry = {
  get boom() {
    throw 'from the getter';
  },
};
try {
  console.log(angry.boom);
} catch (error) {
  console.log('caught', error);
}

// A property in a literal is defined and not assigned. The difference shows when the prototype has
// a setter for the same name: an assignment calls it and takes no property of its own, and a
// literal ignores it completely.
Object.defineProperty(Object.prototype, 'watched', {
  get: function () {
    return 'from the prototype';
  },
  set: function (v) {
    console.log('the prototype setter ran with', v);
  },
  configurable: true,
});
var defined = { watched: 1 };
var descriptor = Object.getOwnPropertyDescriptor(defined, 'watched');
console.log(descriptor.value, descriptor.writable, descriptor.enumerable, descriptor.configurable);
console.log(defined.watched);
var assigned = {};
assigned.watched = 2;
console.log(Object.getOwnPropertyDescriptor(assigned, 'watched'), assigned.watched);

// The same rule again, with a prototype property that is not writable. An assignment is refused and
// a literal is not.
Object.defineProperty(Object.prototype, 'frozen', {
  value: 'from the prototype',
  writable: false,
  configurable: true,
});
var over = { frozen: 'mine' };
console.log(over.frozen);
var tried = {};
tried.frozen = 'mine';
console.log(tried.frozen, Object.getOwnPropertyDescriptor(tried, 'frozen'));

// An empty literal, which is the case with no properties to define at all.
console.log({}, Object.getOwnPropertyDescriptor({}, 'x'));
