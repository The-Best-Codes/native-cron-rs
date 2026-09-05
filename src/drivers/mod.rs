#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod darwin;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub mod linux;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod windows;
