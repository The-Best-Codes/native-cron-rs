use crate::error::Error;
use crate::error::Result;
use crate::normalize::NormalizedJob;
use crate::types::{JobState, JobStatus};

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

    /// Reports the current native state of a job.
    fn status(&self, id: &str) -> Result<JobStatus>;
}
