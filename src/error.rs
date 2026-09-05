use std::path::PathBuf;

use thiserror::Error;

/// The error type returned by all fallible `native-cron` operations.
#[derive(Debug, Error)]
pub enum Error {
    /// The job id is empty, too long, or contains disallowed characters.
    #[error("cron job id must be 1-100 characters containing only letters, numbers, hyphens, and underscores")]
    InvalidId,

    /// The cron expression could not be parsed.
    #[error("invalid cron expression: {0}")]
    InvalidCronExpression(String),

    /// The command list was empty.
    #[error("cron command must be a non-empty list of arguments")]
    EmptyCommand,

    /// A string field was empty or contained a newline or NUL.
    #[error("{label} must be a non-empty single-line string")]
    InvalidText {
        /// Which field failed validation.
        label: &'static str,
    },

    /// An environment variable name was not a valid identifier.
    #[error("invalid environment variable name: {0}")]
    InvalidEnvName(String),

    /// The working directory does not exist.
    #[error("cron working directory does not exist: {0}")]
    MissingCwd(PathBuf),

    /// The executable path does not exist or is not a file.
    #[error("cron executable does not exist: {0}")]
    MissingExecutable(PathBuf),

    /// The executable has no execute bit (Unix).
    #[error("cron executable is not executable: {0}")]
    NotExecutable(PathBuf),

    /// A bare executable name was not found in `PATH`.
    #[error("cannot resolve executable '{0}' from PATH; use an absolute path")]
    ExecutableNotFound(String),

    /// Both `cron` and `run_at_startup` were set.
    #[error("either `cron` or `run_at_startup` must be set, but not both")]
    AmbiguousTrigger,

    /// Neither `cron` nor `run_at_startup` was set.
    #[error("either `cron` or `run_at_startup` must be set")]
    MissingTrigger,

    /// A job with this id is already registered and `overwrite` was not set.
    #[error("a cron job with id '{0}' is already registered; pass overwrite: true to replace it")]
    AlreadyExists(String),

    /// `enable` or `disable` was called for an id that is not registered.
    #[error("cron job '{0}' is not registered")]
    NotRegistered(String),

    /// The expression expands to more launchd calendar entries than allowed.
    #[error("cron expression expands to {0} launchd intervals; simplify it")]
    TooManyIntervals(usize),

    /// The expression needs more Task Scheduler triggers than Windows allows.
    #[error(
        "cron expression requires {0} Windows Task Scheduler triggers; the platform limit is 48"
    )]
    TooManyWindowsTriggers(usize),

    /// The Windows user identity could not be determined.
    #[error("cannot register a Windows job without the current user identity")]
    MissingUserId,

    /// The operating system is not macOS, Linux, or Windows.
    #[error("unsupported operating system")]
    UnsupportedPlatform,

    /// A native command (`launchctl`, `systemctl`, `schtasks`) exited non-zero.
    #[error("{command} failed: {detail}")]
    CommandFailed {
        /// The command line that failed.
        command: String,
        /// stderr, stdout, or the exit code.
        detail: String,
    },

    /// A native command could not be started.
    #[error("unable to run {command}: {source}")]
    Spawn {
        /// The program that could not be spawned.
        command: String,
        #[source]
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// A filesystem error while reading or writing configuration.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Failed to encode a launchd plist.
    #[error("failed to encode plist: {0}")]
    Plist(#[from] plist::Error),

    /// Failed to build or parse a systemd unit.
    #[error("failed to parse systemd unit: {0}")]
    SystemdUnit(String),

    /// Failed to encode Task Scheduler XML.
    #[error("failed to encode xml: {0}")]
    Xml(#[from] quick_xml::Error),
}

/// A `Result` alias using [`enum@Error`] as the error type.
pub type Result<T> = std::result::Result<T, Error>;
