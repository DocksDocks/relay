#[cfg(target_os = "linux")]
pub mod linux;
pub mod macos;

pub const LINUX_BACKEND: &str = "linux_cgroup_v2_pidfd";
pub const MACOS_INADMISSIBLE_BACKEND: &str = "macos_pgroup_libproc";
pub const MACOS_STOP_REASON: &str = macos::STOP_REASON;

pub fn admit_macos_writable_custody_for_test() -> Result<PlatformAdmission, String> {
    macos::admit()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformAdmission { pub backend: &'static str, }

pub fn admit_writable_custody() -> Result<PlatformAdmission, String> { #[cfg(target_os = "linux")]
{
    linux::admit()?;
    Ok(PlatformAdmission { backend: LINUX_BACKEND })
}
#[cfg(target_os = "macos")]
{
    macos::admit()
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
{
    Err("managed workspace custody is supported only by the admitted Linux cgroup-v2/pidfd backend".to_string())
} }
