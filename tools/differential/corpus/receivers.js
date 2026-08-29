// Receivers. What `this` is inside a call, and the methods on `Object.prototype` that exist because
// there is now something for them to read.

// A method reads the object it was called on, and two calls on two objects read two different ones.
var counter = {
  n: 0,
  bump: function () {
    this.n = this.n + 1;
    return this.n;
  },
};
console.log(counter.bump(), counter.bump(), counter.n);

var one = { name: 'one', who: function () { return this.name; } };
var two = { name: 'two', who: one.who };
console.log(one.who(), two.who());

// The receiver is the object the call went through and not the one the function came from, which is
// what taking a method off one object and calling it on another shows.
var nested = {
  name: 'outer',
  go: function () {
    var inner = { name: 'inner', who: this.who };
    return inner.who();
  },
  who: function () {
    return this.name;
  },
};
console.log(nested.go());

// A strict function called on nothing gets undefined, and that is an answer rather than a gap.
function strictly() {
  'use strict';
  return this;
}
console.log(strictly(), typeof strictly());

// A method that returns the receiver itself, which is how identity is checked without a name.
var self = { me: function () { return this; } };
console.log(self.me() === self, self.me() === {});

// Every object has these now, and they come off the prototype rather than off the object.
var plain = { a: 1 };
console.log(plain.toString(), plain.valueOf() === plain);
console.log(typeof plain.toString, typeof plain.valueOf, typeof plain.hasOwnProperty);
console.log(plain.hasOwnProperty('a'), plain.hasOwnProperty('b'), plain.hasOwnProperty('toString'));

// Own and not inherited, which is the whole reason the method exists.
var child = Object.create(plain);
console.log(child.a, child.hasOwnProperty('a'));

// The key is converted the way any property name is.
console.log(plain.hasOwnProperty(undefined), ({ undefined: 1 }).hasOwnProperty(undefined));

// Anywhere on the chain and not just immediately above, and the object itself does not count.
var top = {};
var middle = Object.create(top);
var bottom = Object.create(middle);
console.log(top.isPrototypeOf(bottom), middle.isPrototypeOf(bottom), bottom.isPrototypeOf(top));
console.log(top.isPrototypeOf(top), top.isPrototypeOf(5), top.isPrototypeOf(undefined));
console.log(Object.prototype.isPrototypeOf(plain), Object.prototype.isPrototypeOf(top));
console.log(Object.prototype.isPrototypeOf(Object.create(null)));

// A hidden property is still an own property, which is where these two questions come apart.
var hidden = { shown: 1 };
Object.defineProperty(hidden, 'kept', { value: 2 });
console.log(hidden.hasOwnProperty('kept'), hidden.propertyIsEnumerable('kept'));
console.log(hidden.hasOwnProperty('shown'), hidden.propertyIsEnumerable('shown'));
console.log(hidden.propertyIsEnumerable('missing'), hidden.propertyIsEnumerable('toString'));

// And the methods themselves are hidden, which is what stops them appearing in every object printed
// and in everything serialised.
console.log(plain, JSON.stringify(plain));
console.log(Object.getOwnPropertyDescriptor(Object.prototype, 'hasOwnProperty'));
console.log(Object.getOwnPropertyDescriptor(Object.prototype, 'toString').enumerable);
console.log(Object.getOwnPropertyDescriptor(Object.prototype, 'valueOf').writable);
console.log(Object.getOwnPropertyDescriptor(Object.prototype, 'isPrototypeOf').configurable);
console.log(Object.getOwnPropertyDescriptor(plain, 'toString'));

// An object made with no prototype has none of them, because it inherits from nothing.
var bare = Object.create(null);
console.log(bare.toString, bare.hasOwnProperty, bare.valueOf);
