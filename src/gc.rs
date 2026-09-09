//! Garbage collection for legacy session state.

use crate::store::LegacyGc;
use std::time::SystemTime;

pub(crate) fn run(now: SystemTime, self_id: Option<&str>) -> Result<usize, String> {
    let Some(legacy) = LegacyGc::prepare()? else {
        return Ok(0);
    };
    if legacy.preflight_throttled(now)? {
        return Ok(0);
    }
    legacy.collect(now, self_id)
}
