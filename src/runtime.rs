use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::process::SystemCommandRunner;

#[cfg(target_os = "macos")]
use crate::drivers::darwin::{DarwinContext, DarwinDriver};
#[cfg(target_os = "linux")]
use crate::drivers::linux::{LinuxContext, LinuxDriver};
#[cfg(target_os = "windows")]
use crate::drivers::windows::{WindowsContext, WindowsDriver};

fn home_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .ok_or(Error::UnsupportedPlatform)
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(Error::UnsupportedPlatform)
    }
}

#[cfg(target_os = "macos")]
fn build_driver(namespace: &str) -> Result<Box<dyn Driver>> {
    let home = home_dir()?;
    let uid = unsafe { libc::getuid() };
    Ok(Box::new(DarwinDriver::with_namespace(
        DarwinContext {
            home,
            uid,
            runner: Box::new(SystemCommandRunner),
        },
        namespace,
    )))
}

#[cfg(target_os = "linux")]
fn build_driver(namespace: &str) -> Result<Box<dyn Driver>> {
    let config_root = match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
        _ => home_dir()?.join(".config"),
    };
    Ok(Box::new(LinuxDriver::with_namespace(
        LinuxContext {
            config_root,
            runner: Box::new(SystemCommandRunner),
        },
        namespace,
    )))
}

#[cfg(target_os = "windows")]
fn build_driver(namespace: &str) -> Result<Box<dyn Driver>> {
    let root = match env::var_os("LOCALAPPDATA") {
        Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
        _ => home_dir()?.join("AppData").join("Local"),
    }
    .join("native-cron");

    Ok(Box::new(WindowsDriver::with_namespace(
        WindowsContext {
            root,
            runner: Box::new(SystemCommandRunner),
        },
        namespace,
    )))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn build_driver(_namespace: &str) -> Result<Box<dyn Driver>> {
    Err(Error::UnsupportedPlatform)
}

/// Returns a driver for the current operating system whose native names
/// start with `namespace`. Drivers are cheap to build, so this isn't cached.
pub fn driver(namespace: &str) -> Result<Arc<dyn Driver>> {
    Ok(Arc::from(build_driver(namespace)?))
}
