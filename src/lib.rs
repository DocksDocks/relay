#[cfg(not(target_os = "linux"))]
compile_error!(
    "session-relay supports Linux only: managed workspace custody requires cgroup v2, pidfd, Landlock and seccomp"
);

pub(crate) mod appserver;
pub mod bus;
pub mod channel;
pub mod cli;
pub mod discover;
pub mod fanout;
mod gc;
pub mod hook;
pub mod lifecycle;
pub mod protocol;
pub(crate) mod sha256;
pub mod spawn;
pub mod store;
pub mod supervisor;
pub mod watch;
pub mod workspace;
