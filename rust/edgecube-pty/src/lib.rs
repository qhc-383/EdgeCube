//! EdgeCube PTY 桥。

#[cfg(unix)]
pub mod pty;

#[cfg(unix)]
pub mod bridge;
pub mod broadcast;
pub mod frame;
#[cfg(unix)]
pub mod lifecycle;
#[cfg(unix)]
pub mod run;
#[cfg(unix)]
pub mod session;

#[cfg(unix)]
#[unsafe(no_mangle)]
pub extern "C" fn edgecube_pty_smoke_probe() -> i32 {
    match pty::smoke_probe() {
        Ok(_) => 0,
        Err(_) => -1,
    }
}
