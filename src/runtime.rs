use std::env;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

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
fn build_driver() -> Result<Box<dyn Driver>> {
    let home = home_dir()?;
    let uid = unsafe { libc::getuid() };
    Ok(Box::new(DarwinDriver::new(DarwinContext {
        home,
        uid,
        runner: Box::new(SystemCommandRunner),
    })))
}

#[cfg(target_os = "linux")]
fn build_driver() -> Result<Box<dyn Driver>> {
    let config_root = match env::var_os("XDG_CONFIG_HOME") {
        Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
        _ => home_dir()?.join(".config"),
    };
    Ok(Box::new(LinuxDriver::new(LinuxContext {
        config_root,
        runner: Box::new(SystemCommandRunner),
    })))
}

#[cfg(target_os = "windows")]
fn build_driver() -> Result<Box<dyn Driver>> {
    let root = match env::var_os("LOCALAPPDATA") {
        Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
        _ => home_dir()?.join("AppData").join("Local"),
    }
    .join("native-cron");

    Ok(Box::new(WindowsDriver::new(WindowsContext {
        root,
        runner: Box::new(SystemCommandRunner),
    })))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn build_driver() -> Result<Box<dyn Driver>> {
    Err(Error::UnsupportedPlatform)
}

static DRIVER: OnceLock<Arc<dyn Driver>> = OnceLock::new();

/// Returns the process-wide driver for the current operating system,
/// building it on first use.
pub fn driver() -> Result<Arc<dyn Driver>> {
    if let Some(driver) = DRIVER.get() {
        return Ok(driver.clone());
    }
    let built: Arc<dyn Driver> = Arc::from(build_driver()?);
    Ok(DRIVER.get_or_init(|| built).clone())
}
