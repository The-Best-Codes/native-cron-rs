use std::env;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use native_cron::CronOptions;

const JOB_ID: &str = "daily_file_example";
const OUTPUT_FILE: &str = "daily-times.txt";
const APPEND_FILE_FLAG: &str = "--append-file";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();

    if let Some(path) = append_file_mode(&args)? {
        append_current_time(&path)?;
        return Ok(());
    }

    match args.as_slice() {
        [flag, time] if flag == "--time" => register_daily_job(time)?,
        [flag] if flag == "--remove-cron" => remove_daily_job()?,
        _ => print_usage(),
    }

    Ok(())
}

fn append_file_mode(args: &[String]) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    match args {
        [flag, path] if flag == APPEND_FILE_FLAG => Ok(Some(PathBuf::from(path))),
        [flag] if flag == APPEND_FILE_FLAG => {
            Err(invalid_input("missing path after --append-file").into())
        }
        _ => Ok(None),
    }
}

fn register_daily_job(time: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (hour, minute) = parse_time(time)?;
    let cron = format!("{minute} {hour} * * *");
    let current_dir = env::current_dir()?;
    let output_path = current_dir.join(OUTPUT_FILE);
    let example_executable = env::current_exe()?;

    let job = native_cron::register(
        CronOptions::new(
            JOB_ID,
            cron,
            [
                example_executable.to_string_lossy().into_owned(),
                APPEND_FILE_FLAG.to_string(),
                output_path.to_string_lossy().into_owned(),
            ],
        )
        .overwrite(true),
    )?;

    let status = job.status()?;
    println!("registered {JOB_ID} for {time} every day");
    println!("file: {}", output_path.display());
    println!("platform: {}", status.platform);
    Ok(())
}

fn remove_daily_job() -> Result<(), Box<dyn std::error::Error>> {
    native_cron::remove(JOB_ID)?;
    println!("removed {JOB_ID}");
    Ok(())
}

fn parse_time(input: &str) -> Result<(u8, u8), Box<dyn std::error::Error>> {
    let (hour, minute) = input
        .split_once(':')
        .ok_or_else(|| invalid_input("time must be in HH:MM format"))?;

    let hour = hour
        .parse::<u8>()
        .map_err(|_| invalid_input("hour must be a number from 0 to 23"))?;
    let minute = minute
        .parse::<u8>()
        .map_err(|_| invalid_input("minute must be a number from 0 to 59"))?;

    if hour > 23 || minute > 59 {
        return Err(invalid_input("time must be in HH:MM format using a 24-hour clock").into());
    }

    Ok((hour, minute))
}

fn append_current_time(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", current_time_string())?;
    Ok(())
}

fn current_time_string() -> String {
    local_time_from_os().unwrap_or_else(fallback_time_string)
}

fn local_time_from_os() -> Option<String> {
    #[cfg(windows)]
    {
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz'",
            ])
            .output()
            .ok()?;

        output_to_string(output.stdout)
    }

    #[cfg(not(windows))]
    {
        let output = Command::new("date")
            .args(["+%Y-%m-%d %H:%M:%S %Z"])
            .output()
            .ok()?;

        output_to_string(output.stdout)
    }
}

fn output_to_string(bytes: Vec<u8>) -> Option<String> {
    let text = String::from_utf8(bytes).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn fallback_time_string() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("unix-seconds={}", duration.as_secs()),
        Err(_) => "time-unavailable".to_string(),
    }
}

fn print_usage() {
    println!("register a daily file-writing cron in the current directory");
    println!();
    println!("usage:");
    println!("  cargo run --example daily_file -- --time HH:MM");
    println!("  cargo run --example daily_file -- --remove-cron");
    println!();
    println!("this writes to ./{OUTPUT_FILE} at the requested local time every day");
}

fn invalid_input(message: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}
