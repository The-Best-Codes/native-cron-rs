use std::path::PathBuf;

#[cfg(target_os = "windows")]
use chrono::Local;
use quick_xml::events::{BytesDecl, BytesText, Event};
use quick_xml::writer::Writer;

use crate::driver::{ensure_runnable, unix_seconds, Driver, RunInfo};
use crate::error::{Error, Result};
use crate::escape::{powershell_quote, windows_argument};
use crate::files::{atomic_write, ensure_output_directory, path_exists};
use crate::normalize::{qualified_name, NormalizedJob, DEFAULT_NAMESPACE};
use crate::process::{CommandRunner, ProcessOutput};
use crate::schedule::{CalendarSchedule, Schedule};
use crate::types::{JobState, JobStatus, Platform};

const MONTHS: [&str; 13] = [
    "",
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Context shared by the Task Scheduler driver; primarily overridable for tests.
pub struct WindowsContext {
    pub root: PathBuf,
    pub runner: Box<dyn CommandRunner>,
}

pub struct WindowsDriver {
    context: WindowsContext,
    namespace: String,
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
    if step == 0 || !period.is_multiple_of(step) || values.len() as u32 != period / step {
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
    #[cfg(target_os = "windows")]
    let today = Local::now();
    #[cfg(target_os = "windows")]
    let date_str = today.format("%Y-%m-%d").to_string();
    #[cfg(not(target_os = "windows"))]
    let date_str = "1970-01-01".to_string();

    format!("{date_str}T{hour:02}:{minute:02}:00")
}

/// Whether Task Scheduler can run the command itself instead of going through
/// the PowerShell wrapper. Only done for visible jobs: the wrapper's console
/// would show up in the user's session, and there is nothing (env vars or
/// output redirection) that needs the wrapper anyway.
fn runs_directly(job: &NormalizedJob) -> bool {
    job.windows.visible && job.env.is_empty() && job.stdout.is_none() && job.stderr.is_none()
}

/// `LastTaskResult` values that are Task Scheduler states rather than an exit
/// code: ready, currently running, and has not run yet.
const TASK_STATE_RESULTS: [i64; 3] = [0x41300, 0x41301, 0x41303];

/// Renders a PowerShell command that prints a task's run history as
/// `Key=value` lines, with times as Unix seconds. Task Scheduler reports a
/// never-run task's last run time as 1999-11-30, so pre-2000 times are left
/// empty.
fn render_task_info_script(task: &str) -> String {
    format!(
        "$i = Get-ScheduledTaskInfo -TaskName {} -ErrorAction Stop\n\
         function T($d) {{ if ($d -and $d.Year -ge 2000) {{ [DateTimeOffset]::new($d).ToUnixTimeSeconds() }} }}\n\
         \"LastRunTime=$(T $i.LastRunTime)\"\n\
         \"LastTaskResult=$($i.LastTaskResult)\"\n\
         \"NextRunTime=$(T $i.NextRunTime)\"",
        powershell_quote(task)
    )
}

fn parse_task_info(output: &str) -> RunInfo {
    let value = |key: &str| {
        output
            .lines()
            .find_map(|line| line.trim().strip_prefix(key)?.strip_prefix('='))
            .and_then(|value| value.trim().parse::<i64>().ok())
    };
    RunInfo {
        last_run: value("LastRunTime").and_then(unix_seconds),
        // Exit codes are reported as unsigned 32-bit values.
        last_exit_code: value("LastTaskResult")
            .filter(|result| !TASK_STATE_RESULTS.contains(result))
            .map(|result| result as u32 as i32),
        next_run: value("NextRunTime").and_then(unix_seconds),
    }
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
        script.push_str(&format!(
            " 1>> {}",
            powershell_quote(&stdout.to_string_lossy())
        ));
    }
    if let Some(stderr) = &job.stderr {
        script.push_str(&format!(
            " 2>> {}",
            powershell_quote(&stderr.to_string_lossy())
        ));
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
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
        "CalendarTrigger",
    )))?;
    writer
        .create_element("StartBoundary")
        .write_text_content(BytesText::new(boundary))?;
    if let Some(interval) = repetition {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "Repetition",
        )))?;
        writer
            .create_element("Interval")
            .write_text_content(BytesText::new(interval))?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Repetition")))?;
    }
    schedule_xml(writer)?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
        "CalendarTrigger",
    )))?;
    Ok(())
}

fn write_values_xml(
    writer: &mut Writer<Vec<u8>>,
    wrapper: &str,
    element: &str,
    values: &[String],
) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(wrapper)))?;
    for value in values {
        writer
            .create_element(element)
            .write_text_content(BytesText::new(value))?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(wrapper)))?;
    Ok(())
}

fn write_named_values_xml(
    writer: &mut Writer<Vec<u8>>,
    wrapper: &str,
    values: &[&str],
) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(wrapper)))?;
    for value in values {
        writer.create_element(*value).write_empty()?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(wrapper)))?;
    Ok(())
}

fn write_text_values_xml(
    writer: &mut Writer<Vec<u8>>,
    wrapper: &str,
    element: &str,
    values: &[&str],
) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(wrapper)))?;
    for value in values {
        writer
            .create_element(element)
            .write_text_content(BytesText::new(value))?;
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(wrapper)))?;
    Ok(())
}

fn monthly_schedule(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
        "ScheduleByMonth",
    )))?;
    let days: Vec<String> = schedule
        .day_of_month
        .values
        .iter()
        .map(u32::to_string)
        .collect();
    write_values_xml(writer, "DaysOfMonth", "Day", &days)?;
    let months: Vec<&str> = schedule
        .month
        .values
        .iter()
        .map(|value| MONTHS[*value as usize])
        .collect();
    write_named_values_xml(writer, "Months", &months)?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
        "ScheduleByMonth",
    )))?;
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
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "ScheduleByWeek",
        )))?;
        writer
            .create_element("WeeksInterval")
            .write_text_content(BytesText::new("1"))?;
        write_named_values_xml(writer, "DaysOfWeek", &days)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
            "ScheduleByWeek",
        )))?;
    } else {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "ScheduleByMonthDayOfWeek",
        )))?;
        write_text_values_xml(writer, "Weeks", "Week", &["1", "2", "3", "4", "Last"])?;
        write_named_values_xml(writer, "DaysOfWeek", &days)?;
        let months: Vec<&str> = schedule
            .month
            .values
            .iter()
            .map(|value| MONTHS[*value as usize])
            .collect();
        write_named_values_xml(writer, "Months", &months)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
            "ScheduleByMonthDayOfWeek",
        )))?;
    }
    Ok(())
}

fn any_day_schedule(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    if schedule.month.wildcard {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "ScheduleByDay",
        )))?;
        writer
            .create_element("DaysInterval")
            .write_text_content(BytesText::new("1"))?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
            "ScheduleByDay",
        )))?;
    } else {
        writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
            "ScheduleByMonth",
        )))?;
        let days: Vec<String> = (1..=31u32).map(|value| value.to_string()).collect();
        write_values_xml(writer, "DaysOfMonth", "Day", &days)?;
        let months: Vec<&str> = schedule
            .month
            .values
            .iter()
            .map(|value| MONTHS[*value as usize])
            .collect();
        write_named_values_xml(writer, "Months", &months)?;
        writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
            "ScheduleByMonth",
        )))?;
    }
    Ok(())
}

fn render_triggers(writer: &mut Writer<Vec<u8>>, schedule: &CalendarSchedule) -> Result<()> {
    let dates_wildcard =
        schedule.day_of_month.wildcard && schedule.day_of_week.wildcard && schedule.month.wildcard;
    let minute_step = evenly_spaced(&schedule.minute.values, 60);
    let hour_step = evenly_spaced(&schedule.hour.values, 24);
    let minute_repetition =
        dates_wildcard && schedule.hour.wildcard && minute_step.is_some_and(|step| 60 % step == 0);
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
    let count =
        schedule.hour.values.len() * schedule.minute.values.len() * if or_split { 2 } else { 1 };
    if count > 48 {
        return Err(Error::TooManyWindowsTriggers(count));
    }

    for hour in &schedule.hour.values {
        for minute in &schedule.minute.values {
            let boundary = start_boundary(*hour, *minute);
            if !schedule.day_of_month.wildcard {
                render_calendar_trigger(
                    writer,
                    &boundary,
                    |writer| monthly_schedule(writer, schedule),
                    None,
                )?;
            }
            if !schedule.day_of_week.wildcard {
                render_calendar_trigger(
                    writer,
                    &boundary,
                    |writer| weekday_schedule(writer, schedule),
                    None,
                )?;
            }
            if schedule.day_of_month.wildcard && schedule.day_of_week.wildcard {
                render_calendar_trigger(
                    writer,
                    &boundary,
                    |writer| any_day_schedule(writer, schedule),
                    None,
                )?;
            }
        }
    }
    Ok(())
}

/// Renders the Task Scheduler XML task definition for `job`.
pub fn render_task_xml(job: &NormalizedJob, script_path: &str) -> Result<String> {
    let (program, arguments) = if runs_directly(job) {
        (job.command[0].as_str(), job.command[1..].to_vec())
    } else {
        let mut arguments = vec!["-NoLogo", "-NonInteractive", "-NoProfile"];
        if job.windows.visible {
            arguments.extend(["-WindowStyle", "Hidden"]);
        }
        arguments.extend(["-ExecutionPolicy", "Bypass", "-File", script_path]);
        (
            "powershell.exe",
            arguments.into_iter().map(str::to_string).collect(),
        )
    };
    let arguments = arguments
        .iter()
        .map(|arg| windows_argument(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let mut writer = Writer::new(Vec::new());

    writer.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-16"), None)))?;

    writer.write_event(Event::Start(
        quick_xml::events::BytesStart::new("Task").with_attributes([
            ("version", "1.2"),
            (
                "xmlns",
                "http://schemas.microsoft.com/windows/2004/02/mit/task",
            ),
        ]),
    ))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
        "RegistrationInfo",
    )))?;
    writer
        .create_element("Description")
        .write_text_content(BytesText::new(&format!("native-cron job: {}", job.id)))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new(
        "RegistrationInfo",
    )))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Triggers")))?;
    match &job.schedule {
        Schedule::Startup => {
            writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
                "LogonTrigger",
            )))?;
            writer
                .create_element("Enabled")
                .write_text_content(BytesText::new("true"))?;
            writer.write_event(Event::End(quick_xml::events::BytesEnd::new("LogonTrigger")))?;
        }
        Schedule::Calendar(calendar) => {
            render_triggers(&mut writer, calendar)?;
        }
    }
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Triggers")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
        "Principals",
    )))?;
    writer.write_event(Event::Start(quick_xml::events::BytesStart::new(
        "Principal",
    )))?;
    writer
        .create_element("LogonType")
        .write_text_content(BytesText::new(if job.windows.visible {
            "InteractiveToken"
        } else {
            "S4U"
        }))?;
    writer
        .create_element("RunLevel")
        .write_text_content(BytesText::new("LeastPrivilege"))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Principal")))?;
    writer.write_event(Event::End(quick_xml::events::BytesEnd::new("Principals")))?;

    writer.write_event(Event::Start(quick_xml::events::BytesStart::new("Settings")))?;
    writer
        .create_element("Enabled")
        .write_text_content(BytesText::new("true"))?;
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
        .write_text_content(BytesText::new(program))?;
    if !arguments.is_empty() {
        writer
            .create_element("Arguments")
            .write_text_content(BytesText::new(&arguments))?;
    }
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(context: WindowsContext) -> Self {
        Self::with_namespace(context, DEFAULT_NAMESPACE)
    }

    /// Creates a driver whose native names start with `namespace` instead of
    /// [`DEFAULT_NAMESPACE`].
    pub fn with_namespace(context: WindowsContext, namespace: impl Into<String>) -> Self {
        Self {
            context,
            namespace: namespace.into(),
        }
    }

    fn paths(&self, id: &str) -> (PathBuf, PathBuf) {
        // Jobs in the default namespace keep their files directly in the
        // root, where they were before namespaces existed.
        let directory = if self.namespace == DEFAULT_NAMESPACE {
            self.context.root.clone()
        } else {
            self.context.root.join(&self.namespace)
        };
        (
            directory.join(format!("{id}.xml")),
            directory.join(format!("{id}.ps1")),
        )
    }

    fn task_name(&self, id: &str) -> String {
        qualified_name(&self.namespace, id)
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
        let task = self.task_name(id);
        self.context.runner.run(
            "schtasks.exe",
            &["/query", "/tn", &task, "/xml", "/hresult"],
        )
    }
}

impl Driver for WindowsDriver {
    fn preflight(&self, job: &NormalizedJob) -> Result<()> {
        let (_, script_path) = self.paths(&job.id);
        if !runs_directly(job) {
            render_powershell_wrapper(job);
        }
        render_task_xml(job, &script_path.to_string_lossy())?;
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
            if runs_directly(job) {
                if path_exists(&script_path) {
                    std::fs::remove_file(&script_path)?;
                }
            } else {
                let script = render_powershell_wrapper(job);
                atomic_write(&script_path, &utf16le_with_bom(&script))?;
            }
            let xml = render_task_xml(job, &script_path.to_string_lossy())?;
            atomic_write(&xml_path, &utf16le_with_bom(&xml))?;

            let task = self.task_name(&job.id);
            let xml_str = xml_path.to_string_lossy();
            // `/it` (InteractiveToken) and `/np` (S4U) select the same logon
            // type as the `<LogonType>` written into the XML above. Task
            // Scheduler has a known quirk where the XML's `<LogonType>` alone
            // is not always honored via `/create /xml`, so it must be
            // reiterated on the command line.
            let logon_flag = if job.windows.visible { "/it" } else { "/np" };
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/create", "/xml", &xml_str, "/tn", &task, logon_flag, "/f"],
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
            let task = self.task_name(&job.id);
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/run", "/tn", &task],
            )?;
        }
        Ok(())
    }

    fn enable(&self, id: &str) -> Result<()> {
        if self.status(id)?.state == JobState::Missing {
            return Err(Error::NotRegistered(id.to_string()));
        }
        let task = self.task_name(id);
        run_checked_owned(
            self.context.runner.as_ref(),
            "schtasks.exe",
            vec!["/change", "/tn", &task, "/enable"],
        )?;
        if self.is_startup(id) {
            run_checked_owned(
                self.context.runner.as_ref(),
                "schtasks.exe",
                vec!["/run", "/tn", &task],
            )?;
        }
        Ok(())
    }

    fn disable(&self, id: &str) -> Result<()> {
        if self.status(id)?.state != JobState::Missing {
            let task = self.task_name(id);
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
            let task = self.task_name(id);
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

    fn run_now(&self, id: &str) -> Result<()> {
        ensure_runnable(&self.status(id)?)?;
        let task = self.task_name(id);
        run_checked_owned(
            self.context.runner.as_ref(),
            "schtasks.exe",
            vec!["/run", "/tn", &task],
        )?;
        Ok(())
    }

    fn run_info(&self, id: &str) -> RunInfo {
        let script = render_task_info_script(&self.task_name(id));
        match self.context.runner.run(
            "powershell.exe",
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ],
        ) {
            Ok(output) if output.code == 0 => parse_task_info(&output.stdout),
            _ => RunInfo::default(),
        }
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
                    last_run: None,
                    last_exit_code: None,
                    next_run: None,
                });
            }
            return Err(Error::CommandFailed {
                command: format!("schtasks.exe /query /tn {}", self.task_name(id)),
                detail: result.stderr,
            });
        }

        let disabled = result
            .stdout
            .to_lowercase()
            .contains("<enabled>false</enabled>");
        Ok(JobStatus {
            id: id.to_string(),
            platform: Platform::Windows,
            state: if disabled {
                JobState::Inactive
            } else {
                JobState::Active
            },
            config_paths: vec![xml_path, script_path],
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

fn run_checked_owned(
    runner: &dyn CommandRunner,
    command: &str,
    args: Vec<&str>,
) -> Result<ProcessOutput> {
    crate::process::run_checked(runner, command, &args)
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
            "*/15 * * * *",
            [test_executable(), "it's safe".to_string()],
        )
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
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap();
        assert_eq!(xml.matches("<CalendarTrigger>").count(), 1);
        assert!(xml.contains("<Interval>PT15M</Interval>"));
        assert!(xml.contains("<LogonType>S4U</LogonType>"));
        assert!(!xml.contains("<UserId>"));

        let hourly_job =
            normalize(CronOptions::new("backup", "0 * * * *", [test_executable()]).cwd(dir.path()))
                .unwrap();
        let hourly = render_task_xml(&hourly_job, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(hourly.contains("<Interval>PT60M</Interval>"));
    }

    #[test]
    fn renders_interactive_token_logon_type_when_windows_visible_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(
            options("backup", dir.path()).windows(crate::types::WindowsOptions { visible: true }),
        )
        .unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(!xml.contains("<LogonType>S4U</LogonType>"));
    }

    #[test]
    fn runs_visible_jobs_directly_without_a_powershell_console() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(
            CronOptions::new(
                "backup",
                "0 9 * * *",
                [test_executable(), "agent-one://cron?v=1&id=x".to_string()],
            )
            .cwd(dir.path())
            .windows(crate::types::WindowsOptions { visible: true }),
        )
        .unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(xml.contains(&format!("<Command>{}</Command>", test_executable())));
        assert!(xml.contains("<Arguments>agent-one://cron?v=1&amp;id=x</Arguments>"));
        assert!(!xml.contains("powershell.exe"));

        let wrapped = normalize(
            options("backup", dir.path()).windows(crate::types::WindowsOptions { visible: true }),
        )
        .unwrap();
        let xml = render_task_xml(&wrapped, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(xml.contains("<Command>powershell.exe</Command>"));
        assert!(xml.contains("-WindowStyle Hidden"));

        let hidden = normalize(options("backup", dir.path())).unwrap();
        let xml = render_task_xml(&hidden, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(!xml.contains("-WindowStyle"));
    }

    #[test]
    fn renders_startup_schedules_as_windows_logon_triggers() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(CronOptions::at_startup("backup", [test_executable()]).cwd(dir.path()))
            .unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap();
        assert!(xml.contains("<LogonTrigger>"));
        assert!(!xml.contains("<CalendarTrigger>"));
        assert!(!xml.contains("<UserId>"));
    }

    #[test]
    fn splits_day_of_month_and_weekday_restrictions_to_preserve_or_logic() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(
            CronOptions::new("backup", "0 9 15 JAN MON-FRI", [test_executable()]).cwd(dir.path()),
        )
        .unwrap();
        let xml = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap();
        assert_eq!(xml.matches("<CalendarTrigger>").count(), 2);
        assert!(xml.contains("<ScheduleByMonth>"));
        assert!(xml.contains("<ScheduleByMonthDayOfWeek>"));
        assert!(xml.contains("<Weeks><Week>1</Week><Week>2</Week><Week>3</Week><Week>4</Week><Week>Last</Week></Weeks>"));
    }

    #[test]
    fn rejects_expressions_that_exceed_the_windows_48_trigger_limit() {
        let dir = tempfile::tempdir().unwrap();
        let job = normalize(
            CronOptions::new("backup", "*/7 * * * *", [test_executable()]).cwd(dir.path()),
        )
        .unwrap();
        let error = render_task_xml(&job, "C:\\native-cron\\backup.ps1").unwrap_err();
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
                enabled_clone.store(
                    args.contains(&"/enable"),
                    std::sync::atomic::Ordering::SeqCst,
                );
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
    fn passes_interactive_token_flag_to_schtasks_when_windows_visible_is_set() {
        let dir = tempfile::tempdir().unwrap();
        let seen_args = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_args_clone = seen_args.clone();
        let runner = FakeRunner::new(move |command, args| {
            if command == "schtasks.exe" && args.first() == Some(&"/create") {
                *seen_args_clone.lock().unwrap() = args.iter().map(|arg| arg.to_string()).collect();
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
            runner: Box::new(runner),
        });

        let job = normalize(
            options("backup", dir.path()).windows(crate::types::WindowsOptions { visible: true }),
        )
        .unwrap();
        driver.register(&job).unwrap();

        let args = seen_args.lock().unwrap();
        assert!(args.iter().any(|arg| arg == "/it"));
        assert!(!args.iter().any(|arg| arg == "/np"));
    }

    #[test]
    fn disabling_an_unregistered_job_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let driver = WindowsDriver::new(WindowsContext {
            root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::new(|_, _| ProcessOutput {
                code: 0x8007_0002u32 as i32,
                stdout: String::new(),
                stderr: "Task not found".to_string(),
            })),
        });
        driver.disable("never-registered").unwrap();
    }

    #[test]
    fn run_now_runs_enabled_tasks_and_rejects_disabled_ones() {
        let dir = tempfile::tempdir().unwrap();
        let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let enabled_clone = enabled.clone();
        let (runner, calls) = crate::test_support::recording_runner(move |_, args| {
            if args.first() == Some(&"/query") && args.contains(&"native-cron-missing") {
                return crate::test_support::exit_code(0x8007_0002u32 as i32);
            }
            if args.first() == Some(&"/query") {
                return ProcessOutput {
                    code: 0,
                    stdout: format!(
                        "<Task><Settings><Enabled>{}</Enabled></Settings></Task>",
                        enabled_clone.load(std::sync::atomic::Ordering::SeqCst)
                    ),
                    stderr: String::new(),
                };
            }
            crate::test_support::success()
        });
        let driver = WindowsDriver::new(WindowsContext {
            root: dir.path().to_path_buf(),
            runner: Box::new(runner),
        });

        driver.run_now("backup").unwrap();
        assert!(calls
            .lock()
            .unwrap()
            .contains(&"schtasks.exe /run /tn native-cron-backup".to_string()));

        enabled.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(driver.run_now("backup"), Err(Error::Disabled(_))));
        assert!(matches!(
            driver.run_now("missing"),
            Err(Error::NotRegistered(_))
        ));
    }

    #[test]
    fn reports_run_history_from_get_scheduled_task_info() {
        let info = parse_task_info(
            "LastRunTime=1700000000\r\nLastTaskResult=2147942402\r\nNextRunTime=1700086400\r\n",
        );
        assert_eq!(info.last_run, unix_seconds(1_700_000_000));
        assert_eq!(info.last_exit_code, Some(0x8007_0002u32 as i32));
        assert_eq!(info.next_run, unix_seconds(1_700_086_400));

        let never_ran = parse_task_info("LastRunTime=\nLastTaskResult=267011\nNextRunTime=\n");
        assert_eq!(never_ran, RunInfo::default());

        let script = render_task_info_script("native-cron-backup");
        assert!(script.contains("Get-ScheduledTaskInfo -TaskName 'native-cron-backup'"));
    }

    #[test]
    fn namespaces_task_names_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let (runner, calls) = crate::test_support::recording_runner(|_, args| {
            if args.first() == Some(&"/query") {
                return crate::test_support::exit_code(0x8007_0002u32 as i32);
            }
            crate::test_support::success()
        });
        let driver = WindowsDriver::with_namespace(
            WindowsContext {
                root: dir.path().to_path_buf(),
                runner: Box::new(runner),
            },
            "dev.agent-one",
        );
        let job = normalize(options("backup", dir.path()).namespace("dev.agent-one")).unwrap();
        driver.register(&job).unwrap();

        let status = driver.status("backup").unwrap();
        assert_eq!(
            status.config_paths[0],
            dir.path().join("dev.agent-one").join("backup.xml")
        );
        assert_eq!(
            status.config_paths[1],
            dir.path().join("dev.agent-one").join("backup.ps1")
        );
        assert!(status.config_paths.iter().all(|path| path.exists()));
        assert!(calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("schtasks.exe /create")
                && call.contains("/tn dev.agent-one.backup")));

        let default = WindowsDriver::new(WindowsContext {
            root: dir.path().to_path_buf(),
            runner: Box::new(FakeRunner::always_success()),
        });
        assert_eq!(default.paths("backup").0, dir.path().join("backup.xml"));
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
