// Property access with a key the program works out rather than writes down. `o[k]` reaches the
// same properties `o.x` does, and the interesting part is everything that happens to the key on the
// way there.

// The plain case, read and write, with a key that happens to be the name of a property.
var o = { a: 1, b: 2 };
var k = 'a';
console.log(o[k], o['b']);
o[k] = 10;
o['c'] = 3;
console.log(o.a, o.c, o[k], o['c']);

// A name nothing has reads as undefined rather than throwing, the same as a dotted one.
console.log(o['nothing'], o[k + 'a']);

// Every key is a string, so a number and the text of that number are one property.
var numbered = {};
numbered[0] = 'zero';
console.log(numbered[0], numbered['0'], numbered['0'] === numbered[0]);
numbered[1.5] = 'half';
console.log(numbered['1.5'], numbered[1.5]);
numbered[-0] = 'minus zero';
console.log(numbered['0']);
numbered[1e21] = 'big';
console.log(numbered['1e+21']);
numbered[NaN] = 'not a number';
console.log(numbered['NaN']);

// The other primitives convert the same way they would in a string concatenation.
var converted = {};
converted[true] = 'yes';
converted[false] = 'no';
converted[null] = 'null';
converted[undefined] = 'undefined';
console.log(converted['true'], converted['false'], converted['null'], converted['undefined']);
console.log(converted);

// An object key goes through toString like anything else being made into text. A toString written
// in JavaScript is the known gap that `'' + o` has too, so the one measured here is a builtin's.
var holder = {};
holder[new TypeError('boom')] = 'here';
console.log(holder['TypeError: boom'], holder);

// A key does not have to be constant to reach an accessor, and the accessor still runs.
var accessor = {
  get x() { return 'got'; },
  set x(value) { console.log('set to ' + value); }
};
var accessed = 'x';
console.log(accessor[accessed]);
accessor[accessed] = 'a value';

// A prototype's property is found through a computed key too.
var parent = { inherited: 'from above' };
var child = Object.create(parent);
console.log(child['inherited']);
child['inherited'] = 'own now';
console.log(child.inherited, parent.inherited);

// Reading and writing a property of nothing, which names the key when it can name it safely.
try {
  var missing;
  missing[k];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  var alsoMissing = null;
  alsoMissing[0];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  var third;
  third[k] = 1;
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  var fourth = null;
  fourth[2] = 1;
} catch (error) {
  console.log(error.name + ': ' + error.message);
}

// Naming the key would mean running the program's own code, so node does not name it at all rather
// than run it to build an error message. What it offers instead is the constructor's name, and only
// when the toString the object reaches is the one on Object.prototype.
try {
  var nothing;
  nothing[{}];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
function Weird() {}
try {
  var built;
  built[new Weird()];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  var throwing;
  throwing[{ toString: function () { throw new Error('should not run'); } }];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  var bare;
  bare[Object.create(null)];
} catch (error) {
  console.log(error.name + ': ' + error.message);
}

// The same four refusals a dotted store gets in strict mode, since by the time there is a name
// there is nothing left that says how the program spelled it.
var locked = {};
Object.defineProperty(locked, 'a', { value: 1 });
locked['a'] = 2;
console.log(locked.a);
try {
  (function () {
    'use strict';
    locked['a'] = 3;
  })();
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  (function () {
    'use strict';
    var getterOnly = { get only() { return 1; } };
    getterOnly['only'] = 2;
  })();
} catch (error) {
  console.log(error.name + ': ' + error.message);
}
try {
  (function () {
    'use strict';
    var number = 5;
    number['x'] = 1;
  })();
} catch (error) {
  console.log(error.name + ': ' + error.message);
}

// Outside strict mode all three are silent, and the write goes nowhere.
var number = 5;
number['x'] = 1;
console.log(number.x);

// The key is worked out once and the object once, in that order, which is visible when both of them
// have something to say.
function say(what, value) {
  console.log(what);
  return value;
}
say('object', { seen: 'yes' })[say('key', 'seen')];
