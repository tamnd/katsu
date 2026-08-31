// `new` and `instanceof`. Constructing is a property read and a call and an allocation that has to
// survive the call, and `instanceof` is a walk up the chain that the read decides the top of, so the
// two are one subject and are tested as one.
function Point(x, y) {
  this.x = x;
  this.y = y;
}
var origin = new Point(0, 0);
var somewhere = new Point(3, 4);
console.log(origin, somewhere);
console.log(origin.x, somewhere.y);
console.log(origin instanceof Point, origin instanceof Object);
// The object inherits rather than copies, so a method added after the object was built is on it.
Point.prototype.magnitude = function () {
  return this.x * this.x + this.y * this.y;
};
console.log(somewhere.magnitude());
console.log(Object.getPrototypeOf(somewhere) === Point.prototype);
console.log(somewhere.constructor === Point);
console.log(somewhere.hasOwnProperty("x"), somewhere.hasOwnProperty("magnitude"));
// What the constructor returns. An object wins, and anything else is dropped and the fresh object
// is the value of the expression. A function counts as an object here.
function ReturnsObject() {
  this.a = 1;
  return { b: 2 };
}
function ReturnsPrimitive() {
  this.a = 1;
  return 5;
}
function ReturnsNull() {
  this.a = 1;
  return null;
}
function ReturnsNothing() {
  this.a = 1;
  return;
}
function ReturnsFunction() {
  return Point;
}
console.log(new ReturnsObject());
console.log(new ReturnsPrimitive());
console.log(new ReturnsNull());
console.log(new ReturnsNothing());
console.log(new ReturnsFunction() === Point);
// The prototype is read at construction time, so moving it moves what later objects inherit and
// leaves the ones already built alone.
function Movable() {}
var before = new Movable();
Movable.prototype = { tag: "second" };
var after = new Movable();
console.log(before.tag, after.tag);
console.log(before instanceof Movable, after instanceof Movable);
// A prototype that is not an object at all. The instance falls back to `Object.prototype` rather
// than to nothing, which is visible both in what it inherits and in how it prints.
function Broken() {
  this.a = 1;
}
Broken.prototype = 5;
var fallback = new Broken();
console.log(fallback);
console.log(Object.getPrototypeOf(fallback) === Object.prototype);
console.log(fallback.hasOwnProperty("a"));
Broken.prototype = null;
console.log(Object.getPrototypeOf(new Broken()) === Object.prototype);
// Inheritance without `class`, which is a constructor whose prototype is an instance of another.
function Animal(name) {
  this.name = name;
}
Animal.prototype.speak = function () {
  return this.name + " makes a sound";
};
// The parent constructor is not called here, because calling it needs `Function.prototype.call`,
// which is not there yet. Setting the property the parent would have set makes the same object.
function Dog(name) {
  this.name = name;
}
Dog.prototype = new Animal("prototype");
Dog.prototype.constructor = Dog;
var rex = new Dog("Rex");
console.log(rex.speak());
console.log(rex instanceof Dog, rex instanceof Animal, rex instanceof Point);
console.log(rex);
// Nested construction, which matters because the fresh object lives in the callee's register until
// the call returns and the inner `new` allocates its own registers inside that.
function Box(held) {
  this.held = held;
}
console.log(new Box(new Box(new Point(1, 2))));
console.log(new Box(new Point(1, 2)).held instanceof Point);
// Constructing in a loop, which is the case the inline cache on the prototype read exists for.
var total = 0;
for (var i = 0; i < 50; i++) {
  total += new Point(i, i).x;
}
console.log(total);
// A primitive on the left of `instanceof` is false rather than an error, which is the one place the
// operator is forgiving.
console.log(5 instanceof Point);
console.log("text" instanceof Point);
console.log(null instanceof Point, undefined instanceof Point);
console.log(true instanceof Point);
console.log(Point instanceof Object, Point instanceof Point);
// The three ways a right hand side can be wrong, which say three different things.
try {
  console.log({} instanceof {});
} catch (error) {
  console.log(error.name, error.message);
}
try {
  console.log({} instanceof 5);
} catch (error) {
  console.log(error.name, error.message);
}
try {
  console.log({} instanceof null);
} catch (error) {
  console.log(error.name, error.message);
}
try {
  console.log({} instanceof "text");
} catch (error) {
  console.log(error.name, error.message);
}
try {
  console.log({} instanceof Broken);
} catch (error) {
  console.log(error.name, error.message);
}
function Text() {}
Text.prototype = "abc";
try {
  console.log({} instanceof Text);
} catch (error) {
  console.log(error.message);
}
// A primitive on the left answers before `prototype` is read, so the same right hand side that
// throws for an object is quietly false for a number.
console.log(0 instanceof Text, "abc" instanceof Text, null instanceof Text);
// Constructing something that is not a constructor. Node names the expression and katsu names the
// value, which is the same difference `Op::Call` already carries, so only the kind of error and
// whether it can be caught are compared here.
var notAFunction = {};
try {
  new notAFunction();
} catch (error) {
  console.log(error.name);
}
try {
  var five = 5;
  new five();
} catch (error) {
  console.log(error.name);
}
// A constructor that throws leaves nothing behind, and the object it was building is unreachable.
function Explodes() {
  this.a = 1;
  throw "boom";
}
try {
  new Explodes();
} catch (error) {
  console.log(error);
}
// How an instance prints, which is the constructor's name and then the properties. The name counts
// towards the width, so an instance breaks onto several lines earlier than a plain object holding
// the same thing.
function Named() {}
console.log(new Named());
var wide = new Named();
wide.k0 = 1000000;
wide.k1 = 1000001;
wide.k2 = 1000002;
wide.k3 = 1000003;
var plain = {};
plain.k0 = 1000000;
plain.k1 = 1000001;
plain.k2 = 1000002;
plain.k3 = 1000003;
console.log(plain);
console.log(wide);
console.log({ held: new Point(1, 2) });
console.log({ a: { b: { c: new Point(1, 2) } } });
var cyclic = new Point(1, 2);
cyclic.self = cyclic;
console.log(cyclic);
