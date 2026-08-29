// Prototype chains. A property that is not on an object is looked for above it, an object carries
// its prototype in its shape rather than in itself, and an object with no prototype at all is a
// different kind of thing that node goes out of its way to mark when it prints one.
console.log(Object.getPrototypeOf({}) === Object.prototype);
console.log(Object.getPrototypeOf(Object.prototype));
var parent = { x: 1, shared: "from the prototype" };
var child = Object.create(parent);
console.log(child.x);
console.log(child.shared);
console.log(child.missing);
console.log(Object.getPrototypeOf(child) === parent);
// A write makes an own property. There are no setters, so nothing goes up the chain, and the
// prototype keeping its own value is the whole observable difference between inheriting a property
// and sharing one.
var sibling = Object.create(parent);
child.x = 2;
console.log(child.x, sibling.x, parent.x);
// The walk is the whole chain and not one step of it.
var grandchild = Object.create(Object.create({ deep: "found" }));
console.log(grandchild.deep);
console.log(Object.create(null, undefined).nothing);
console.log(Object.getPrototypeOf(Object.create(null)));
// An object with no prototype has no `toString` to convert it, which is a TypeError rather than a
// text. Having a prototype is not enough either: what matters is whether the walk arrives somewhere
// that has one.
try {
  String(Object.create(null));
  console.log("no error");
} catch (error) {
  console.log("threw");
}
try {
  console.log("" + Object.create(Object.create(null)));
} catch (error) {
  console.log("threw");
}
console.log(String({}), "" + { a: 1 });
// Refusing to create from something that is neither an object nor null, and naming the value.
try {
  Object.create(1);
} catch (error) {
  console.log("threw");
}
try {
  Object.getPrototypeOf(null);
} catch (error) {
  console.log("threw");
}
// How an object with no prototype prints, which is not how an ordinary one prints. The tag counts
// towards the width, so the last two of these break onto several lines at a point an ordinary
// object with the same contents would still fit on one.
console.log(Object.create(null));
var bare = Object.create(null);
bare.x = 1;
bare.y = { z: 2 };
console.log(bare);
console.log({ inner: Object.create(null) });
console.log({ a: { b: { c: Object.create(null) } } });
var wide = Object.create(null);
var ordinary = {};
for (var i = 0; i < 5; i++) {
  wide["k" + i] = 100000 + i;
  ordinary["k" + i] = 100000 + i;
}
console.log(ordinary);
console.log(wide);
// Own properties only, for the two things that walk an object rather than reading one name off it.
console.log(JSON.stringify(child));
console.log(JSON.stringify(Object.create({ a: 1 })));
console.log(typeof Object.prototype, typeof Object.create, typeof Object.getPrototypeOf);
