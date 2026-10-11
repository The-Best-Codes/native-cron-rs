use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::Error;
use crate::error::Result;
use crate::normalize::NormalizedJob;
use crate::types::{JobState, JobStatus};

/// Run history reported by [`Driver::run_info`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunInfo {
    pub last_run: Option<SystemTime>,
    pub last_exit_code: Option<i32>,
    pub next_run: Option<SystemTime>,
}

/// Converts whole seconds since the Unix epoch to a [`SystemTime`].
pub fn unix_seconds(seconds: i64) -> Option<SystemTime> {
    let seconds = u64::try_from(seconds).ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

/// Checks that a job can be run on demand: registered and enabled.
pub fn ensure_runnable(status: &JobStatus) -> Result<()> {
    match status.state {
        JobState::Active => Ok(()),
        JobState::Inactive => Err(Error::Disabled(status.id.clone())),
        JobState::Missing => Err(Error::NotRegistered(status.id.clone())),
    }
}

/// The operations every platform backend (launchd, systemd, Task Scheduler)
/// must implement.
pub trait Driver: Send + Sync {
    /// Validates that `job` can be rendered into native configuration,
    /// without writing anything or calling any native command. Used to
    /// fail fast before registering a batch of jobs.
    fn preflight(&self, job: &NormalizedJob) -> Result<()>;

    /// Writes native configuration for `job` and enables it. If a job with
    /// the same id is already registered, `job.overwrite` determines whether
    /// this replaces it or returns [`crate::Error::AlreadyExists`].
    fn register(&self, job: &NormalizedJob) -> Result<()>;

    /// Enables a previously registered, disabled job.
    fn enable(&self, id: &str) -> Result<()>;

    /// Disables a job while preserving its configuration. Does nothing if
    /// the job is not registered.
    fn disable(&self, id: &str) -> Result<()>;

    /// Removes a job's configuration entirely. Idempotent.
    fn remove(&self, id: &str) -> Result<()>;

    /// Starts one run of an enabled job right away, through the native
    /// scheduler, without changing its schedule.
    fn run_now(&self, id: &str) -> Result<()>;

    /// Reports when a registered job last ran, how that run exited, and when
    /// it runs next, as far as the native scheduler reports them. Best
    /// effort: anything that can't be determined is left `None`.
    fn run_info(&self, id: &str) -> RunInfo;

    /// Reports the current native state of a job.
    fn status(&self, id: &str) -> Result<JobStatus>;
}
