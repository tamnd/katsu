// Number to string is where two correct engines stop agreeing first, because the specification
// gives an exact algorithm and almost nobody implements it exactly. Every line here is a boundary
// in that algorithm rather than an arbitrary value.
console.log(0.1 + 0.2);
console.log(1e21);
console.log(1e-7);
console.log(0.000001);
console.log(-0);
console.log(1 / 0);
console.log(-1 / 0);
console.log(0 / 0);
console.log(123456789012345678901234);
console.log(5e-324);
console.log(1.7976931348623157e308);
console.log(9007199254740993);
console.log(100 / 3);
