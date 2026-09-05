//! Cross-platform OS-level cron scheduling for Rust.
//!
//! No daemon, timers, or resident process: `native-cron` registers commands
//! with the scheduler already built into the operating system (launchd on
//! macOS, systemd user timers on Linux, Task Scheduler on Windows) and exits.
//! The OS starts a fresh process when the schedule fires.
//!
//! ```no_run
//! use native_cron::CronOptions;
//!
//! # fn main() -> native_cron::Result<()> {
//! let job = native_cron::register(CronOptions::new(
//!     "backup",
//!     "0 2 * * *",
//!     ["/usr/bin/backup"],
//! ))?;
//!
//! println!("{:?}", job.status()?.state);
//! # Ok(())
//! # }
//! ```
//!
//! Registering a job whose id already exists returns
//! [`Error::AlreadyExists`] unless [`CronOptions::overwrite`] is set, in
//! which case the existing job is replaced and restarted.
//!
//! Lifecycle operations use standard OS terminology: [`Job::enable`],
//! [`Job::disable`], and [`Job::remove`]. [`Job::status`] reports the native
//! state plus a copy of the configuration the job was registered with.

mod driver;
mod drivers;
mod error;
mod escape;
mod files;
mod normalize;
mod process;
mod runtime;
mod schedule;
#[cfg(test)]
mod test_support;
mod types;

pub use error::{Error, Result};
pub use process::{CommandRunner, ProcessOutput};
pub use schedule::{CalendarSchedule, CronField, Schedule};
pub use types::{CronOptions, JobState, JobStatus, Platform};

use std::sync::Arc;

use driver::Driver;
use normalize::{normalize, NormalizedJob};

/// A handle to a registered job, returned by [`register`].
///
/// The handle remembers the configuration it was registered with (for
/// [`Job::status`]) and forwards lifecycle operations to the native driver
/// for the current platform. A handle obtained from [`job`] instead of
/// [`register`] does not know its configuration, so [`Job::status`] omits it.
#[derive(Clone)]
pub struct Job {
    driver: Arc<dyn Driver>,
    id: String,
    normalized: Option<NormalizedJob>,
}

impl Job {
    /// The job's id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Enables a previously disabled job, or does nothing if it is already enabled.
    pub fn enable(&self) -> Result<()> {
        self.driver.enable(&self.id)
    }

    /// Disables the job while preserving its native configuration.
    pub fn disable(&self) -> Result<()> {
        self.driver.disable(&self.id)
    }

    /// Removes the job's native configuration entirely. Idempotent.
    pub fn remove(&self) -> Result<()> {
        self.driver.remove(&self.id)
    }

    /// Reports the job's current native state, enriched with the
    /// configuration this handle was registered with (if any).
    pub fn status(&self) -> Result<JobStatus> {
        let mut status = self.driver.status(&self.id)?;
        if let Some(normalized) = &self.normalized {
            status.cron = match &normalized.schedule {
                schedule::Schedule::Calendar(calendar) => Some(calendar.normalized.clone()),
                schedule::Schedule::Startup => None,
            };
            status.run_at_startup = matches!(normalized.schedule, schedule::Schedule::Startup);
            status.command = Some(normalized.command.clone());
            status.cwd = normalized.cwd.clone();
            status.env = if normalized.env.is_empty() {
                None
            } else {
                Some(normalized.env.clone())
            };
            status.stdout = normalized.stdout.clone();
            status.stderr = normalized.stderr.clone();
        }
        Ok(status)
    }
}

/// Registers a job with the operating system's native scheduler.
///
/// This is an upsert only when [`CronOptions::overwrite`] is set; otherwise
/// registering an id that already exists returns [`Error::AlreadyExists`].
/// When overwriting, the job's native configuration is replaced and
/// restarted with the new schedule and command.
///
/// The executable in `command` is resolved to an absolute path during
/// registration (via `PATH` if it is a bare name). Relative `cwd`, `stdout`,
/// and `stderr` paths are resolved against the working directory in effect
/// at registration time (or the explicit `cwd`, if supplied). Output
/// directories are created automatically.
pub fn register(options: CronOptions) -> Result<Job> {
    let driver = runtime::driver()?;
    let normalized = normalize(options)?;
    driver.register(&normalized)?;
    Ok(Job {
        driver,
        id: normalized.id.clone(),
        normalized: Some(normalized),
    })
}

/// Retrieves a handle to a job registered in another process, by id.
///
/// Unlike the handle returned by [`register`], this handle does not know the
/// job's schedule or command, so [`Job::status`] will report `None` for
/// those fields. Use [`register`] with `overwrite: true` if you need to
/// inspect or change the configuration.
pub fn job(id: impl Into<String>) -> Result<Job> {
    let id = id.into();
    normalize::validate_id(&id)?;
    let driver = runtime::driver()?;
    Ok(Job {
        driver,
        id,
        normalized: None,
    })
}

/// Removes a job by id without needing a [`Job`] handle first. Idempotent.
pub fn remove(id: impl AsRef<str>) -> Result<()> {
    let id = id.as_ref();
    normalize::validate_id(id)?;
    runtime::driver()?.remove(id)
}

/// Validates that `options` can be registered, without writing any native
/// configuration or calling any native command. Useful for fast-failing a
/// batch of registrations before committing to any of them.
pub fn validate(options: CronOptions) -> Result<()> {
    let driver = runtime::driver()?;
    let normalized = normalize(options)?;
    driver.preflight(&normalized)
}
