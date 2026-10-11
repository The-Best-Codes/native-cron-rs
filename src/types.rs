use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

/// The operating system a driver targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// macOS (launchd).
    Darwin,
    /// Linux (systemd user timers).
    Linux,
    /// Windows (Task Scheduler).
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

/// Windows-specific options for [`CronOptions`]. Has no effect on other
/// platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowsOptions {
    /// Runs the job with Task Scheduler's `InteractiveToken` logon type
    /// instead of the default `S4U` logon type.
    ///
    /// Task Scheduler always runs `S4U` jobs in a non-interactive session,
    /// even while the target user is logged on interactively, so any window
    /// the command creates can never be shown. Setting `visible` to `true`
    /// runs the job in the user's interactive session instead, so its
    /// windows are visible, at the cost of the job not running at all while
    /// that user isn't logged on (unlike `S4U`, which runs regardless of
    /// logon state).
    ///
    /// Visible jobs whose command is an `.exe` and that don't set `env`,
    /// `stdout`, or `stderr` are started by Task Scheduler directly, so no
    /// PowerShell console appears. Otherwise the PowerShell wrapper is still
    /// used, with its window hidden.
    pub visible: bool,
}

/// macOS-specific options for [`CronOptions`]. Has no effect on other
/// platforms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MacOSOptions {
    /// Bundle identifiers of the apps this job belongs to, written as the
    /// launchd `AssociatedBundleIdentifiers` key. On macOS 13 and later,
    /// System Settings > Login Items and the "Background Items Added"
    /// notification then show the job under that app's name and icon,
    /// instead of the name of the executable it runs.
    pub associated_bundle_identifiers: Vec<String>,
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
    /// Windows-specific options. Has no effect on other platforms.
    pub windows: WindowsOptions,
    /// macOS-specific options. Has no effect on other platforms.
    pub macos: MacOSOptions,
    /// The prefix for this job's native names, so jobs from different apps
    /// can't collide. Defaults to `native-cron`, giving
    /// `native-cron.<id>` on macOS and `native-cron-<id>` elsewhere. A
    /// reverse-DNS name such as `com.example.app` is a good choice.
    pub namespace: Option<String>,
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

    /// Creates options for a calendar-scheduled job that opens `url` (a web
    /// page, a file, or an app's custom URL scheme such as a deep link) with
    /// the user's default handler, in the user's graphical session.
    ///
    /// The command is chosen per platform so that the opened app outlives the
    /// job and no console window appears:
    ///
    /// - macOS: `/usr/bin/open <url>`.
    /// - Linux: `systemd-run --user --scope --quiet -- xdg-open <url>`, so the
    ///   opened app gets its own scope instead of being killed when the job's
    ///   oneshot service exits.
    /// - Windows: `rundll32.exe url.dll,FileProtocolHandler <url>` with
    ///   [`WindowsOptions::visible`] set, since the URL has to open in the
    ///   user's interactive session.
    pub fn open_url(
        id: impl Into<String>,
        cron: impl Into<String>,
        url: impl Into<String>,
    ) -> Self {
        Self {
            windows: WindowsOptions { visible: true },
            ..Self::new(id, cron, open_url_command(current_platform(), url.into()))
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

    /// Sets Windows-specific options. Has no effect on other platforms.
    pub fn windows(mut self, windows: WindowsOptions) -> Self {
        self.windows = windows;
        self
    }

    /// Sets macOS-specific options. Has no effect on other platforms.
    pub fn macos(mut self, macos: MacOSOptions) -> Self {
        self.macos = macos;
        self
    }

    /// Sets the namespace this job's native names start with. Use the same
    /// namespace with [`crate::job_in`] and [`crate::remove_in`] to manage
    /// it later.
    pub fn namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }
}

fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Darwin
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Linux
    }
}

fn open_url_command(platform: Platform, url: String) -> Vec<String> {
    let prefix: &[&str] = match platform {
        Platform::Darwin => &["/usr/bin/open"],
        Platform::Linux => &[
            "systemd-run",
            "--user",
            "--scope",
            "--quiet",
            "--",
            "xdg-open",
        ],
        Platform::Windows => &["rundll32.exe", "url.dll,FileProtocolHandler"],
    };
    prefix
        .iter()
        .map(|arg| arg.to_string())
        .chain(std::iter::once(url))
        .collect()
}

/// The current status of a registered job, including a copy of the
/// configuration it was registered with (when known).
#[derive(Debug, Clone)]
pub struct JobStatus {
    /// The job id.
    pub id: String,
    /// The backend that owns this job.
    pub platform: Platform,
    /// Whether the job is enabled, disabled, or missing.
    pub state: JobState,
    /// Paths to the native configuration files backing this job.
    pub config_paths: Vec<PathBuf>,
    /// The normalized cron expression, if this handle knows its configuration.
    pub cron: Option<String>,
    /// Whether this job runs at startup rather than on a calendar schedule.
    pub run_at_startup: bool,
    /// The resolved command, if this handle knows its configuration.
    pub command: Option<Vec<String>>,
    /// Working directory, if this handle knows its configuration.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables, if this handle knows its configuration.
    pub env: Option<HashMap<String, String>>,
    /// Standard output path, if this handle knows its configuration.
    pub stdout: Option<PathBuf>,
    /// Standard error path, if this handle knows its configuration.
    pub stderr: Option<PathBuf>,
    /// When the job last started, if the native scheduler reports it.
    /// Always `None` on macOS, where launchd doesn't record it.
    pub last_run: Option<SystemTime>,
    /// The exit code of the last finished run, if the native scheduler
    /// reports it.
    pub last_exit_code: Option<i32>,
    /// When the job will next run, if the native scheduler reports it.
    /// `None` for disabled and startup jobs, and always `None` on macOS,
    /// where launchd doesn't expose it.
    pub next_run: Option<SystemTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_urls_with_each_platforms_default_handler() {
        let url = "agent-one://cron?v=1&id=x";
        assert_eq!(
            open_url_command(Platform::Darwin, url.to_string()),
            ["/usr/bin/open", url]
        );
        assert_eq!(
            open_url_command(Platform::Linux, url.to_string()),
            [
                "systemd-run",
                "--user",
                "--scope",
                "--quiet",
                "--",
                "xdg-open",
                url
            ]
        );
        assert_eq!(
            open_url_command(Platform::Windows, url.to_string()),
            ["rundll32.exe", "url.dll,FileProtocolHandler", url]
        );
    }

    #[test]
    fn open_url_jobs_are_visible_on_windows() {
        let options = CronOptions::open_url("open", "0 9 * * *", "https://example.com");
        assert!(options.windows.visible);
        assert_eq!(options.cron.as_deref(), Some("0 9 * * *"));
        assert_eq!(
            options.command.last().map(String::as_str),
            Some("https://example.com")
        );
    }
}
