pub mod linux;

pub const LINUX_BACKEND: &str = "linux_cgroup_v2_pidfd";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformAdmission {
    pub backend: &'static str,
}

pub fn admit_writable_custody() -> Result<PlatformAdmission, String> {
    linux::admit()?;
    Ok(PlatformAdmission {
        backend: LINUX_BACKEND,
    })
}
