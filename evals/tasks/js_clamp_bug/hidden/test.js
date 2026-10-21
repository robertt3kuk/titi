const assert = require("assert");
const { clamp } = require("./clamp");

assert.strictEqual(clamp(5, 0, 10), 5);
assert.strictEqual(clamp(-3, 0, 10), 0);
assert.strictEqual(clamp(42, 0, 10), 10);
assert.strictEqual(clamp(10, 0, 10), 10);
console.log("ok");
