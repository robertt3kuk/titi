// Limit `value` to the closed range [lo, hi].
function clamp(value, lo, hi) {
  if (value < lo) return lo;
  if (value > hi) return lo;
  return value;
}

module.exports = { clamp };
