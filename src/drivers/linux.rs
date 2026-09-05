use std::path::PathBuf;
use std::str::FromStr;

use systemd_unit_edit::SystemdUnit;

use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::escape::{systemd_exec_quote, systemd_quote};
use crate::files::{atomic_write, ensure_output_directory, path_exists};
use crate::normalize::NormalizedJob;
use crate::process::{run_checked, CommandRunner};
use crate::schedule::{CalendarSchedule, Schedule};
use crate::types::{JobState, JobStatus, Platform};

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Context shared by the systemd driver; primarily overridable for tests.
pub struct LinuxContext {
    pub config_root: PathBuf,
    pub runner: Box<dyn CommandRunner>,
}

pub struct LinuxDriver {
    context: LinuxContext,
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

/// Renders the `.service` unit for `job`.
pub fn render_service(job: &NormalizedJob) -> Result<String> {
    let mut unit =
        SystemdUnit::from_str("").map_err(|error| Error::SystemdUnit(error.to_string()))?;
    unit.add_section("Unit");
    {
        let mut section = unit.get_section("Unit").expect("just added");
        section.set("Description", &format!("native-cron job: {}", job.id));
    }

    unit.add_section("Service");
    {
        let mut section = unit.get_section("Service").expect("just added");
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
        let mut section = unit.get_section("Install").expect("just added");
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
        let mut section = unit.get_section("Unit").expect("just added");
        section.set("Description", &format!("native-cron schedule: {}", job.id));
    }

    unit.add_section("Timer");
    {
        let mut section = unit.get_section("Timer").expect("just added");
        for line in calendar_lines(calendar) {
            section.add("OnCalendar", &line);
        }
        section.set_bool("Persistent", true);
        section.set("AccuracySec", "1s");
        section.set("Unit", &format!("native-cron-{}.service", job.id));
    }

    unit.add_section("Install");
    {
        let mut section = unit.get_section("Install").expect("just added");
        section.set("WantedBy", "timers.target");
    }

    Ok(unit.text())
}

impl LinuxDriver {
    pub fn new(context: LinuxContext) -> Self {
        Self { context }
    }

    fn paths(&self, id: &str) -> (PathBuf, PathBuf) {
        let root = self.context.config_root.join("systemd").join("user");
        (
            root.join(format!("native-cron-{id}.service")),
            root.join(format!("native-cron-{id}.timer")),
        )
    }

    fn timer_unit(id: &str) -> String {
        format!("native-cron-{id}.timer")
    }

    fn service_unit(id: &str) -> String {
        format!("native-cron-{id}.service")
    }

    fn installed_unit(&self, id: &str) -> Option<String> {
        let (service_path, timer_path) = self.paths(id);
        if path_exists(&timer_path) {
            Some(Self::timer_unit(id))
        } else if path_exists(&service_path) {
            Some(Self::service_unit(id))
        } else {
            None
        }
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
                Self::service_unit(&job.id)
            }
            Schedule::Calendar(calendar) => {
                let timer_contents = render_timer(job, calendar)?;
                atomic_write(&timer_path, timer_contents.as_bytes())?;
                Self::timer_unit(&job.id)
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

    fn status(&self, id: &str) -> Result<JobStatus> {
        let (service_path, timer_path) = self.paths(id);
        let service_exists = path_exists(&service_path);
        let timer_exists = path_exists(&timer_path);
        let unit = if timer_exists {
            Self::timer_unit(id)
        } else {
            Self::service_unit(id)
        };

        let result = self
            .context
            .runner
            .run("systemctl", &["--user", "is-active", "--quiet", &unit])?;
        if result.code != 0 && result.code != 3 && result.code != 4 {
            return Err(Error::CommandFailed {
                command: format!("systemctl --user is-active {unit}"),
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize;
    use crate::test_support::FakeRunner;
    use crate::types::CronOptions;

    fn options(id: &str, cwd: &std::path::Path) -> CronOptions {
        CronOptions::new(id, "0 6 15 * 1", ["/bin/echo", "100%", "has space"])
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

        assert!(service.contains("ExecStart=\"/bin/echo\" \"100%%\" \"has space\""));
        assert!(service.contains("Environment=\"VALUE=a$b%%\""));
        assert!(timer.contains("OnCalendar=*-*-15 6:0:00"));
        assert!(timer.contains("OnCalendar=Mon *-*-* 6:0:00"));
        assert!(timer.contains("Persistent=yes"));
    }

    #[test]
    fn renders_startup_schedules_as_enabled_services_without_timers() {
        let dir = tempfile::tempdir().unwrap();
        let job =
            normalize(CronOptions::at_startup("login", ["/bin/echo"]).cwd(dir.path())).unwrap();
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
