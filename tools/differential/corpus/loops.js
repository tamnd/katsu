// The three loop forms are three different answers to the same question, and the parts an engine
// gets wrong are the ones that are not where the source suggests they are. A `for` runs its update
// after the body and before the test, a `do while` asks nothing before its first iteration, and a
// `continue` lands somewhere different in each of them.
let out = '';
for (let i = 0; i < 4; i++) {
  out += i;
}
console.log(out);
// The head runs once and the update runs at the bottom, so a counter that is also written in the
// body moves twice per iteration.
let steps = '';
for (let i = 0; i < 6; i++) {
  steps += i;
  i++;
}
console.log(steps);
// Every part of the head is optional, and a `for` with no test never asks anything, so only a
// `break` in the body can end it.
let count = 0;
for (;;) {
  count++;
  if (count === 3) {
    break;
  }
}
console.log(count);
// An empty init and an empty update are a `while` written with more punctuation.
let k = 0;
for (; k < 3; ) {
  k++;
}
console.log(k);
// A `continue` in a `for` lands on the update rather than on the test, which is what keeps the loop
// finite. The same `continue` in a `while` written with the increment at the bottom would not.
let odd = '';
for (let i = 0; i < 6; i++) {
  if (i % 2 === 0) {
    continue;
  }
  odd += i;
}
console.log(odd);
// A `do while` runs its body before it asks anything, so a condition that is false from the start
// still gets one iteration.
let once = 0;
do {
  once++;
} while (false);
console.log(once);
// A `do while` with a body that is not a block is legal and is the shortest loop in the language.
let n = 0;
do n++;
while (n < 3);
console.log(n);
// A `continue` in a `do while` goes to the test and not past it, so the loop still ends.
let visited = '';
let d = 0;
do {
  d++;
  if (d === 2) {
    continue;
  }
  visited += d;
} while (d < 5);
console.log(visited, d);
// A `break` leaves a `do while` without asking the condition at all.
let stopped = 0;
do {
  stopped++;
  if (stopped === 2) {
    break;
  }
} while (true);
console.log(stopped);
// A `let` in a head belongs to the loop and is gone afterwards, and a `var` in a head belongs to the
// function and outlives it. This is the difference the two keywords exist for.
function scoping() {
  for (let inner = 0; inner < 2; inner++) {}
  for (var outer = 0; outer < 2; outer++) {}
  return typeof inner + ' ' + outer;
}
console.log(scoping());
// A name declared in a head is shadowed by a name declared in the body, because the body is its own
// scope inside the head's scope rather than the same one.
let shadowed = '';
for (let v = 'head'; shadowed === ''; ) {
  let v = 'body';
  shadowed = v;
}
console.log(shadowed);
// The head's own name is in scope while its initialiser runs, so this reads the head's `dead` before
// it has a value rather than reading the outer one.
let dead = 'outer';
try {
  for (let dead = dead; false; ) {}
} catch (error) {
  console.log(error.name, error.message);
}
// A `finally` runs on the way out of a loop however the iteration left, which is the case where a
// `continue` and a `break` are easiest to lower as if they were a plain jump.
let trace = '';
for (let i = 0; i < 3; i++) {
  try {
    if (i === 1) {
      continue;
    }
    trace += 'body' + i;
  } finally {
    trace += 'f' + i;
  }
}
console.log(trace);
let left = '';
for (let i = 0; i < 3; i++) {
  try {
    if (i === 1) {
      break;
    }
    left += 'body' + i;
  } finally {
    left += 'g' + i;
  }
}
console.log(left);
// A `break` and a `continue` bind to the nearest loop, so the inner one stops and the outer one
// keeps going.
let grid = '';
for (let i = 0; i < 3; i++) {
  for (let j = 0; j < 3; j++) {
    if (j === 1) {
      break;
    }
    grid += i + '' + j;
  }
}
console.log(grid);
// A loop nested in the body of the other form has to keep the two frames apart, and a `continue` in
// the inner one must not be taken for one in the outer one.
let mixed = '';
let a = 0;
do {
  a++;
  for (let b = 0; b < 2; b++) {
    if (b === 0) {
      continue;
    }
    mixed += a + '' + b;
  }
} while (a < 3);
console.log(mixed);
// The test of a `for` is evaluated once per iteration including the one that ends it, and the update
// runs one time fewer than the test does.
let asked = 0;
let bumped = 0;
function ask(value) {
  asked++;
  return value;
}
function bump(value) {
  bumped++;
  return value + 1;
}
for (let i = 0; ask(i < 3); i = bump(i)) {}
console.log(asked, bumped);
// A loop whose body never runs still runs its head, and the update never runs at all.
let never = 'untouched';
for (let i = 0; i < 0; i++) {
  never = 'touched';
}
console.log(never);
// A `var` head is one binding rather than one per loop, so two loops over the same name in one
// function are writing to the same place.
function shared() {
  var i;
  for (i = 0; i < 2; i++) {}
  let first = i;
  for (i = 0; i < 5; i++) {}
  return first + ' ' + i;
}
console.log(shared());
// A loop is an expression's worth of work in the middle of a function, and a `return` from inside
// one leaves the function rather than the loop.
function firstOver(limit) {
  for (let i = 0; i < 100; i++) {
    if (i > limit) {
      return i;
    }
  }
  return -1;
}
console.log(firstOver(3), firstOver(200));
