use std::path::PathBuf;

use quick_xml::events::{BytesDecl, BytesText, Event};
use quick_xml::writer::Writer;

use crate::driver::Driver;
use crate::error::{Error, Result};
use crate::escape::{powershell_quote, windows_argument};
use crate::files::{atomic_write, ensure_output_directory, path_exists};
use crate::normalize::NormalizedJob;
use crate::process::{CommandRunner, ProcessOutput};
use crate::schedule::{CalendarSchedule, Schedule};
use crate::types::{JobState, JobStatus, Platform};

const MONTHS: [&str; 13] = [
    "", "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday",
];

/// Context shared by the Task Scheduler driver; primarily overridable for tests.
pub struct WindowsContext {
    pub root: PathBuf,
    pub user_id: Option<String>,
    pub runner: Box<dyn CommandRunner>,
}

pub struct WindowsDriver {
    context: WindowsContext,
}

fn utf16le_with_bom(value: &str) -> Vec<u8> {
    let mut bytes = vec![0xff, 0xfe];
    bytes.extend(value.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

fn task_is_missing(code: i32) -> bool {
    code == 0x8007_0002u32 as i32 || code == -2147024894
}

fn evenly_spaced(values: &[u32], period: u32) -> Option<u32> {
    if values.is_empty() {
        return None;
    }
    if values.len() == 1 {
        return Some(period);
    }
    let step = values[1].checked_sub(values[0])?;
    if step == 0 || period % step != 0 || values.len() as u32 != period / step {
        return None;
    }
    for (index, value) in values.iter().enumerate() {
        if *value != values[0] + index as u32 * step {
            return None;
        }
    }
    Some(step)
}

fn start_boundary(hour: u32, minute: u32) -> String {
    format!("2000-01-01T{hour:02}:{minute:02}:00")
}

/// Renders the PowerShell wrapper script that Task Scheduler invokes.
pub fn render_powershell_wrapper(job: &NormalizedJob) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    for (key, value) in &job.env {
        script.push_str(&format!("$env:{key} = {}\n", powershell_quote(value)));
    }
    if let Some(cwd) = &job.cwd {
        script.push_str(&format!(
            "Set-Location -LiteralPath {}\n",
            powershell_quote(&cwd.to_string_lossy())
        ));
    }
    let command = job
        .command
        .iter()
        .map(|arg| powershell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    script.push_str(&format!("& {command}"));
    if let Some(stdout) = &job.stdout {
        script.push_str(&format!(" 1>> {}", powershell_quote(&stdout.to_string_lossy())));
    }
    if let Some(stderr) = &job.stderr {
        script.push_str(&format!(" 2>> {}", powershell_quote(&stderr.to_string_lossy())));
    }
    script.push_str(
        "\n$nativeCronExitCode = $LASTEXITCODE\nif ($null -eq $nativeCronExitCode) { $nativeCronExitCode = 0 }\nexit $nativeCronExitCode\n",
    );
    script
}

fn render_calendar_trigger(
    writer: &mut Writer<Vec<u8>>,
    boundary: &str,
    schedule_xml: impl FnOnce(&mut Writer<Vec<u8>>) -> Result<()>,
    repetition: Option<&str>,
) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("CalendarTrigger")))?;
    writer
        .create_element("StartBoundary")
        .write_text_content(BytesText::new(boundary))?;
    if let Some(interval) = repetition {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Repetition")))?;
        writer
            .create_element("Interval")
            .write_text_content(BytesText::new(interval))?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Repetition")))?;
    }
    schedule_xml(writer)?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("CalendarTrigger")))?;
    Ok(())
}

fn write_values_xml(writer: &mut Writer<Vec<u8>>, wrapper: &str, element: &str, values: &[String]) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(wrapper)))?;
    for value in values {
        writer.create_element(element).write_text_content(BytesText::new(value))?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(wrapper)))?;
    Ok(())
}

fn write_named_values_xml(writer: &mut Writer<Vec<u8>>, wrapper: &str, values: &[&str]) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(wrapper)))?;
    for value in values {
        writer.create_element(*value).write_empty()?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(wrapper)))?;
    Ok(())
}

fn monthly_schedule(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("ScheduleByMonth")))?;
    let days: Vec<String> = schedule.day_of_month.values.iter().map(u32::to_string).collect();
    write_values_xml(writer, "DaysOfMonth", "Day", &days)?;
    let months: Vec<&str> = schedule.month.values.iter().map(|value| MONTHS[*value as usize]).collect();
    write_named_values_xml(writer, "Months", &months)?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("ScheduleByMonth")))?;
    Ok(())
}

fn weekday_schedule(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    let days: Vec<&str> = schedule
        .day_of_week
        .values
        .iter()
        .map(|value| WEEKDAYS[*value as usize])
        .collect();

    if schedule.month.wildcard {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new("ScheduleByWeek")))?;
        writer
            .create_element("WeeksInterval")
            .write_text_content(BytesText::new("1"))?;
        write_named_values_xml(writer, "DaysOfWeek", &days)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new("ScheduleByWeek")))?;
    } else {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "ScheduleByMonthDayOfWeek",
        )))?;
        write_named_values_xml(writer, "Weeks", &["Week1", "Week2", "Week3", "Week4", "WeekLast"])?;
        write_named_values_xml(writer, "DaysOfWeek", &days)?;
        let months: Vec<&str> = schedule.month.values.iter().map(|value| MONTHS[*value as usize]).collect();
        write_named_values_xml(writer, "Months", &months)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
            "ScheduleByMonthDayOfWeek",
        )))?;
    }
    Ok(())
}

fn any_day_schedule(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    if schedule.month.wildcard {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new("ScheduleByDay")))?;
        writer
            .create_element("DaysInterval")
            .write_text_content(BytesText::new("1"))?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new("ScheduleByDay")))?;
    } else {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new("ScheduleByMonth")))?;
        let days: Vec<String> = (1..=31u32).map(|value| value.to_string()).collect();
        write_values_xml(writer, "DaysOfMonth", "Day", &days)?;
        let months: Vec<&str> = schedule.month.values.iter().map(|value| MONTHS[*value as usize]).collect();
        write_named_values_xml(writer, "Months", &months)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new("ScheduleByMonth")))?;
    }
    Ok(())
}

fn render_triggers(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    let dates_wildcard =
        schedule.day_of_month.wildcard && schedule.day_of_week.wildcard && schedule.month.wildcard;
    let minute_step = evenly_spaced(&schedule.minute.values, 60);
    let hour_step = evenly_spaced(&schedule.hour.values, 24);
    let minute_repetition = dates_wildcard
        && schedule.hour.wildcard
        && minute_step.is_some_and(|step| 60 % step == 0);
    let hour_repetition = dates_wildcard
        && schedule.minute.values.len() == 1
        && hour_step.is_some_and(|step| 24 % step == 0);

    if minute_repetition || hour_repetition {
        let interval = if minute_repetition {
            format!("PT{}M", minute_step.unwrap())
        } else {
            format!("PT{}H", hour_step.unwrap())
        };
        let boundary = start_boundary(schedule.hour.values[0], schedule.minute.values[0]);
        render_calendar_trigger(
            writer,
            &boundary,
            |writer| any_day_schedule(writer, schedule),
            Some(&interval),
        )?;
        return Ok(());
    }

    let or_split = !schedule.day_of_month.wildcard && !schedule.day_of_week.wildcard;
    let count = schedule.hour.values.len() * schedule.minute.values.len() * if or_split { 2 } else { 1 };
    if count > 48 {
        return Err(Error::TooManyWindowsTriggers(count));
    }

    for hour in &schedule.hour.values {
        for minute in &schedule.minute.values {
            let boundary = start_boundary(*hour, *minute);
            if !schedule.day_of_month.wildcard {
                render_calendar_trigger(writer, &boundary, |writer| monthly_schedule(writer, schedule), None)?;
            }
            if !schedule.day_of_week.wildcard {
                render_calendar_trigger(writer, &boundary, |writer| weekday_schedule(writer, schedule), None)?;
            }
            if schedule.day_of_month.wildcard && schedule.day_of_week.wildcard {
                render_calendar_trigger(writer, &boundary, |writer| any_day_schedule(writer, schedule), None)?;
            }
        }
    }
    Ok(())
}

/// Renders the Task Scheduler XML task definition for `job`.
pub fn render_task_xml(job: &NormalizedJob, script_path: &str, user_id: Option<&str>) -> Result<String> {
    let user_id = user_id.ok_or(Error::MissingUserId)?;

    let arguments = [
        "-NoLogo",
        "-NonInteractive",
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        script_path,
    ]
    .iter()
    .map(|arg| windows_argument(arg))
    .collect::<Vec<_>>()
    .join(" ");

    let mut writer = Writer::new(Vec::new());

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-16"), None)))?;

    writer.write_event(Event::Start(
        quick_xml::events::BytesStart::new("Task").with_attributes([
            ("version", "1.2"),
            ("xmlns", "http://schemas.microsoft.com/windows/2004/02/mit/task"),
        ]),
    ))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("RegistrationInfo")))?;
    writer
        .create_element("Description")
        .write_text_content(BytesText::new(&format!("native-cron job: {}", job.id)))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("RegistrationInfo")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Triggers")))?;
    match &job.schedule {
        Schedule::Startup => {
            writer.write_event(Event::Start(quick_xml::events::BytesStart::new("LogonTrigger")))?;
            writer
                .create_element("Enabled")
                .write_text_content(BytesText::new("true"))?;
            writer.create_element("UserId").write_text_content(BytesText::new(user_id))?;
            writer.write_event(Event::End(quick_xml::events::BytesEnd::new("LogonTrigger")))?;
        }
        Schedule::Calendar(calendar) => {
            render_triggers(&mut writer, calendar)?;
        }
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Triggers")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Principals")))?;
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Principal")))?;
    writer.create_element("UserId").write_text_content(BytesText::new(user_id))?;
    writer
        .create_element("LogonType")
        .write_text_content(BytesText::new("S4U"))?;
    writer
        .create_element("RunLevel")
        .write_text_content(BytesText::new("LeastPrivilege"))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Principal")))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Principals")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Settings")))?;
    writer.create_element("Enabled").write_text_content(BytesText::new("true"))?;
    writer
        .create_element("AllowStartOnDemand")
        .write_text_content(BytesText::new("true"))?;
    writer
        .create_element("AllowHardTerminate")
        .write_text_content(BytesText::new("true"))?;
    writer
        .create_element("MultipleInstancesPolicy")
        .write_text_content(BytesText::new("IgnoreNew"))?;
    writer
        .create_element("StartWhenAvailable")
        .write_text_content(BytesText::new("true"))?;
    writer
        .create_element("DisallowStartIfOnBatteries")
        .write_text_content(BytesText::new("false"))?;
    writer
        .create_element("StopIfGoingOnBatteries")
        .write_text_content(BytesText::new("false"))?;
    writer
        .create_element("ExecutionTimeLimit")
        .write_text_content(BytesText::new("PT0S"))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Settings")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Actions")))?;
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Exec")))?;
    writer
        .create_element("Command")
        .write_text_content(BytesText::new("powershell.exe"))?;
    writer
        .create_element("Arguments")
        .write_text_content(BytesText::new(&arguments))?;
    if let Some(cwd) = &job.cwd {
        writer
            .create_element("WorkingDirectory")
            .write_text_content(BytesText::new(&cwd.to_string_lossy()))?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Exec")))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Actions")))?;

    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Task")))?;

    Ok(String::from_utf8_lossy(&writer.into_inner()).into_owned())
}

impl WindowsDriver {
    pub fn new(context: WindowsContext) -> Self {
        Self { context }
    }

    fn paths(&self, id: &str) -> (PathBuf, PathBuf) {
        (
            self.context.root.join(format!("{id}.xml")),
            self.context.root.join(format!("{id}.ps1")),
        )
    }

    fn task_name(id: &str) -> String {
        format!("native-cron-{id}")
    }

    fn is_startup(&self, id: &str) -> bool {
        let (xml_path, _) = self.paths(id);
        std::fs::read(&xml_path)
            .ok()
            .map(|bytes| {
                let text = crate::process::decode_process_output(&bytes);
                text.contains("<LogonTrigger>")
            })
            .unwrap_or(false)
    }

    fn query(&self, id: &str) -> Result<ProcessOutput> {
        let task = Self::task_name(id);
        self.context
            .runner
            .run("schtasks.exe", &["/query", "/tn", &task, "/xml", "/hresult"])
    }
}

impl Driver for WindowsDriver {
    fn preflight(&self, job: &NormalizedJob) -> Result<()> {
        let (_, script_path) = self.paths(&job.id);
        render_powershell_wrapper(job);
        render_task_xml(job, &script_path.to_string_lossy(), self.context.user_id.as_deref())?;
        Ok(())
    }

    fn register(&self, job: &NormalizedJob) -> Result<()> {
        let status = self.status(&job.id)?;
        if status.state != JobState::Missing && !job.overwrite {
            return Err(Error::AlreadyExists(job.id.clone()));
        }

        ensure_output_directory(job.stdout.as_deref())?;
        ensure_output_directory(job.stderr.as_deref())?;

        let (xml_path, script_path) = self.paths(&job.id);
        let previous_xml = std::fs::read(&xml_path).ok();
        let previous_script = std::fs::read(&script_path).ok();

        let write_result = (|| -> Result<()> {
            let script = render_powershell_wrapper(job);
            atomic_write(&script_path, &utf16le_with_bom(&script))?;
            let xml = render_task_xml(job, &script_path.to_string_lossy(), self.context.user_id.as_deref())?;
            atomic_write(&xml_path, &utf16le_with_bom(&xml))?;

            let task = Self::task_name(&job.id);
            let xml_str = xml_path.to_string_lossy();
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/create", "/xml", &xml_str, "/tn", &task, "/np", "/f"],
            )?;
            Ok(())
        })();

        if let Err(error) = write_result {
            match previous_xml {
                Some(bytes) => atomic_write(&xml_path, &bytes)?,
                None => {
                    if path_exists(&xml_path) {
                        std::fs::remove_file(&xml_path)?;
                    }
                }
            }
            match previous_script {
                Some(bytes) => atomic_write(&script_path, &bytes)?,
                None => {
                    if path_exists(&script_path) {
                        std::fs::remove_file(&script_path)?;
                    }
                }
            }
            return Err(error);
        }

        if matches!(job.schedule, Schedule::Startup) {
            let task = Self::task_name(&job.id);
            run_checked_owned(self.context.runner.as_ref(), "schtasks.exe", vec!["/run", "/tn", &task])?;
        }
        Ok(())
    }

    fn enable(&self, id: &str) -> Result<()> {
        if self.status(id)?.state == JobState::Missing {
            return Err(Error::NotRegistered(id.to_string()));
        }
        let task = Self::task_name(id);
        run_checked_owned(
            self.context.runner.as_ref(),
            "schtasks.exe",
            vec!["/change", "/tn", &task, "/enable"],
        )?;
        if self.is_startup(id) {
            run_checked_owned(self.context.runner.as_ref(), "schtasks.exe", vec!["/run", "/tn", &task])?;
        }
        Ok(())
    }

    fn disable(&self, id: &str) -> Result<()> {
        if self.status(id)?.state != JobState::Missing {
            let task = Self::task_name(id);
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/change", "/tn", &task, "/disable"],
            )?;
        }
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        let (xml_path, script_path) = self.paths(id);
        if self.status(id)?.state != JobState::Missing {
            let task = Self::task_name(id);
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/delete", "/tn", &task, "/f"],
            )?;
        }
        for path in [xml_path, script_path] {
            if path_exists(&path) {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    fn status(&self, id: &str) -> Result<JobStatus> {
        let (xml_path, script_path) = self.paths(id);
        let result = self.query(id)?;
        if result.code != 0 {
            if task_is_missing(result.code) {
                return Ok(JobStatus {
                    id: id.to_string(),
                    platform: Platform::Windows,
                    state: JobState::Missing,
                    config_paths: vec![xml_path, script_path],
                    cron: None,
                    run_at_startup: false,
                    command: None,
                    cwd: None,
                    env: None,
                    stdout: None,
                    stderr: None,
                });
            }
            return Err(Error::CommandFailed {
                command: format!("schtasks.exe /query /tn native-cron-{id}"),
                detail: result.stderr,
            });
        }

        let disabled = result.stdout.to_lowercase().contains("<enabled>false</enabled>");
        Ok(JobStatus {
            id: id.to_string(),
            platform: Platform::Windows,
            state: if disabled { JobState::Inactive } else { JobState::Active },
            config_paths: vec![xml_path, script_path],
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

fn run_checked_owned(runner: &dyn CommandRunner, command: &str, args: Vec<&str>) -> Result<ProcessOutput> {
    crate::process::run_checked(runner, command, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::normalize;
    use crate::test_support::FakeRunner;
    use crate::types::CronOptions;

    fn options(id: &str, cwd: &std::path::Path) -> CronOptions {
        CronOptions::new(id, "*/15 * * * *", ["/bin/echo", "it's safe"])
            .cwd(cwd)
            .env("TOKEN", "it's secret")
    }

    #[test]
    fn renders_a_powershell_wrapper_without_interpolating_values_as_code() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(options("backup", dir.path())).unwrap();
        let wrapper = render_powershell_wrapper(&job);
        assert!(wrapper.contains("$env:TOKEN = 'it''s secret'"));
        assert!(wrapper.contains("'it''s safe'"));
    }

    #[test]
    fn compresses_regular_intervals_into_one_windows_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(options("backup", dir.path())).unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1", Some("DOMAIN\\me")).unwrap();
        assert_eq!(xml.matches("<CalendarTrigger>").count(), 1);
        assert!(xml.contains("<Interval>PT15M</Interval>"));
        assert!(xml.contains("<LogonType>S4U</LogonType>"));

        let hourly_job = normalize(
            CronOptions::new("backup", "0 * * * *", ["/bin/echo"]).cwd(dir.path()),
        )
        .unwrap();
        let hourly = render_task_xml(&hourly_job, "C:\\native-cron\\backup.ps1", Some("DOMAIN\\me")).unwrap();
        assert!(hourly.contains("<Interval>PT60M</Interval>"));
    }

    #[test]
    fn renders_startup_schedules_as_windows_logon_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(CronOptions::at_startup("backup", ["/bin/echo"]).cwd(dir.path())).unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1", Some("DOMAIN\\me")).unwrap();
        assert!(xml.contains("<LogonTrigger>"));
        assert!(!xml.contains("<CalendarTrigger>"));
    }

    #[test]
    fn splits_day_of_month_and_weekday_restrictions_to_preserve_or_logic() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(
            CronOptions::new("backup", "0 9 15 JAN MON-FRI", ["/bin/echo"]).cwd(dir.path()),
        )
        .unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1", Some("DOMAIN\\me")).unwrap();
        assert_eq!(xml.matches("<CalendarTrigger>").count(), 2);
        assert!(xml.contains("<ScheduleByMonth>"));
        assert!(xml.contains("<ScheduleByMonthDayOfWeek>"));
    }

    #[test]
    fn rejects_expressions_that_exceed_the_windows_48_trigger_limit() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(CronOptions::new("backup", "*/7 * * * *", ["/bin/echo"]).cwd(dir.path())).unwrap();
        let error = render_task_xml(&job, "C:\\native-cron\\backup.ps1", Some("DOMAIN\\me")).unwrap_err();
        assert!(matches!(error, Error::TooManyWindowsTriggers(_)));
    }

    #[test]
    fn manages_and_reports_a_windows_scheduled_task() {
        let dir = tempfile::tempdir().unwrap();
        let registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (registered_clone, enabled_clone) = (registered.clone(), enabled.clone());
        let runner = FakeRunner::new(move |command, args| {
            if command != "schtasks.exe" {
                return crate::test_support::success();
            }
            if args.first() == Some(&"/create") {
                registered_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                enabled_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                return crate::test_support::success();
            }
            if args.first() == Some(&"/query") {
                return if registered_clone.load(std::sync::atomic::Ordering::SeqCst) {
                    ProcessOutput {
                        code: 0,
                        stdout: format!(
                            "<Task><Settings><Enabled>{}</Enabled></Settings></Task>",
                            enabled_clone.load(std::sync::atomic::Ordering::SeqCst)
                        ),
                        stderr: String::new(),
                    }
                } else {
                    ProcessOutput {
                        code: 0x8007_0002u32 as i32,
                        stdout: String::new(),
                        stderr: "Task not found".to_string(),
                    }
                };
            }
            if args.first() == Some(&"/change") {
                enabled_clone.store(args.contains(&"/enable"), std::sync::atomic::Ordering::SeqCst);
                return crate::test_support::success();
            }
            if args.first() == Some(&"/delete") {
                registered_clone.store(false, std::sync::atomic::Ordering::SeqCst);
                return crate::test_support::success();
            }
            crate::test_support::success()
        });
        let driver = WindowsDriver::new(WindowsContext {
            root: dir.path().to_path_buf(),
            user_id: Some("DOMAIN\\me".to_string()),
            runner: Box::new(runner),
        });

        let job = normalize(options("backup", dir.path())).unwrap();
        driver.register(&job).unwrap();
        let status = driver.status("backup").unwrap();
        assert_eq!(status.state, JobState::Active);
        let xml_bytes = std::fs::read(&status.config_paths[0]).unwrap();
        let script_bytes = std::fs::read(&status.config_paths[1]).unwrap();
        assert_eq!(&xml_bytes[0..2], &[0xff, 0xfe]);
        assert_eq!(&script_bytes[0..2], &[0xff, 0xfe]);

        driver.disable("backup").unwrap();
        assert_eq!(driver.status("backup").unwrap().state, JobState::Inactive);
        driver.enable("backup").unwrap();
        assert_eq!(driver.status("backup").unwrap().state, JobState::Active);
        driver.remove("backup").unwrap();
        assert_eq!(driver.status("backup").unwrap().state, JobState::Missing);
    }

    #[test]
    fn restores_local_state_when_windows_task_creation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner::new(|command, args| {
            if command == "schtasks.exe" && args.first() == Some(&"/create") {
                return ProcessOutput {
                    code: 1,
                    stdout: String::new(),
                    stderr: "Task creation failed".to_string(),
                };
            }
            if command == "schtasks.exe" && args.first() == Some(&"/query") {
                return ProcessOutput {
                    code: 0x8007_0002u32 as i32,
                    stdout: String::new(),
                    stderr: "Task not found".to_string(),
                };
            }
            crate::test_support::success()
        });
        let driver = WindowsDriver::new(WindowsContext {
            root: dir.path().to_path_buf(),
            user_id: Some("DOMAIN\\me".to_string()),
            runner: Box::new(runner),
        });

        let job = normalize(options("backup", dir.path())).unwrap();
        let error = driver.register(&job).unwrap_err();
        assert!(matches!(error, Error::CommandFailed { .. }));
        let status = driver.status("backup").unwrap();
        assert_eq!(status.state, JobState::Missing);
        for path in &status.config_paths {
            assert!(!path.exists());
        }
    }
}
