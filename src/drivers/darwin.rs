use std::path::{Path, PathBuf};

use plist::{Dictionary, Value};

use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::files::{atomic_write, ensure_output_directory, path_exists};
use crate::normalize::NormalizedJob;
use crate::process::{run_checked, CommandRunner};
use crate::schedule::{CalendarSchedule, Schedule};
use crate::types::{JobState, JobStatus, Platform};

/// Context shared by the launchd driver; primarily overridable for tests.
pub struct DarwinContext {
    pub home: PathBuf,
    pub uid: u32,
    pub runner: Box<dyn CommandRunner>,
}

pub struct DarwinDriver {
    context: DarwinContext,
}

#[derive(Clone)]
struct CalendarEntry {
    minute: Option<u32>,
    hour: Option<u32>,
    day: Option<u32>,
    month: Option<u32>,
    weekday: Option<u32>,
}

impl CalendarEntry {
    fn to_dictionary(&self) -> Dictionary {
        let mut dict = Dictionary::new();
        if let Some(value) = self.minute {
            dict.insert("Minute".to_string(), Value::Integer(value.into()));
        }
        if let Some(value) = self.hour {
            dict.insert("Hour".to_string(), Value::Integer(value.into()));
        }
        if let Some(value) = self.day {
            dict.insert("Day".to_string(), Value::Integer(value.into()));
        }
        if let Some(value) = self.month {
            dict.insert("Month".to_string(), Value::Integer(value.into()));
        }
        if let Some(value) = self.weekday {
            dict.insert("Weekday".to_string(), Value::Integer(value.into()));
        }
        dict
    }
}

fn cartesian(entries: Vec<CalendarEntry>, key: Option<&[u32]>, apply: impl Fn(&mut CalendarEntry, u32) + Copy) -> Vec<CalendarEntry> {
    match key {
        None => entries,
        Some(values) => entries
            .into_iter()
            .flat_map(|entry| {
                values.iter().map(move |value| {
                    let mut clone = CalendarEntry {
                        minute: entry.minute,
                        hour: entry.hour,
                        day: entry.day,
                        month: entry.month,
                        weekday: entry.weekday,
                    };
                    apply(&mut clone, *value);
                    clone
                })
            })
            .collect(),
    }
}

fn calendar_entries(schedule: &CalendarSchedule) -> Vec<CalendarEntry> {
    let seed = vec![CalendarEntry {
        minute: None,
        hour: None,
        day: None,
        month: None,
        weekday: None,
    }];

    let minute_values = if schedule.minute.wildcard { None } else { Some(schedule.minute.values.as_slice()) };
    let hour_values = if schedule.hour.wildcard { None } else { Some(schedule.hour.values.as_slice()) };
    let month_values = if schedule.month.wildcard { None } else { Some(schedule.month.values.as_slice()) };
    let day_values = if schedule.day_of_month.wildcard { None } else { Some(schedule.day_of_month.values.as_slice()) };
    let weekday_values = if schedule.day_of_week.wildcard { None } else { Some(schedule.day_of_week.values.as_slice()) };

    let common = cartesian(seed, minute_values, |entry, value| entry.minute = Some(value));
    let common = cartesian(common, hour_values, |entry, value| entry.hour = Some(value));
    let common = cartesian(common, month_values, |entry, value| entry.month = Some(value));

    if day_values.is_some() && weekday_values.is_some() {
        let mut day_branch = cartesian(common.clone(), day_values, |entry, value| entry.day = Some(value));
        let weekday_branch = cartesian(common, weekday_values, |entry, value| entry.weekday = Some(value));
        day_branch.extend(weekday_branch);
        day_branch
    } else {
        let with_day = cartesian(common, day_values, |entry, value| entry.day = Some(value));
        cartesian(with_day, weekday_values, |entry, value| entry.weekday = Some(value))
    }
}

/// Renders the launchd agent plist for `job`.
pub fn render_plist(job: &NormalizedJob) -> Result<Vec<u8>> {
    let label = format!("native-cron.{}", job.id);
    let mut root = Dictionary::new();
    root.insert("Label".to_string(), Value::String(label));

    let arguments: Vec<Value> = job.command.iter().map(|arg| Value::String(arg.clone())).collect();
    root.insert("ProgramArguments".to_string(), Value::Array(arguments));

    if let Some(cwd) = &job.cwd {
        root.insert(
            "WorkingDirectory".to_string(),
            Value::String(cwd.to_string_lossy().into_owned()),
        );
    }

    if !job.env.is_empty() {
        let mut env_dict = Dictionary::new();
        for (key, value) in &job.env {
            env_dict.insert(key.clone(), Value::String(value.clone()));
        }
        root.insert("EnvironmentVariables".to_string(), Value::Dictionary(env_dict));
    }

    match &job.schedule {
        Schedule::Startup => {
            root.insert("RunAtLoad".to_string(), Value::Boolean(true));
        }
        Schedule::Calendar(calendar) => {
            let entries = calendar_entries(calendar);
            if entries.len() > 10_000 {
                return Err(Error::TooManyIntervals(entries.len()));
            }
            let array = entries
                .iter()
                .map(|entry| Value::Dictionary(entry.to_dictionary()))
                .collect();
            root.insert("StartCalendarInterval".to_string(), Value::Array(array));
        }
    }

    if let Some(stdout) = &job.stdout {
        root.insert(
            "StandardOutPath".to_string(),
            Value::String(stdout.to_string_lossy().into_owned()),
        );
    }
    if let Some(stderr) = &job.stderr {
        root.insert(
            "StandardErrorPath".to_string(),
            Value::String(stderr.to_string_lossy().into_owned()),
        );
    }

    let mut buffer = Vec::new();
    Value::Dictionary(root).to_writer_xml(&mut buffer)?;
    Ok(buffer)
}

impl DarwinDriver {
    pub fn new(context: DarwinContext) -> Self {
        Self { context }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.context
            .home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("native-cron.{id}.plist"))
    }

    fn label(id: &str) -> String {
        format!("native-cron.{id}")
    }

    fn service(&self, id: &str) -> String {
        format!("gui/{}/{}", self.context.uid, Self::label(id))
    }

    fn bootout(&self, id: &str) -> Result<()> {
        let service = self.service(id);
        let result = self.context.runner.run("launchctl", &["bootout", &service])?;
        if result.code != 0 && result.code != 3 && result.code != 113 {
            return Err(Error::CommandFailed {
                command: format!("launchctl bootout {service}"),
                detail: result.stderr,
            });
        }
        Ok(())
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let service = self.service(id);
        let verb = if enabled { "enable" } else { "disable" };
        run_checked(self.context.runner.as_ref(), "launchctl", &[verb, &service])?;
        Ok(())
    }

    fn bootstrap(&self, path: &Path) -> Result<()> {
        let domain = format!("gui/{}", self.context.uid);
        let path_str = path.to_string_lossy();
        run_checked(
            self.context.runner.as_ref(),
            "launchctl",
            &["bootstrap", &domain, &path_str],
        )?;
        Ok(())
    }
}

impl Driver for DarwinDriver {
    fn preflight(&self, job: &NormalizedJob) -> Result<()> {
        render_plist(job)?;
        Ok(())
    }

    fn register(&self, job: &NormalizedJob) -> Result<()> {
        let path = self.path(&job.id);
        if !job.overwrite && path_exists(&path) {
            return Err(Error::AlreadyExists(job.id.clone()));
        }

        let contents = render_plist(job)?;
        ensure_output_directory(job.stdout.as_deref())?;
        ensure_output_directory(job.stderr.as_deref())?;
        atomic_write(&path, &contents)?;
        self.bootout(&job.id)?;
        self.set_enabled(&job.id, true)?;
        self.bootstrap(&path)?;
        Ok(())
    }

    fn enable(&self, id: &str) -> Result<()> {
        let path = self.path(id);
        if !path_exists(&path) {
            return Err(Error::NotRegistered(id.to_string()));
        }
        self.bootout(id)?;
        self.set_enabled(id, true)?;
        self.bootstrap(&path)?;
        Ok(())
    }

    fn disable(&self, id: &str) -> Result<()> {
        self.set_enabled(id, false)?;
        self.bootout(id)?;
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        self.bootout(id)?;
        let path = self.path(id);
        if path_exists(&path) {
            std::fs::remove_file(&path)?;
        }
        self.set_enabled(id, true)?;
        Ok(())
    }

    fn status(&self, id: &str) -> Result<JobStatus> {
        let path = self.path(id);
        let exists = path_exists(&path);
        let service = self.service(id);
        let result = self.context.runner.run("launchctl", &["print", &service])?;
        if result.code != 0 && result.code != 3 && result.code != 113 {
            return Err(Error::CommandFailed {
                command: format!("launchctl print {service}"),
                detail: result.stderr,
            });
        }

        let state = if result.code == 0 {
            JobState::Active
        } else if exists {
            JobState::Inactive
        } else {
            JobState::Missing
        };

        Ok(JobStatus {
            id: id.to_string(),
            platform: Platform::Darwin,
            state,
            config_paths: vec![path],
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
        CronOptions::new(id, "0 2 15 * 5", ["/bin/echo", "a&b"]).cwd(cwd)
    }

    #[test]
    fn renders_a_plist_with_safe_native_arguments_and_or_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(options("backup", dir.path())).unwrap();
        let xml = render_plist(&job).unwrap();
        let text = String::from_utf8(xml).unwrap();
        assert!(text.contains("<string>a&amp;b</string>"));
        assert!(text.contains("<key>Day</key>"));
        assert!(text.contains("<key>Weekday</key>"));
    }

    #[test]
    fn renders_startup_schedules_as_launchd_run_at_load_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(CronOptions::at_startup("login-task", ["/bin/echo"]).cwd(dir.path())).unwrap();
        let xml = String::from_utf8(render_plist(&job).unwrap()).unwrap();
        assert!(xml.contains("<key>RunAtLoad</key>"));
        assert!(!xml.contains("StartCalendarInterval"));
    }

    #[test]
    fn registers_disables_enables_and_removes_a_launchd_job() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|command, args| {
            if command == "launchctl" && args.first() == Some(&"bootout") {
                return crate::process::ProcessOutput {
                    code: 3,
                    stdout: String::new(),
                    stderr: "not found".to_string(),
                };
            }
            if command == "launchctl" && args.first() == Some(&"print") {
                return crate::process::ProcessOutput {
                    code: 113,
                    stdout: String::new(),
                    stderr: "not found".to_string(),
                };
            }
            crate::test_support::success()
        });
        let driver = DarwinDriver::new(DarwinContext {
            home: dir.path().to_path_buf(),
            uid: 501,
            runner: Box::new(runner),
        });

        let job = normalize(options("backup", dir.path())).unwrap();
        driver.register(&job).unwrap();
        let status = driver.status("backup").unwrap();
        assert_eq!(status.state, JobState::Inactive);
        assert!(std::fs::read_to_string(&status.config_paths[0])
            .unwrap()
            .contains("native-cron.backup"));

        driver.enable("backup").unwrap();
        driver.disable("backup").unwrap();
        driver.remove("backup").unwrap();
        assert_eq!(driver.status("backup").unwrap().state, JobState::Missing);
    }

    #[test]
    fn register_without_overwrite_rejects_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::always_success();
        let driver = DarwinDriver::new(DarwinContext {
            home: dir.path().to_path_buf(),
            uid: 501,
            runner: Box::new(runner),
        });
        let job = normalize(options("backup", dir.path())).unwrap();
        driver.register(&job).unwrap();
        let err = driver.register(&job).unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)));
    }
}
