use std::path::PathBuf;

use thiserror::Error;

/// The error type returned by all fallible `native-cron` operations.
#[derive(Debug, Error)]
pub enum Error {
    #[error("cron job id must be 1-100 characters containing only letters, numbers, hyphens, and underscores")]
    InvalidId,

    #[error("invalid cron expression: {0}")]
    InvalidCronExpression(String),

    #[error("cron command must be a non-empty list of arguments")]
    EmptyCommand,

    #[error("{label} must be a non-empty single-line string")]
    InvalidText { label: &'static str },

    #[error("invalid environment variable name: {0}")]
    InvalidEnvName(String),

    #[error("cron working directory does not exist: {0}")]
    MissingCwd(PathBuf),

    #[error("cron executable does not exist: {0}")]
    MissingExecutable(PathBuf),

    #[error("cron executable is not executable: {0}")]
    NotExecutable(PathBuf),

    #[error("cannot resolve executable '{0}' from PATH; use an absolute path")]
    ExecutableNotFound(String),

    #[error("either `cron` or `run_at_startup` must be set, but not both")]
    AmbiguousTrigger,

    #[error("either `cron` or `run_at_startup` must be set")]
    MissingTrigger,

    #[error("a cron job with id '{0}' is already registered; pass overwrite: true to replace it")]
    AlreadyExists(String),

    #[error("cron job '{0}' is not registered")]
    NotRegistered(String),

    #[error("cron expression expands to {0} launchd intervals; simplify it")]
    TooManyIntervals(usize),

    #[error(
        "cron expression requires {0} Windows Task Scheduler triggers; the platform limit is 48"
    )]
    TooManyWindowsTriggers(usize),

    #[error("cannot register a Windows job without the current user identity")]
    MissingUserId,

    #[error("unsupported operating system")]
    UnsupportedPlatform,

    #[error("{command} failed: {detail}")]
    CommandFailed { command: String, detail: String },

    #[error("unable to run {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("failed to encode plist: {0}")]
    Plist(#[from] plist::Error),

    #[error("failed to parse systemd unit: {0}")]
    SystemdUnit(String),

    #[error("failed to encode xml: {0}")]
    Xml(#[from] quick_xml::Error),
}

/// A `Result` alias using [`enum@Error`] as the error type.
pub type Result<T> = std::result::Result<T, Error>;
