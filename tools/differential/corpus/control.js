// A switch is where an engine's control flow is easiest to get subtly wrong, because almost every
// part of it is the opposite of what the syntax suggests. Clauses are not scopes of their own, a
// default in the middle is still compared last, and falling out of one clause falls into the next.
function pick(x) {
  let out = '';
  switch (x) {
    case 1:
      out += 'one';
    case 2:
      out += 'two';
      break;
    default:
      out += 'other';
      break;
    case 3:
      out += 'three';
  }
  return out;
}
console.log(pick(1), pick(2), pick(3), pick(4));
// The comparison is strict, so a string never matches a number and NaN never matches itself.
console.log(pick('1'), pick(NaN));
// A switch with no clause that matches and no default runs nothing at all.
function nothing(x) {
  let out = 'before';
  switch (x) {
    case 1:
      out = 'matched';
  }
  return out;
}
console.log(nothing(0), nothing(1));
// An empty clause is how two values are made to share a body.
function shared(x) {
  switch (x) {
    case 1:
    case 2:
      return 'low';
    default:
      return 'high';
  }
}
console.log(shared(1), shared(2), shared(3));
// The discriminant is evaluated once, before anything else, and every case test after it in
// source order until one matches. Nothing after the match is evaluated.
let calls = '';
function seen(name, value) {
  calls += name;
  return value;
}
switch (seen('d', 2)) {
  case seen('a', 1):
    break;
  case seen('b', 2):
    break;
  case seen('c', 3):
    break;
}
console.log(calls);
// Every clause shares one scope, so this is one binding and not three.
switch (1) {
  case 1:
    let shared_binding = 'first';
    console.log(shared_binding);
  case 2:
    console.log(shared_binding);
}
// A break leaves the switch and a continue leaves the iteration, so a loop with both in it visits
// a different set of values depending on which one ran.
let log = '';
let i = 0;
while (i < 8) {
  i = i + 1;
  switch (i) {
    case 2:
      continue;
    case 5:
      break;
    case 7:
      log += 'seven';
      continue;
  }
  log += i;
}
console.log(log, i);
// A break in a loop stops the loop rather than the iteration, and the counter keeps the value it
// had when it stopped.
let j = 0;
while (true) {
  j = j + 1;
  if (j === 4) {
    break;
  }
}
console.log(j);
// A nested switch catches its own break and leaves the outer one alone.
function nested(x, y) {
  let out = '';
  switch (x) {
    case 1:
      switch (y) {
        case 1:
          out += 'a';
          break;
        default:
          out += 'b';
      }
      out += 'c';
      break;
    default:
      out += 'd';
  }
  return out;
}
console.log(nested(1, 1), nested(1, 2), nested(2, 1));
