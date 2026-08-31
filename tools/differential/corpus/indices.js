// Properties whose names are array indices, which are stored in a flat array rather than on a shape
// and so take a different path through the runtime than every other property does. Everything here
// has to come out the same as it would if they were ordinary names, because in the language they
// are ordinary names.

// A run of ascending writes, which is the case the storage exists for.
var run = {};
for (var i = 0; i < 40; i++) run[i] = i * 3;
console.log(run[0], run[17], run[39], run[40]);

// A number and the text of that number are one property, and so are the two spellings of zero.
var one = {};
one[0] = 'zero';
console.log(one[0], one['0'], one[-0], one[0.0], one[0] === one['0']);

// Only the spelling `ToString` would produce is an index. Everything else is an ordinary name and
// has to stay one, or the same object would answer two different ways for the same property.
var spelling = {};
spelling[1] = 'one';
spelling['01'] = 'padded';
spelling[' 1'] = 'spaced';
spelling['+1'] = 'signed';
spelling['1.0'] = 'pointed';
console.log(spelling[1], spelling['01'], spelling[' 1'], spelling['+1'], spelling['1.0']);
console.log(JSON.stringify(spelling));

// The largest index is 2^32 - 2, so the value above it is a name and not an index.
var edge = {};
edge[4294967294] = 'last';
edge[4294967295] = 'past';
console.log(edge[4294967294], edge[4294967295], JSON.stringify(edge));

// Indices enumerate first and in ascending order, whatever order they were written in, and the
// names that are not indices keep the order they arrived in.
var order = {};
order.x = 1;
order[2] = 2;
order[0] = 3;
order.a = 4;
order[1] = 5;
console.log(JSON.stringify(order));
console.log(order);

// An index on a prototype is reached from below, and one written below hides it without touching it.
var above = {};
above[0] = 'proto';
above[5] = 'five';
var below = Object.create(above);
console.log(below[0], below['0'], below[5]);
below[0] = 'own';
console.log(below[0], above[0]);

// A name on the object hides an index of the same text on the prototype, which is the other half of
// the same rule and is why the chain is walked once rather than twice.
var named = Object.create(above);
Object.defineProperty(named, '5', {
  value: 'hidden',
  writable: true,
  enumerable: true,
  configurable: true
});
console.log(named[5], above[5]);

// An index far past the end is not worth an array, so it becomes a name. Filling in everything
// below it grows the array out past it, and it still has to be one property afterwards.
var sparse = {};
sparse[3000] = 'named';
console.log(sparse[3000], JSON.stringify(sparse));
for (var j = 0; j < 3000; j++) sparse[j] = j;
sparse[3000] = 'again';
console.log(sparse[3000], sparse['3000']);

// Everything that asks about an own property has to see one that is stored as an element.
var owned = {};
owned[0] = 1;
console.log(owned.hasOwnProperty('0'), owned.hasOwnProperty(0), owned.hasOwnProperty('00'));
console.log(owned.propertyIsEnumerable('0'));
console.log(Object.getOwnPropertyDescriptor(owned, '0'));

// Defining an index with the flags an assignment would give it goes to the same place.
var defined = {};
Object.defineProperty(defined, '0', {
  value: 7,
  writable: true,
  enumerable: true,
  configurable: true
});
console.log(defined[0], JSON.stringify(defined));

// Defining one with any other flags cannot, because there is nowhere to put the flags, so it goes
// under the name instead. It still has to be one property and it still has to enumerate as an index.
var flagged = {};
Object.defineProperty(flagged, '0', { value: 7, writable: true, enumerable: true });
flagged[0] = 9;
flagged.after = 1;
console.log(flagged[0], flagged['0'], JSON.stringify(flagged), flagged);
console.log(Object.getOwnPropertyDescriptor(flagged, '0'));

// The same, with the array growing out over the index afterwards.
var grown = {};
Object.defineProperty(grown, '3', { value: 7, writable: true, enumerable: true });
for (var k = 0; k < 9; k++) if (k !== 3) grown[k] = k;
grown[3] = 9;
console.log(grown[3], grown['3'], JSON.stringify(grown));

// A read only index refuses a write and keeps its value, which is the flags doing their job in the
// place they had to be put.
var frozen = {};
Object.defineProperty(frozen, '2', { value: 5, enumerable: true });
frozen[2] = 99;
console.log(frozen[2], JSON.stringify(frozen));

// A hidden index is there and does not print, the same as a hidden name.
var hidden = {};
hidden[0] = 'a';
Object.defineProperty(hidden, '0', { enumerable: false });
console.log(hidden[0], JSON.stringify(hidden), hidden);
console.log(Object.getOwnPropertyDescriptor(hidden, '0'));

// An object with no prototype at all still has its own indices.
var bare = Object.create(null);
bare[3] = 'x';
console.log(bare[3], JSON.stringify(bare));

// Indices nested inside indices, so the printing path meets them at more than one depth.
var nested = {};
nested[0] = {};
nested[0][1] = 'inner';
console.log(nested, JSON.stringify(nested));

// A key that is not a number at all takes the ordinary path and keeps working.
var mixed = {};
mixed[true] = 'yes';
mixed[null] = 'nothing';
mixed[0] = 'zero';
console.log(JSON.stringify(mixed));
