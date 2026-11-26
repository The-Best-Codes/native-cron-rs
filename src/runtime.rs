use std::env;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::process::SystemCommandRunner;

#[cfg(target_os = "macos")]
use crate::drivers::darwin::{DarwinContext, DarwinDriver};
#[cfg(target_os = "linux")]
use crate::drivers::linux::{LinuxContext, LinuxDriver};
#[cfg(target_os = "windows")]
use crate::drivers::windows::{WindowsContext, WindowsDriver};

/// Folder set by [`crate::set_windows_root`], used instead of
/// `%LOCALAPPDATA%\native-cron` when present.
static WINDOWS_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

pub(crate) fn set_windows_root(root: PathBuf) {
    *WINDOWS_ROOT
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(root);
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_root_override() -> Option<PathBuf> {
    WINDOWS_ROOT
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

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
fn windows_logon_user() -> Option<String> {
    let user = env::var("USERNAME").ok().filter(|user| !user.is_empty())?;
    Some(match env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    })
}

#[cfg(target_os = "windows")]
fn build_driver(namespace: &str) -> Result<Box<dyn Driver>> {
    let root = match windows_root_override() {
        Some(root) => root,
        None => match env::var_os("LOCALAPPDATA") {
            Some(value) if PathBuf::from(&value).is_absolute() => PathBuf::from(value),
            _ => home_dir()?.join("AppData").join("Local"),
        }
        .join("native-cron"),
    };

    Ok(Box::new(WindowsDriver::with_namespace(
        WindowsContext {
            root,
            runner: Box::new(SystemCommandRunner),
            logon_user: windows_logon_user(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_the_windows_root_override() {
        let root = env::temp_dir().join("native-cron-root-test");
        set_windows_root(root.clone());
        assert_eq!(windows_root_override(), Some(root));
    }
}
