// A label is a name for a statement, and a labelled jump is the only way out of more than one loop
// at once. The cases worth checking are the ones where the jump has to walk past a frame that would
// have caught an unlabelled jump, because that walk is the whole feature and it is easy to be off
// by one frame in either direction.
let out = '';
outer: for (let i = 0; i < 3; i++) {
  for (let j = 0; j < 3; j++) {
    if (j === 1) {
      continue outer;
    }
    out += '' + i + j;
  }
}
console.log(out);
// The same shape with a `break` ends both loops rather than one, so the outer counter never reaches
// its bound.
let pairs = '';
search: for (let i = 0; i < 3; i++) {
  for (let j = 0; j < 3; j++) {
    pairs += '' + i + j;
    if (i + j === 2) {
      break search;
    }
  }
}
console.log(pairs);
// An unlabelled jump in the same place leaves the nearest loop and nothing else, which is what the
// labelled versions above are being compared against.
let near = '';
for (let i = 0; i < 3; i++) {
  for (let j = 0; j < 3; j++) {
    if (j === 1) {
      break;
    }
    near += '' + i + j;
  }
}
console.log(near);
// A label goes on any statement at all, so a labelled block plus a `break` is an early exit out of
// a piece of straight line code, with no loop involved.
function classify(n) {
  let answer = 'other';
  done: {
    if (n < 0) {
      answer = 'negative';
      break done;
    }
    if (n === 0) {
      answer = 'zero';
      break done;
    }
    answer = 'positive';
  }
  return answer;
}
console.log(classify(-1), classify(0), classify(7));
// Several labels can name one statement, and every one of them denotes it, so a `continue` may use
// whichever name it likes.
let chained = '';
a: b: for (let i = 0; i < 4; i++) {
  if (i % 2 === 0) {
    continue b;
  }
  chained += i;
}
console.log(chained);
// A `break` inside a `switch` inside a loop leaves the switch, and a labelled one leaves the loop,
// which is the case where the unlabelled default is almost never what was meant.
let picked = '';
loop: for (let i = 0; i < 5; i++) {
  switch (i) {
    case 1:
      picked += 'one';
      break;
    case 3:
      picked += 'three';
      break loop;
    default:
      picked += '.';
  }
}
console.log(picked);
// A labelled jump out through a `finally` still runs the `finally`, and two different labelled
// jumps through the same `finally` have to end up in two different places.
let trace = '';
first: for (let i = 0; i < 3; i++) {
  second: for (let j = 0; j < 3; j++) {
    try {
      if (j === 1) {
        break first;
      }
      if (i === 0) {
        break second;
      }
      trace += 'body';
    } finally {
      trace += 'f';
    }
  }
}
console.log(trace);
// The same through two nested `finally` clauses, where the jump has to be picked up and put down
// again by each of them in turn.
let both = '';
away: while (true) {
  try {
    try {
      break away;
    } finally {
      both += 'inner';
    }
  } finally {
    both += 'outer';
  }
}
console.log(both);
// A labelled `continue` through a `finally` goes to the update of the loop it names, not to the end
// of the loop, so the counter still moves and the loop still ends.
let kept = '';
counting: for (let i = 0; i < 3; i++) {
  try {
    continue counting;
  } finally {
    kept += i;
  }
}
console.log(kept);
// A `return` written inside a labelled block inside a `finally` still returns, and the label around
// it changes nothing about that.
function returning() {
  try {
    return 'from the block';
  } finally {
    skip: {
      break skip;
    }
  }
}
console.log(returning());
// A label on a `do while` names an iteration statement like any other loop, so both kinds of jump
// can aim at it.
let counted = 0;
spin: do {
  counted++;
  if (counted < 3) {
    continue spin;
  }
  break spin;
} while (true);
console.log(counted);
// Labels and variables are two namespaces that never meet, so a label may reuse a name that is a
// live variable and neither one shadows the other.
let x = 'value';
x: for (let i = 0; i < 2; i++) {
  if (i === 0) {
    continue x;
  }
  x += ' seen';
}
console.log(x);
// A label goes out of scope at the end of the statement it names, so the same name can be used
// again next to it, and again inside a nested function.
let reused = '';
one: for (let i = 0; i < 2; i++) {
  reused += 'a';
  continue one;
}
one: for (let i = 0; i < 2; i++) {
  reused += 'b';
  continue one;
}
function inner() {
  one: for (let i = 0; i < 2; i++) {
    reused += 'c';
    continue one;
  }
}
inner();
console.log(reused);
