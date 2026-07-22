use super::PlatformAdmission;

pub const STOP_REASON: &str = "process groups are escapable, kqueue is PID observation rather than durable containment, and no documented public primitive provides crash-durable descendant membership plus atomic kill/empty proof";

pub fn admit() -> Result<PlatformAdmission, String> { Err(STOP_REASON.to_string()) }

pub fn live_evidence() -> Result<std::convert::Infallible, String> { Err(STOP_REASON.to_string()) }
