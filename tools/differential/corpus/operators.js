// The bitwise operators, which truncate to thirty two bits, and the shifts, which mask their right
// operand. Both are easy to implement almost correctly and the difference only shows at the edges.
console.log(2147483647 | 0);
console.log(2147483648 | 0);
console.log(4294967295 | 0);
console.log(-1 >>> 0);
console.log(1 << 31);
console.log(1 << 32);
console.log(1 << 33);
console.log(-8 >> 1);
console.log(-8 >>> 1);
console.log(~0);
console.log(~-1);
console.log(5 % 3);
console.log(-5 % 3);
console.log(5 % -3);
console.log(2 ** 10);
console.log((-2) ** 2);
console.log(0.1 % 0.03);
console.log(1 && 2);
console.log(0 || "fallback");
console.log(null ?? "fallback");
console.log(0 ?? "fallback");
