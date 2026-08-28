// Block scoping and the order things are evaluated in. A shadowed name and a compound assignment
// are both places an engine can be off by one scope or one read without any test noticing.
let a = 1;
{
  let a = 2;
  console.log(a);
}
console.log(a);
var b = 1;
{
  var b = 2;
}
console.log(b);
let c = 0;
let d = c++;
console.log(c, d);
let e = 0;
let f = ++e;
console.log(e, f);
let g = 1;
g += g += 1;
console.log(g);
let h = 0;
while (h < 3) {
  h += 1;
}
console.log(h);
let i = 0;
let total = 0;
while (i < 4) {
  if (i % 2 === 0) {
    total += i;
  } else {
    total -= i;
  }
  i++;
}
console.log(total);
