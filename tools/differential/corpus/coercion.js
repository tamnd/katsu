// The abstract operations that turn one type into another, which is where most of the surprising
// behaviour in the language lives and where an engine written from the specification and an engine
// written from intuition come apart.
console.log(1 + "1");
console.log("1" - 1);
console.log(1 + null);
console.log(1 + undefined);
console.log("" + null);
console.log("" + undefined);
console.log(true + true);
console.log("10" < "9");
console.log(10 < 9);
console.log("10" < 9);
console.log(null == undefined);
console.log(null === undefined);
console.log(null == 0);
console.log(null >= 0);
console.log(NaN == NaN);
console.log(typeof null);
console.log(typeof undefined);
console.log(typeof 1);
console.log(typeof "a");
console.log(typeof true);
console.log(!"");
console.log(!"0");
console.log(!0);
