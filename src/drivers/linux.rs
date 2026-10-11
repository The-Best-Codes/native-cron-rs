use std::path::PathBuf;
use std::str::FromStr;

use systemd_unit_edit::{Section, SystemdUnit};

use crate::driver::{ensure_runnable, unix_seconds, Driver, RunInfo};
use crate::error::{Error, Result};
use crate::escape::{systemd_exec_quote, systemd_quote};
use crate::files::{atomic_write, ensure_output_directory, path_exists};
use crate::normalize::{qualified_name, NormalizedJob, DEFAULT_NAMESPACE};
use crate::process::{run_checked, CommandRunner};
use crate::schedule::{CalendarSchedule, Schedule};
use crate::types::{JobState, JobStatus, Platform};

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

fn section(unit: &SystemdUnit, name: &str) -> Result<Section> {
    unit.get_section(name)
        .ok_or_else(|| Error::SystemdUnit(format!("missing [{name}] section after add_section")))
}

/// Context shared by the systemd driver; primarily overridable for tests.
pub struct LinuxContext {
    pub config_root: PathBuf,
    pub runner: Box<dyn CommandRunner>,
}

pub struct LinuxDriver {
    context: LinuxContext,
    namespace: String,
}

fn list(values: &[u32], wildcard: bool) -> String {
    if wildcard {
        "*".to_string()
    } else {
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn calendar_lines(schedule: &CalendarSchedule) -> Vec<String> {
    let month = list(&schedule.month.values, schedule.month.wildcard);
    let day = list(
        &schedule.day_of_month.values,
        schedule.day_of_month.wildcard,
    );
    let hour = list(&schedule.hour.values, schedule.hour.wildcard);
    let minute = list(&schedule.minute.values, schedule.minute.wildcard);
    let time = format!("{hour}:{minute}:00");
    let weekdays = schedule
        .day_of_week
        .values
        .iter()
        .map(|value| WEEKDAYS[*value as usize])
        .collect::<Vec<_>>()
        .join(",");

    if !schedule.day_of_month.wildcard && !schedule.day_of_week.wildcard {
        vec![
            format!("*-{month}-{day} {time}"),
            format!("{weekdays} *-{month}-* {time}"),
        ]
    } else if !schedule.day_of_week.wildcard {
        vec![format!("{weekdays} *-{month}-* {time}")]
    } else {
        vec![format!("*-{month}-{day} {time}")]
    }
}

/// Reads `Key=value` from `systemctl show` output.
fn show_value<'a>(show: &'a str, key: &str) -> Option<&'a str> {
    show.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "n/a")
}

/// Reads a `--timestamp=unix` value (`@1700000000`) from `systemctl show`
/// output. Unset timestamps are empty, `n/a`, or `@0`.
fn show_timestamp(show: &str, key: &str) -> Option<std::time::SystemTime> {
    show_value(show, key)?
        .strip_prefix('@')?
        .parse::<i64>()
        .ok()
        .filter(|seconds| *seconds > 0)
        .and_then(unix_seconds)
}

/// Renders the `.service` unit for `job`.
pub fn render_service(job: &NormalizedJob) -> Result<String> {
    let mut unit =
        SystemdUnit::from_str("").map_err(|error| Error::SystemdUnit(error.to_string()))?;
    unit.add_section("Unit");
    {
        let mut section = section(&unit, "Unit")?;
        section.set("Description", &format!("native-cron job: {}", job.id));
    }

    unit.add_section("Service");
    {
        let mut section = section(&unit, "Service")?;
        section.set("Type", "oneshot");
        if let Some(cwd) = &job.cwd {
            section.set("WorkingDirectory", &systemd_quote(&cwd.to_string_lossy()));
        }
        let exec_start = job
            .command
            .iter()
            .map(|arg| systemd_exec_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        section.set("ExecStart", &exec_start);
        for (key, value) in &job.env {
            section.add("Environment", &systemd_quote(&format!("{key}={value}")));
        }
        if let Some(stdout) = &job.stdout {
            section.set(
                "StandardOutput",
                &systemd_quote(&format!("append:{}", stdout.to_string_lossy())),
            );
        }
        if let Some(stderr) = &job.stderr {
            section.set(
                "StandardError",
                &systemd_quote(&format!("append:{}", stderr.to_string_lossy())),
            );
        }
        if matches!(job.schedule, Schedule::Startup) {
            section.set_bool("RemainAfterExit", true);
        }
    }

    if matches!(job.schedule, Schedule::Startup) {
        unit.add_section("Install");
        let mut section = section(&unit, "Install")?;
        section.set("WantedBy", "default.target");
    }

    Ok(unit.text())
}

/// Renders the `.timer` unit for `job`. Only meaningful for calendar schedules.
pub fn render_timer(job: &NormalizedJob, calendar: &CalendarSchedule) -> Result<String> {
    let mut unit =
        SystemdUnit::from_str("").map_err(|error| Error::SystemdUnit(error.to_string()))?;
    unit.add_section("Unit");
    {
        let mut section = section(&unit, "Unit")?;
        section.set("Description", &format!("native-cron schedule: {}", job.id));
    }

    unit.add_section("Timer");
    {
        let mut section = section(&unit, "Timer")?;
        for line in calendar_lines(calendar) {
            section.add("OnCalendar", &line);
        }
        section.set_bool("Persistent", true);
        section.set("AccuracySec", "1s");
        section.set(
            "Unit",
            &format!("{}.service", qualified_name(&job.namespace, &job.id)),
        );
    }

    unit.add_section("Install");
    {
        let mut section = section(&unit, "Install")?;
        section.set("WantedBy", "timers.target");
    }

    Ok(unit.text())
}

impl LinuxDriver {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(context: LinuxContext) -> Self {
        Self::with_namespace(context, DEFAULT_NAMESPACE)
    }

    /// Creates a driver whose native names start with `namespace` instead of
    /// [`DEFAULT_NAMESPACE`].
    pub fn with_namespace(context: LinuxContext, namespace: impl Into<String>) -> Self {
        Self {
            context,
            namespace: namespace.into(),
        }
    }

    fn paths(&self, id: &str) -> (PathBuf, PathBuf) {
        let root = self.context.config_root.join("systemd").join("user");
        (
            root.join(self.service_unit(id)),
            root.join(self.timer_unit(id)),
        )
    }

    fn timer_unit(&self, id: &str) -> String {
        format!("{}.timer", qualified_name(&self.namespace, id))
    }

    fn service_unit(&self, id: &str) -> String {
        format!("{}.service", qualified_name(&self.namespace, id))
    }

    fn installed_unit(&self, id: &str) -> Option<String> {
        let (service_path, timer_path) = self.paths(id);
        if path_exists(&timer_path) {
            Some(self.timer_unit(id))
        } else if path_exists(&service_path) {
            Some(self.service_unit(id))
        } else {
            None
        }
    }

    /// Forgets when a stopped timer last fired. `Persistent=true` otherwise
    /// fires the job immediately on start if it "missed" a run while it was
    /// disabled or under its previous schedule. Best effort: `clean` needs
    /// systemd 243+, and older versions just keep the old behavior.
    fn clear_timer_state(&self, unit: &str) {
        let _ = self
            .context
            .runner
            .run("systemctl", &["--user", "clean", "--what=state", unit]);
    }

    fn disable_unit(&self, unit: &str) -> Result<()> {
        run_checked(
            self.context.runner.as_ref(),
            "systemctl",
            &["--user", "disable", "--now", unit],
        )?;
        Ok(())
    }
}

impl Driver for LinuxDriver {
    fn preflight(&self, job: &NormalizedJob) -> Result<()> {
        render_service(job)?;
        if let Schedule::Calendar(calendar) = &job.schedule {
            render_timer(job, calendar)?;
        }
        Ok(())
    }

    fn register(&self, job: &NormalizedJob) -> Result<()> {
        let (service_path, timer_path) = self.paths(&job.id);
        let already_registered = self.installed_unit(&job.id).is_some();
        if already_registered && !job.overwrite {
            return Err(Error::AlreadyExists(job.id.clone()));
        }

        ensure_output_directory(job.stdout.as_deref())?;
        ensure_output_directory(job.stderr.as_deref())?;

        if already_registered {
            if let Some(unit) = self.installed_unit(&job.id) {
                self.disable_unit(&unit)?;
            }
        }

        let service_contents = render_service(job)?;
        atomic_write(&service_path, service_contents.as_bytes())?;

        let unit = match &job.schedule {
            Schedule::Startup => {
                if path_exists(&timer_path) {
                    std::fs::remove_file(&timer_path)?;
                }
                self.service_unit(&job.id)
            }
            Schedule::Calendar(calendar) => {
                let timer_contents = render_timer(job, calendar)?;
                atomic_write(&timer_path, timer_contents.as_bytes())?;
                self.timer_unit(&job.id)
            }
        };

        run_checked(
            self.context.runner.as_ref(),
            "systemctl",
            &["--user", "daemon-reload"],
        )?;
        run_checked(
            self.context.runner.as_ref(),
            "systemctl",
            &["--user", "enable", &unit],
        )?;
        match &job.schedule {
            Schedule::Startup => {
                run_checked(
                    self.context.runner.as_ref(),
                    "systemctl",
                    &["--user", "start", "--no-block", &unit],
                )?;
            }
            Schedule::Calendar(_) => {
                self.clear_timer_state(&unit);
                run_checked(
                    self.context.runner.as_ref(),
                    "systemctl",
                    &["--user", "restart", &unit],
                )?;
            }
        }
        Ok(())
    }

    fn enable(&self, id: &str) -> Result<()> {
        let unit = self
            .installed_unit(id)
            .ok_or_else(|| Error::NotRegistered(id.to_string()))?;
        if unit.ends_with(".service") {
            run_checked(
                self.context.runner.as_ref(),
                "systemctl",
                &["--user", "enable", &unit],
            )?;
            run_checked(
                self.context.runner.as_ref(),
                "systemctl",
                &["--user", "start", "--no-block", &unit],
            )?;
        } else {
            self.clear_timer_state(&unit);
            run_checked(
                self.context.runner.as_ref(),
                "systemctl",
                &["--user", "enable", "--now", &unit],
            )?;
        }
        Ok(())
    }

    fn disable(&self, id: &str) -> Result<()> {
        if let Some(unit) = self.installed_unit(id) {
            self.disable_unit(&unit)?;
        }
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.disable(id)?;
        let (service_path, timer_path) = self.paths(id);
        for path in [service_path, timer_path] {
            if path_exists(&path) {
                std::fs::remove_file(&path)?;
            }
        }
        run_checked(
            self.context.runner.as_ref(),
            "systemctl",
            &["--user", "daemon-reload"],
        )?;
        Ok(())
    }

    fn run_now(&self, id: &str) -> Result<()> {
        ensure_runnable(&self.status(id)?)?;
        // A startup service stays active after it exits (`RemainAfterExit`),
        // so `start` would be a no-op; restart it instead. A calendar job's
        // service is only active while it's running, and `start` then leaves
        // that run alone rather than overlapping it.
        let (_, timer_path) = self.paths(id);
        let verb = if path_exists(&timer_path) {
            "start"
        } else {
            "restart"
        };
        run_checked(
            self.context.runner.as_ref(),
            "systemctl",
            &["--user", verb, "--no-block", &self.service_unit(id)],
        )?;
        Ok(())
    }

    fn run_info(&self, id: &str) -> RunInfo {
        let show = |unit: &str, properties: &[&str]| {
            let mut args = vec!["--user", "show", "--timestamp=unix"];
            for property in properties {
                args.extend(["-p", property]);
            }
            args.push(unit);
            match self.context.runner.run("systemctl", &args) {
                Ok(output) if output.code == 0 => output.stdout,
                _ => String::new(),
            }
        };

        let service = show(
            &self.service_unit(id),
            &[
                "ExecMainStartTimestamp",
                "ExecMainExitTimestamp",
                "ExecMainStatus",
            ],
        );
        let exited = show_timestamp(&service, "ExecMainExitTimestamp").is_some();
        let mut info = RunInfo {
            last_run: show_timestamp(&service, "ExecMainStartTimestamp"),
            last_exit_code: show_value(&service, "ExecMainStatus")
                .filter(|_| exited)
                .and_then(|value| value.parse().ok()),
            next_run: None,
        };

        let (_, timer_path) = self.paths(id);
        if path_exists(&timer_path) {
            // The timer's last trigger survives the service being unloaded
            // and reboots (`Persistent=true`), but `run_now` starts the
            // service without triggering the timer, so take the newer one.
            let timer = show(
                &self.timer_unit(id),
                &["LastTriggerUSec", "NextElapseUSecRealtime"],
            );
            info.last_run = info.last_run.max(show_timestamp(&timer, "LastTriggerUSec"));
            info.next_run = show_timestamp(&timer, "NextElapseUSecRealtime");
        }
        info
    }

    fn status(&self, id: &str) -> Result<JobStatus> {
        let (service_path, timer_path) = self.paths(id);
        let service_exists = path_exists(&service_path);
        let timer_exists = path_exists(&timer_path);
        let unit = if timer_exists {
            self.timer_unit(id)
        } else {
            self.service_unit(id)
        };
        // A timer is active exactly while it's enabled. A startup service is
        // only active while its last run succeeded (`RemainAfterExit`), so ask
        // whether it's enabled instead; a failed run doesn't disable it.
        let check = if service_exists && !timer_exists {
            "is-enabled"
        } else {
            "is-active"
        };

        let result = self
            .context
            .runner
            .run("systemctl", &["--user", check, "--quiet", &unit])?;
        let expected_failure = match check {
            "is-enabled" => result.code == 1,
            _ => result.code == 3 || result.code == 4,
        };
        if result.code != 0 && !expected_failure {
            return Err(Error::CommandFailed {
                command: format!("systemctl --user {check} {unit}"),
                detail: result.stderr,
            });
        }

        let state = if result.code == 0 {
            JobState::Active
        } else if service_exists || timer_exists {
            JobState::Inactive
        } else {
            JobState::Missing
        };

        Ok(JobStatus {
            id: id.to_string(),
            platform: Platform::Linux,
            state,
            config_paths: vec![service_path, timer_path],
            cron: None,
            run_at_startup: false,
            command: None,
            cwd: None,
            env: None,
            stdout: None,
            stderr: None,
            last_run: None,
            last_exit_code: None,
            next_run: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize;
    use crate::test_support::{test_executable, FakeRunner};
    use crate::types::CronOptions;

    fn options(id: &str, cwd: &std::path::Path) -> CronOptions {
        CronOptions::new(
            id,
            "0 6 15 * 1",
            [
                test_executable(),
                "100%".to_string(),
                "has space".to_string(),
            ],
        )
        .cwd(cwd)
        .env("VALUE", "a$b%")
    }

    #[test]
    fn renders_safe_systemd_service_and_timer_units() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(options("daily-sync", dir.path())).unwrap();
        let calendar = match &job.schedule {
            Schedule::Calendar(calendar) => calendar,
            _ => panic!("expected calendar schedule"),
        };
        let service = render_service(&job).unwrap();
        let timer = render_timer(&job, calendar).unwrap();

        let executable = crate::escape::systemd_exec_quote(&test_executable());
        assert!(service.contains(&format!("ExecStart={executable} \"100%%\" \"has space\"")));
        assert!(service.contains("Environment=\"VALUE=a$b%%\""));
        assert!(timer.contains("OnCalendar=*-*-15 6:0:00"));
        assert!(timer.contains("OnCalendar=Mon *-*-* 6:0:00"));
        assert!(timer.contains("Persistent=yes"));
    }

    #[test]
    fn renders_startup_schedules_as_enabled_services_without_timers() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(CronOptions::at_startup("login", [test_executable()]).cwd(dir.path()))
            .unwrap();
        let service = render_service(&job).unwrap();
        assert!(service.contains("RemainAfterExit=yes"));
        assert!(service.contains("WantedBy=default.target"));
    }

    #[test]
    fn manages_systemd_user_units_and_treats_removal_as_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let active_clone = active.clone();
        let runner = FakeRunner::new(move |command, args| {
            if command == "systemctl" && args.contains(&"is-active") {
                return if active_clone.load(std::sync::atomic::Ordering::SeqCst) {
                    crate::test_support::success()
                } else {
                    crate::process::ProcessOutput {
                        code: 3,
                        stdout: "inactive".to_string(),
                        stderr: String::new(),
                    }
                };
            }
            if command == "systemctl" && args.contains(&"restart") {
                active_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if command == "systemctl" && args.contains(&"--now") && args.contains(&"disable") {
                active_clone.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            crate::test_support::success()
        });
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });

        let job = normalize(options("daily-sync", dir.path())).unwrap();
        driver.register(&job).unwrap();
        let status = driver.status("daily-sync").unwrap();
        assert_eq!(status.state, JobState::Active);
        assert!(std::fs::read_to_string(&status.config_paths[0])
            .unwrap()
            .contains("Type=oneshot"));
        assert!(std::fs::read_to_string(&status.config_paths[1])
            .unwrap()
            .contains("OnCalendar="));

        driver.disable("daily-sync").unwrap();
        assert_eq!(
            driver.status("daily-sync").unwrap().state,
            JobState::Inactive
        );
        driver.enable("daily-sync").unwrap();
        driver.remove("daily-sync").unwrap();
        driver.remove("daily-sync").unwrap();
        assert_eq!(
            driver.status("daily-sync").unwrap().state,
            JobState::Missing
        );
    }

    #[test]
    fn clears_persistent_timer_state_before_starting_a_timer() {
        let dir = tempfile::tempdir().unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls_clone = calls.clone();
        let runner = FakeRunner::new(move |command, args| {
            calls_clone
                .lock()
                .unwrap()
                .push(format!("{command} {}", args.join(" ")));
            crate::test_support::success()
        });
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });

        let job = normalize(options("resume", dir.path())).unwrap();
        driver.register(&job).unwrap();
        driver.disable("resume").unwrap();
        driver.enable("resume").unwrap();

        let calls = calls.lock().unwrap();
        let position = |call: &str| calls.iter().position(|seen| seen == call).unwrap();
        let clean = "systemctl --user clean --what=state native-cron-resume.timer";
        assert_eq!(calls.iter().filter(|seen| *seen == clean).count(), 2);
        assert!(position(clean) < position("systemctl --user restart native-cron-resume.timer"));
        assert!(
            calls.iter().rposition(|seen| seen == clean).unwrap()
                < position("systemctl --user enable --now native-cron-resume.timer")
        );
    }

    #[test]
    fn disabling_an_unregistered_job_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::always_success()),
        });
        driver.disable("never-registered").unwrap();
    }

    #[test]
    fn run_now_starts_the_service_and_restarts_startup_services() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, calls) =
            crate::test_support::recording_runner(|_, _| crate::test_support::success());
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });
        driver
            .register(&normalize(options("daily", dir.path())).unwrap())
            .unwrap();
        driver
            .register(
                &normalize(CronOptions::at_startup("login", [test_executable()]).cwd(dir.path()))
                    .unwrap(),
            )
            .unwrap();

        driver.run_now("daily").unwrap();
        driver.run_now("login").unwrap();
        let calls = calls.lock().unwrap();
        assert!(calls
            .contains(&"systemctl --user start --no-block native-cron-daily.service".to_string()));
        assert!(calls.contains(
            &"systemctl --user restart --no-block native-cron-login.service".to_string()
        ));
    }

    #[test]
    fn startup_jobs_stay_active_and_runnable_after_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let enabled_clone = enabled.clone();
        let (runner, calls) = crate::test_support::recording_runner(move |_, args| {
            if args.contains(&"is-active") {
                // The last run failed, so the service isn't active.
                return crate::test_support::exit_code(3);
            }
            if args.contains(&"is-enabled") {
                let enabled = enabled_clone.load(std::sync::atomic::Ordering::SeqCst);
                return crate::test_support::exit_code(if enabled { 0 } else { 1 });
            }
            crate::test_support::success()
        });
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });
        driver
            .register(
                &normalize(CronOptions::at_startup("login", [test_executable()]).cwd(dir.path()))
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(driver.status("login").unwrap().state, JobState::Active);
        driver.run_now("login").unwrap();
        assert!(calls.lock().unwrap().contains(
            &"systemctl --user restart --no-block native-cron-login.service".to_string()
        ));

        enabled.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(driver.status("login").unwrap().state, JobState::Inactive);
        assert!(matches!(driver.run_now("login"), Err(Error::Disabled(_))));
    }

    #[test]
    fn run_now_rejects_disabled_and_missing_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::new(|_, args| {
                if args.contains(&"is-active") {
                    return crate::test_support::exit_code(3);
                }
                crate::test_support::success()
            })),
        });
        driver
            .register(&normalize(options("daily", dir.path())).unwrap())
            .unwrap();
        assert!(matches!(driver.run_now("daily"), Err(Error::Disabled(_))));
        assert!(matches!(
            driver.run_now("missing"),
            Err(Error::NotRegistered(_))
        ));
    }

    #[test]
    fn reports_run_history_from_systemctl_show() {
        let dir = tempfile::tempdir().unwrap();
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::new(|_, args| {
                let stdout = if args.contains(&"native-cron-daily.timer") {
                    "LastTriggerUSec=@1700000000\nNextElapseUSecRealtime=@1700086400\n"
                } else if args.contains(&"native-cron-daily.service") {
                    "ExecMainStartTimestamp=@1699999999\nExecMainExitTimestamp=@1700000005\nExecMainStatus=2\n"
                } else {
                    ""
                };
                crate::process::ProcessOutput {
                    code: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }
            })),
        });
        driver
            .register(&normalize(options("daily", dir.path())).unwrap())
            .unwrap();

        let info = driver.run_info("daily");
        // The timer's last trigger is newer than the service's last start.
        assert_eq!(info.last_run, unix_seconds(1_700_000_000));
        assert_eq!(info.last_exit_code, Some(2));
        assert_eq!(info.next_run, unix_seconds(1_700_086_400));
    }

    #[test]
    fn reports_a_manual_run_newer_than_the_last_timer_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::new(|_, args| {
                let stdout = if args.contains(&"native-cron-daily.timer") {
                    "LastTriggerUSec=@1700000000\n"
                } else {
                    "ExecMainStartTimestamp=@1700050000\n"
                };
                crate::process::ProcessOutput {
                    code: 0,
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                }
            })),
        });
        driver
            .register(&normalize(options("daily", dir.path())).unwrap())
            .unwrap();
        assert_eq!(
            driver.run_info("daily").last_run,
            unix_seconds(1_700_050_000)
        );
    }

    #[test]
    fn leaves_unreported_run_history_empty() {
        let show = "ExecMainStartTimestamp=\nExecMainExitTimestamp=n/a\nExecMainStatus=0\nLastTriggerUSec=@0\n";
        assert_eq!(show_timestamp(show, "ExecMainStartTimestamp"), None);
        assert_eq!(show_timestamp(show, "ExecMainExitTimestamp"), None);
        assert_eq!(show_timestamp(show, "LastTriggerUSec"), None);
        assert_eq!(show_value(show, "ExecMainStatus"), Some("0"));

        let dir = tempfile::tempdir().unwrap();
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::new(|_, _| crate::test_support::exit_code(1))),
        });
        assert_eq!(driver.run_info("daily"), RunInfo::default());
    }

    #[test]
    fn namespaces_unit_names() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(options("daily", dir.path()).namespace("dev.agent-one")).unwrap();
        let calendar = match &job.schedule {
            Schedule::Calendar(calendar) => calendar,
            _ => panic!("expected calendar schedule"),
        };
        assert!(render_timer(&job, calendar)
            .unwrap()
            .contains("Unit=dev.agent-one.daily.service"));

        let (runner, calls) =
            crate::test_support::recording_runner(|_, _| crate::test_support::success());
        let driver = LinuxDriver::with_namespace(
            LinuxContext {
                config_root: dir.path().to_path_buf(),
                runner: Box::new(runner),
            },
            "dev.agent-one",
        );
        driver.register(&job).unwrap();
        let status = driver.status("daily").unwrap();
        assert!(status.config_paths[0].ends_with("systemd/user/dev.agent-one.daily.service"));
        assert!(status.config_paths[1].ends_with("systemd/user/dev.agent-one.daily.timer"));
        assert!(status.config_paths.iter().all(|path| path.exists()));
        assert!(calls
            .lock()
            .unwrap()
            .contains(&"systemctl --user restart dev.agent-one.daily.timer".to_string()));
    }

    #[test]
    fn uses_xdg_config_home_for_systemd_user_units() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|command, args| {
            if command == "systemctl" && args.contains(&"is-active") {
                return crate::process::ProcessOutput {
                    code: 3,
                    stdout: "inactive".to_string(),
                    stderr: String::new(),
                };
            }
            crate::test_support::success()
        });
        let driver = LinuxDriver::new(LinuxContext {
            config_root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });
        let job = normalize(options("xdg-job", dir.path())).unwrap();
        driver.register(&job).unwrap();
        let status = driver.status("xdg-job").unwrap();
        assert!(status
            .config_paths
            .iter()
            .all(|path| path.starts_with(dir.path())));
    }
}
