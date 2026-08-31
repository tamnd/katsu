// The error constructors, which are the part of the standard library that ordinary code touches
// without meaning to. Every `catch` in a real program looks at what it caught, and what it looks at
// is `name`, `message`, `instanceof` and the text the error converts to, so those are what this file
// compares rather than the printed form of an error.
//
// An error is never printed whole here. Node puts a stack trace on every error it builds and katsu
// has no stacks yet, so `console.log(err)` is a difference that is already known and written down.
// Everything else about an error is the same in both, and that is what this checks.
// All seven exist, all seven are functions, and each one's prototype names itself.
function describe(kind) {
  var made = new kind('m');
  return typeof kind + ' ' + made.name + ' ' + String(made) + ' ' + (made instanceof Error);
}
console.log(describe(Error), '|', describe(TypeError), '|', describe(RangeError));
console.log(describe(ReferenceError), '|', describe(SyntaxError));
console.log(describe(EvalError), '|', describe(URIError));
// The chain, which is what makes one `catch` clause able to look at every kind of error at once.
var wrong = new TypeError('bad');
console.log(wrong instanceof TypeError, wrong instanceof Error, wrong instanceof RangeError);
console.log(Object.getPrototypeOf(TypeError.prototype) === Error.prototype);
console.log(Object.getPrototypeOf(wrong) === TypeError.prototype, wrong.constructor === TypeError);
// `new` and a plain call do the same thing, which is unusual and is the specification's own rule.
var built = new RangeError('same');
var called = RangeError('same');
console.log(called instanceof RangeError, built.message === called.message);
console.log(Object.getPrototypeOf(built) === Object.getPrototypeOf(called));
// A message is only there when one was passed, and the empty one is inherited rather than absent.
var empty = new Error();
console.log(empty.message === '', empty.hasOwnProperty('message'), new Error('x').hasOwnProperty('message'));
console.log(new Error(undefined).hasOwnProperty('message'), new Error(0).message, new Error(null).message);
// The message goes through the same conversion everything else goes through.
console.log(new Error(1.5).message, new Error(true).message, new Error({}).message);
// `toString` drops the half that is empty, and reads both halves off the object rather than the
// class, so a program that renames an error renames what it converts to.
var renamed = new Error('here');
renamed.name = 'Boom';
console.log(String(new Error()), '|', String(new TypeError('m')), '|', String(renamed));
console.log(Error.prototype.toString(), '|', new Error('only').toString());
var nameless = new Error('kept');
nameless.name = '';
console.log(String(nameless), '|', '' + new SyntaxError('joined'));
// `cause` is there when the options bag has one and not there when it does not, including when the
// bag is not an object at all.
console.log(new Error('x', { cause: 7 }).cause, new Error('x', { cause: undefined }).hasOwnProperty('cause'));
console.log(new Error('x', {}).hasOwnProperty('cause'), new Error('x').hasOwnProperty('cause'), new Error('x', 7).hasOwnProperty('cause'));
// What the engine throws is a real error of the right kind, which is the whole point of installing
// these before anything else. Each message is the one node writes, word for word.
function caught(f) {
  try {
    f();
    return 'not reached';
  } catch (e) {
    return e.name + ': ' + e.message + ' [' + (e instanceof Error) + ']';
  }
}
console.log(caught(function () { return undefined.x; }));
console.log(caught(function () { return notDefinedAnywhere; }));
console.log(caught(function () { throw new RangeError('by hand'); }));
console.log(caught(function () { throw 'a string is not an error'; }));
// A thrown error keeps its identity all the way out to the handler.
var thrown = new TypeError('mine');
var back = null;
try {
  throw thrown;
} catch (e) {
  back = e;
}
console.log(back === thrown, back.message, back instanceof TypeError);
// The descriptors, because code that reflects over the realm reads these rather than the values.
var link = Object.getOwnPropertyDescriptor(TypeError, 'prototype');
console.log(link.writable, link.enumerable, link.configurable);
var named = Object.getOwnPropertyDescriptor(Error.prototype, 'name');
console.log(named.value, named.writable, named.enumerable, named.configurable);
var text = Object.getOwnPropertyDescriptor(Error.prototype, 'toString');
console.log(typeof text.value, text.writable, text.enumerable, text.configurable);
console.log(Error.prototype.hasOwnProperty('toString'), TypeError.prototype.hasOwnProperty('toString'));
console.log(Error.prototype.hasOwnProperty('name'), TypeError.prototype.hasOwnProperty('name'));
// Nothing on a prototype is enumerable, so an error with nothing added to it looks empty to code
// that asks what it holds.
var walked = new TypeError('hidden');
console.log(walked.propertyIsEnumerable('message'), TypeError.prototype.propertyIsEnumerable('name'), Error.prototype.propertyIsEnumerable('toString'));
// An object that merely inherits from an error prototype is an error to `instanceof`, which is the
// same question every library asks and the same answer both engines give.
var borrowed = Object.create(RangeError.prototype);
console.log(borrowed instanceof RangeError, borrowed instanceof Error, borrowed.name, String(borrowed));
// An error the program built out of pieces converts by the same two rules.
var pieces = Object.create(Error.prototype);
pieces.name = 'Custom';
pieces.message = 'assembled';
console.log(String(pieces), pieces instanceof Error, pieces.hasOwnProperty('name'));
