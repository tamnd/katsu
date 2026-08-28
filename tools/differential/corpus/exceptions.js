// Exceptions are where a control flow bug hides best, because the path that fires is the one nobody
// runs while writing the code. Every case here is a place two engines can disagree about where a
// throw lands rather than about what it computes.
//
// A caught engine error is asked for its `name` and `message` and never printed whole. Node prints
// one as `TypeError: ...` because it is an `Error` with a prototype behind it, and katsu's is an
// object with those two properties until prototypes arrive, so printing it would be comparing a
// difference that is already known and written down rather than looking for a new one.
function thrower(value) {
  throw value;
}
// Every kind of value survives the trip, including the ones that are easy to special case wrongly.
function roundTrip(value) {
  try {
    thrower(value);
    return 'not reached';
  } catch (e) {
    return e;
  }
}
console.log(roundTrip(1), roundTrip('text'), roundTrip(true), roundTrip(null));
console.log(roundTrip(undefined), roundTrip(0), roundTrip(''), roundTrip(NaN));
console.log(roundTrip({ a: 1 }));
// A throw walks out of every frame between it and the handler, and none of them finish.
let trail = '';
function bottom() {
  trail += 'b';
  throw 'deep';
  trail += 'unreachable';
}
function middle() {
  trail += 'm';
  bottom();
  trail += 'after';
}
function top() {
  trail += 't';
  middle();
}
try {
  top();
} catch (e) {
  trail += e;
}
console.log(trail);
// A throw inside a `catch` is not caught by the `catch` it is written in, which is the whole reason
// the protected range stops where the handler starts.
let order = '';
try {
  try {
    throw 'first';
  } catch (e) {
    order += e;
    throw 'second';
  }
} catch (e) {
  order += e;
}
console.log(order);
// The innermost handler wins, and the outer one never runs when the inner one catches.
function nested(fail) {
  let seen = '';
  try {
    try {
      if (fail) {
        throw 'inner';
      }
      seen += 'clean';
    } catch (e) {
      seen += 'caught ' + e;
    }
  } catch (e) {
    seen += 'outer ' + e;
  }
  return seen;
}
console.log(nested(true), nested(false));
// A `try` that does not throw costs nothing and changes nothing, which is the case that has to keep
// working while the interesting one is being made to work.
let quiet = 0;
try {
  quiet = 1;
} catch (e) {
  quiet = 2;
}
console.log(quiet);
// Engine errors are catchable and say what node says.
try {
  null.x;
} catch (e) {
  console.log(e.name, e.message);
}
try {
  undefined.y;
} catch (e) {
  console.log(e.name, e.message);
}
try {
  missingName;
} catch (e) {
  console.log(e.name, e.message);
}
try {
  const frozen = 1;
  frozen = 2;
} catch (e) {
  console.log(e.name, e.message);
}
try {
  const notCallable = 5;
  notCallable();
} catch (e) {
  console.log(e.name);
}
// Running out of stack is an exception like any other and a program can catch it.
function endless() {
  return endless();
}
try {
  endless();
} catch (e) {
  console.log(e.name);
}
// A `catch` with no binding, for a handler that does not care what happened.
let ignored = 'before';
try {
  throw 'thrown away';
} catch {
  ignored = 'after';
}
console.log(ignored);
// The parameter shadows an outer name of the same shape and gives it back at the closing brace.
let e = 'outer';
try {
  throw 'inner';
} catch (e) {
  console.log(e);
}
console.log(e, typeof e);
// A `var` hoists out of all three of a try's parts and a `let` in one of them does not collide with
// a `var` in another, because the parts are scopes side by side rather than nested.
try {
  var hoisted = 'from the try';
} catch (e) {
  var alsoHoisted = 'from the catch';
}
console.log(hoisted, alsoHoisted);
try {
  let q = 1;
  console.log(q);
} catch (e) {
  var q = 2;
}
console.log(q);
// A throw inside a loop, caught inside the loop, leaves the loop running.
let sum = 0;
let i = 0;
while (i < 5) {
  try {
    if (i % 2 === 0) {
      throw i;
    }
    sum += 100;
  } catch (e) {
    sum += e;
  }
  i++;
}
console.log(sum);
// A throw inside a loop caught outside it ends the loop wherever it was.
let reached = 0;
try {
  let j = 0;
  while (j < 10) {
    reached = j;
    if (j === 3) {
      throw 'stop';
    }
    j++;
  }
} catch (e) {
  reached = reached + 100;
}
console.log(reached);
// `break` and `continue` still leave a `try` correctly, since there is no `finally` in the way.
let jumps = '';
let k = 0;
while (k < 4) {
  k++;
  try {
    if (k === 2) {
      continue;
    }
    if (k === 3) {
      break;
    }
    jumps += k;
  } catch (e) {
    jumps += 'never';
  }
}
console.log(jumps, k);
// A `return` out of a protected block returns from the function it is in.
function returnsFromInside() {
  try {
    return 'from the try';
  } catch (e) {
    return 'from the catch';
  }
}
function returnsFromTheHandler() {
  try {
    throw 'x';
  } catch (e) {
    return 'from the catch';
  }
}
console.log(returnsFromInside(), returnsFromTheHandler());
// A caught value is an ordinary value: it can be stored, read through and thrown again.
const box = { held: 0 };
try {
  throw { code: 7, label: 'seven' };
} catch (e) {
  box.held = e.code;
  console.log(e.label, e.code, box.held);
}
// A closure written inside a handler still sees the caught value after the handler has finished,
// which is the case where the parameter lives in a cell rather than in a frame slot.
let later = null;
try {
  throw 'captured';
} catch (e) {
  later = function () {
    return e;
  };
}
console.log(later());
