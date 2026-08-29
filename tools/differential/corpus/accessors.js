// Accessor properties. A property whose slot holds a pair of functions rather than a value, which
// means every read and every write on it is a call, and the receiver of that call is where the
// access started rather than where the property was found.

// The simplest one. Reading calls the getter, and `this` is the object that was read.
var basic = {};
Object.defineProperty(basic, 'x', {
  get: function () {
    return 42;
  },
});
console.log(basic.x);

// A pair that stores somewhere else, which is the whole reason accessors exist.
var stored = {};
Object.defineProperty(stored, 'value', {
  get: function () {
    return this.hidden;
  },
  set: function (v) {
    this.hidden = v * 2;
  },
});
stored.value = 21;
console.log(stored.value, stored.hidden);

// Printing. node names the halves rather than calling them, so an object with an accessor prints
// without running any user code at all.
var shown = {};
Object.defineProperty(shown, 'g', { get: function () {}, enumerable: true });
Object.defineProperty(shown, 's', { set: function () {}, enumerable: true });
Object.defineProperty(shown, 'b', { get: function () {}, set: function () {}, enumerable: true });
Object.defineProperty(shown, 'n', { enumerable: true, configurable: true });
console.log(shown);

// A property defined with neither half is still an accessor, and reading it answers undefined
// because there is no getter to call.
console.log(shown.n, shown.n === undefined);

// Describing one answers with get and set rather than value and writable.
var described = {};
var reader = function () {
  return 1;
};
Object.defineProperty(described, 'r', { get: reader, enumerable: true });
var descriptor = Object.getOwnPropertyDescriptor(described, 'r');
console.log(descriptor.get === reader, descriptor.set, descriptor.enumerable, descriptor.configurable);

// A half that is not a function is refused, by name, before anything is stored.
try {
  Object.defineProperty({}, 'x', { get: 5 });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty({}, 'x', { set: 'no' });
} catch (error) {
  console.log(error.message);
}
// And undefined is allowed, because that is how a half is left out on purpose.
console.log(Object.getOwnPropertyDescriptor(Object.defineProperty({}, 'x', { get: undefined }), 'x'));

// A second define keeps the half it does not mention.
var kept = {};
var first = function () {
  return 1;
};
var second = function (v) {
  this.seen = v;
};
Object.defineProperty(kept, 'k', { get: first, configurable: true });
Object.defineProperty(kept, 'k', { set: second });
var both = Object.getOwnPropertyDescriptor(kept, 'k');
console.log(both.get === first, both.set === second);

// The chain. An accessor on a prototype answers for everything below it, and the receiver is the
// object the read started from rather than the one holding the property.
var above = {};
Object.defineProperty(above, 'who', {
  get: function () {
    return this.name;
  },
  set: function (v) {
    this.stored = v;
  },
});
var below = Object.create(above);
below.name = 'child';
console.log(below.who);
below.who = 42;
console.log(below.stored, Object.getOwnPropertyDescriptor(below, 'who'));

// A setter on the chain takes the write, so the object below gains no own property of that name.
var sibling = Object.create(above);
sibling.who = 1;
console.log(Object.getOwnPropertyDescriptor(sibling, 'who'), sibling.stored);

// Writing to something that has only a getter is ignored in sloppy mode and refused in strict.
var readonly = {};
Object.defineProperty(readonly, 'r', {
  get: function () {
    return 7;
  },
});
readonly.r = 9;
console.log(readonly.r);
try {
  (function () {
    'use strict';
    readonly.r = 9;
  })();
} catch (error) {
  console.log(error.name + ': ' + error.message);
}

// An object with no prototype is named differently in the same message, the same way it is for a
// read only data property.
try {
  (function () {
    'use strict';
    var bare = Object.create(null);
    Object.defineProperty(bare, 'r', {
      get: function () {
        return 1;
      },
    });
    bare.r = 2;
  })();
} catch (error) {
  console.log(error.message);
}

// Reading something that has only a setter answers undefined rather than throwing.
var writeonly = {};
Object.defineProperty(writeonly, 'w', {
  set: function (v) {
    this.saw = v;
  },
});
writeonly.w = 3;
console.log(writeonly.w, writeonly.saw);

// The redefinition rules. A configurable accessor can become a data property, and the value it
// becomes is not writable unless the descriptor said so.
var turned = {};
Object.defineProperty(turned, 't', {
  get: function () {
    return 1;
  },
  enumerable: true,
  configurable: true,
});
Object.defineProperty(turned, 't', { value: 5 });
console.log(Object.getOwnPropertyDescriptor(turned, 't'));

// And a data property can become an accessor the same way.
var flipped = { f: 1 };
Object.defineProperty(flipped, 'f', {
  get: function () {
    return 2;
  },
});
console.log(flipped.f, Object.getOwnPropertyDescriptor(flipped, 'f').set);

// A non configurable accessor refuses every direction that asks for a change.
var frozen = {};
var half = function () {
  return 1;
};
Object.defineProperty(frozen, 'p', { get: half });
try {
  Object.defineProperty(frozen, 'p', { value: 1 });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(frozen, 'p', {
    get: function () {
      return 2;
    },
  });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(frozen, 'p', { set: function () {} });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(frozen, 'p', { configurable: true });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty(frozen, 'p', { enumerable: true });
} catch (error) {
  console.log(error.message);
}
// Asking for what is already true is not a change, even here.
Object.defineProperty(frozen, 'p', { get: half });
Object.defineProperty(frozen, 'p', { enumerable: false, configurable: false });
console.log(frozen.p);

// A descriptor cannot ask for both kinds at once, whichever way round it is written.
try {
  Object.defineProperty({}, 'x', { get: function () {}, writable: true });
} catch (error) {
  console.log(error.message);
}
try {
  Object.defineProperty({}, 'x', { set: function () {}, value: 1 });
} catch (error) {
  console.log(error.message);
}

// The other places a property is visible. A hidden accessor stays out of all of them.
var enumerated = {};
Object.defineProperty(enumerated, 'seen', {
  get: function () {
    return 1;
  },
  enumerable: true,
});
Object.defineProperty(enumerated, 'unseen', {
  get: function () {
    return 2;
  },
});
console.log(enumerated, enumerated.seen, enumerated.unseen);

// A getter that throws throws from the read, at the point of the read, and the print never runs.
var angry = {};
Object.defineProperty(angry, 'boom', {
  get: function () {
    throw 'from the getter';
  },
});
try {
  console.log(angry.boom);
} catch (error) {
  console.log(error);
}

// A setter that throws throws from the write.
var refuses = {};
Object.defineProperty(refuses, 'boom', {
  set: function () {
    throw 'from the setter';
  },
});
try {
  refuses.boom = 1;
} catch (error) {
  console.log(error);
}

// The value of a write is the value assigned and not what the setter returned, which is why a
// chained assignment does not see the setter's answer.
var discards = {};
Object.defineProperty(discards, 'd', {
  set: function () {
    return 'ignored';
  },
});
var got = (discards.d = 5);
console.log(got);

// Each read is a fresh call, so a getter with state answers differently every time.
var counted = { n: 0 };
Object.defineProperty(counted, 'next', {
  get: function () {
    this.n = this.n + 1;
    return this.n;
  },
});
console.log(counted.next, counted.next, counted.next, counted.n);

// An accessor is a property like any other, so the shape rules still hold. Order is where the name
// was first written, and turning a data property into an accessor does not move it.
var ordered = { a: 1, b: 2, c: 3 };
Object.defineProperty(ordered, 'b', {
  get: function () {
    return 'two';
  },
  enumerable: true,
  configurable: true,
});
console.log(ordered, ordered.b);
Object.defineProperty(ordered, 'b', { value: 2, enumerable: true });
console.log(ordered);
