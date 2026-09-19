#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(not(target_os = "linux"))]
compile_error!("desktop-rs currently supports Linux only");
