// `String` and `JSON.stringify`, the two ways a program turns a value it computed into text it can
// print. Both of them are full of cases that look like rounding errors until you read the
// specification, and the only way to be sure of any of them is to ask node.
console.log(String(undefined));
console.log(String(null));
console.log(String(true));
console.log(String(0));
console.log(String(-0));
console.log(String(1.5));
console.log(String(NaN));
console.log(String(Infinity));
console.log(String(-Infinity));
console.log(String("hi"));
console.log(String({}));
console.log(String({ a: 1 }));
console.log("[" + String() + "]");
console.log(String(1e21));
console.log(String(1e-7));
console.log(String(0.1 + 0.2));
console.log(String(9007199254740993));
console.log(String(1) === "" + 1);
console.log(String(-0) === "" + -0);
console.log(String({}) === "" + {});
console.log(JSON.stringify(null));
console.log(JSON.stringify(true));
console.log(JSON.stringify(0));
console.log(JSON.stringify(-0));
console.log(JSON.stringify(1.5));
console.log(JSON.stringify(NaN));
console.log(JSON.stringify(Infinity));
console.log(JSON.stringify(1e21));
console.log(JSON.stringify(1e-7));
console.log(JSON.stringify("hi"));
console.log(JSON.stringify('a"b'));
console.log(JSON.stringify("a\\b"));
console.log(JSON.stringify("a\nb"));
console.log(JSON.stringify("a\tb"));
console.log(JSON.stringify("ab"));
console.log(JSON.stringify("café"));
console.log(JSON.stringify({}));
console.log(JSON.stringify({ a: 1, b: "x", c: true, d: null }));
console.log(JSON.stringify({ a: 1, b: undefined, c: 2 }));
console.log(JSON.stringify({ f: function () {}, u: undefined }));
console.log(JSON.stringify({ z: 1, a: 2, m: 3 }));
console.log(JSON.stringify({ "k e y": 1 }));
console.log(JSON.stringify({ n: NaN, i: Infinity, z: -0 }));
console.log(JSON.stringify({ a: { b: { c: 1 } } }));
console.log(JSON.stringify({ a: 1, b: { c: 2 } }, null, 2));
console.log(JSON.stringify({ a: 1 }, null, 20));
console.log(JSON.stringify({ a: 1 }, null, 2.9));
console.log(JSON.stringify({ a: 1 }, null, 0));
console.log(JSON.stringify({ a: 1 }, null, -1));
console.log(JSON.stringify({ a: 1 }, null, "ab"));
console.log(JSON.stringify({ a: 1 }, null, ""));
console.log(JSON.stringify({ a: 1 }, null, "0123456789abc"));
console.log(JSON.stringify({ a: 1 }, null, true));
console.log(JSON.stringify({}, null, 2));
console.log(JSON.stringify({ a: {} }, null, 2));
console.log(String(JSON.stringify(undefined)));
console.log(String(JSON.stringify(function () {})));
console.log(typeof JSON, typeof JSON.stringify, typeof String);
// A cycle is a TypeError under both, which is the one case here that is an exception rather than an
// answer, so it is caught and the fact of it printed rather than left to end the program.
var cyclic = {};
cyclic.self = cyclic;
try {
  JSON.stringify(cyclic);
  console.log("no error");
} catch (error) {
  console.log("threw");
}
