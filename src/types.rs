use std::collections::HashMap;
use std::path::PathBuf;

/// The operating system a driver targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Darwin,
    Linux,
    Windows,
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Platform::Darwin => "darwin",
            Platform::Linux => "linux",
            Platform::Windows => "windows",
        })
    }
}

/// The native lifecycle state of a registered job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobState {
    /// Registered and enabled with the native scheduler.
    Active,
    /// Registered but disabled.
    Inactive,
    /// Not registered.
    Missing,
}

/// Options used to register a job with [`crate::register`].
///
/// Only `id` and `command` are required, along with exactly one of `cron` or
/// `run_at_startup`. Everything else is optional and only becomes part of the
/// native scheduler configuration when supplied.
#[derive(Debug, Clone, Default)]
pub struct CronOptions {
    /// A unique identifier for this job. Must be 1-100 characters of letters,
    /// numbers, hyphens, and underscores.
    pub id: String,
    /// The command to run: the first element is the executable, the rest are
    /// its arguments.
    pub command: Vec<String>,
    /// A standard five-field cron expression (`minute hour day month weekday`),
    /// or a supported nickname such as `@daily`. Mutually exclusive with
    /// `run_at_startup`.
    pub cron: Option<String>,
    /// Run the job when the user's native scheduler starts (or when the job
    /// is enabled), instead of on a calendar schedule. Mutually exclusive
    /// with `cron`.
    pub run_at_startup: bool,
    /// The working directory the command runs from. Defaults to the current
    /// directory at registration time.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables to set for the command. Only variables
    /// supplied here are added explicitly; the command otherwise receives the
    /// base environment provided by the native scheduler.
    pub env: Option<HashMap<String, String>>,
    /// A file path to redirect standard output to.
    pub stdout: Option<PathBuf>,
    /// A file path to redirect standard error to.
    pub stderr: Option<PathBuf>,
    /// If a job with the same `id` is already registered, replace its
    /// configuration instead of returning an error. The job is restarted
    /// with the new configuration.
    pub overwrite: bool,
}

impl CronOptions {
    /// Creates options for a calendar-scheduled job.
    pub fn new(
        id: impl Into<String>,
        cron: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into_iter().map(Into::into).collect(),
            cron: Some(cron.into()),
            ..Default::default()
        }
    }

    /// Creates options for a job that runs at user-session startup.
    pub fn at_startup(
        id: impl Into<String>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            command: command.into_iter().map(Into::into).collect(),
            run_at_startup: true,
            ..Default::default()
        }
    }

    /// Sets the working directory.
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Sets an environment variable, merging with any already set.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env
            .get_or_insert_with(HashMap::new)
            .insert(key.into(), value.into());
        self
    }

    /// Sets the standard output redirection path.
    pub fn stdout(mut self, path: impl Into<PathBuf>) -> Self {
        self.stdout = Some(path.into());
        self
    }

    /// Sets the standard error redirection path.
    pub fn stderr(mut self, path: impl Into<PathBuf>) -> Self {
        self.stderr = Some(path.into());
        self
    }

    /// Allows this registration to replace an existing job with the same id.
    pub fn overwrite(mut self, overwrite: bool) -> Self {
        self.overwrite = overwrite;
        self
    }
}

/// The current status of a registered job, including a copy of the
/// configuration it was registered with (when known).
#[derive(Debug, Clone)]
pub struct JobStatus {
    pub id: String,
    pub platform: Platform,
    pub state: JobState,
    /// Paths to the native configuration files backing this job.
    pub config_paths: Vec<PathBuf>,
    /// The normalized cron expression, if this handle knows its configuration.
    pub cron: Option<String>,
    /// Whether this job runs at startup rather than on a calendar schedule.
    pub run_at_startup: bool,
    /// The resolved command, if this handle knows its configuration.
    pub command: Option<Vec<String>>,
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
}
