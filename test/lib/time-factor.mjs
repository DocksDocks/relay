// Wall-clock slack for poll-wait ceilings. Every timeout in these tests is a safety net against a
// hung process, never a measurement of how fast the product must be, so on a loaded box the only
// honest response to a near-miss is a wider net. The factor exists so a saturated CI host (or a
// bisect run under heavy parallelism) can widen every net at once without editing the literals,
// which stay readable as the intended idle-machine budget.
//
// Deliberately NOT applied to durations that are themselves the assertion - ordering windows,
// two-sided ranges, product-configured budgets. Only helper bodies that compute a give-up deadline
// consult this.
const VARIABLE = 'SESSION_RELAY_TEST_TIME_FACTOR';
const MAX_FACTOR = 100;

function readFactor() {
  const raw = process.env[VARIABLE];
  if (raw === undefined || raw === '') return 1;
  // A malformed factor must never degrade to 1: a typo would silently reinstate the tight ceilings
  // the operator was trying to widen, and the resulting timeout would be read as a product flake.
  if (!/^[1-9][0-9]*$/.test(raw) || Number(raw) > MAX_FACTOR)
    throw new Error(`${VARIABLE} must be an integer 1..=${MAX_FACTOR}; got ${JSON.stringify(raw)}`);
  return Number(raw);
}

// Read once at module load. A 20ms poll loop must not re-enter the environment on every iteration,
// and a factor that changed mid-run would make two deadlines in one test incomparable.
export const TIME_FACTOR = readFactor();

export function scaledTimeout(baseMs) {
  return baseMs * TIME_FACTOR;
}
