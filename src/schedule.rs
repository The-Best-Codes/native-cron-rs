//! Parsing for the standard five-field cron expression syntax, plus the
//! `@reboot`/`@login` startup alias and common nicknames.

use crate::error::{Error, Result};

const NICKNAMES: &[(&str, &str)] = &[
    ("@yearly", "0 0 1 1 *"),
    ("@annually", "0 0 1 1 *"),
    ("@monthly", "0 0 1 * *"),
    ("@weekly", "0 0 * * 0"),
    ("@daily", "0 0 * * *"),
    ("@midnight", "0 0 * * *"),
    ("@hourly", "0 * * * *"),
];

const MONTH_NAMES: &[(&str, u32)] = &[
    ("jan", 1),
    ("january", 1),
    ("feb", 2),
    ("february", 2),
    ("mar", 3),
    ("march", 3),
    ("apr", 4),
    ("april", 4),
    ("may", 5),
    ("jun", 6),
    ("june", 6),
    ("jul", 7),
    ("july", 7),
    ("aug", 8),
    ("august", 8),
    ("sep", 9),
    ("september", 9),
    ("oct", 10),
    ("october", 10),
    ("nov", 11),
    ("november", 11),
    ("dec", 12),
    ("december", 12),
];

const WEEKDAY_NAMES: &[(&str, u32)] = &[
    ("sun", 0),
    ("sunday", 0),
    ("mon", 1),
    ("monday", 1),
    ("tue", 2),
    ("tuesday", 2),
    ("wed", 3),
    ("wednesday", 3),
    ("thu", 4),
    ("thursday", 4),
    ("fri", 5),
    ("friday", 5),
    ("sat", 6),
    ("saturday", 6),
];

/// A single field of a parsed calendar cron expression (e.g. the minute field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronField {
    /// The sorted, de-duplicated set of values this field matches.
    pub values: Vec<u32>,
    /// Whether the field was written as a bare wildcard (`*` or `*/1`).
    pub wildcard: bool,
}

/// A parsed calendar cron expression: `minute hour day-of-month month day-of-week`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarSchedule {
    /// Minute field (`0`–`59`).
    pub minute: CronField,
    /// Hour field (`0`–`23`).
    pub hour: CronField,
    /// Day-of-month field (`1`–`31`).
    pub day_of_month: CronField,
    /// Month field (`1`–`12`).
    pub month: CronField,
    /// Day-of-week field (`0`–`6`, Sunday = `0`).
    pub day_of_week: CronField,
    /// Canonical five-field expression for this schedule.
    pub normalized: String,
}

/// A parsed cron expression: either a calendar schedule or the `@reboot` startup alias.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// A five-field calendar expression.
    Calendar(CalendarSchedule),
    /// Run at user-session start (`@reboot` / `@login`).
    Startup,
}

impl Schedule {
    /// Parses a cron expression, applying nicknames and the `@reboot`/`@login` startup alias.
    pub fn parse(expression: &str) -> Result<Self> {
        let trimmed = expression.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower == "@reboot" || lower == "@login" {
            return Ok(Schedule::Startup);
        }

        let expanded = NICKNAMES
            .iter()
            .find(|(nickname, _)| *nickname == lower)
            .map(|(_, value)| *value)
            .unwrap_or(trimmed);

        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            let message = if fields.len() > 5 {
                "expected 5 fields; seconds are not supported".to_string()
            } else {
                "expected 5 fields (minute hour day month weekday)".to_string()
            };
            return Err(Error::InvalidCronExpression(message));
        }

        let minute = parse_field(fields[0], "minute", 0, 59, None, false)?;
        let hour = parse_field(fields[1], "hour", 0, 23, None, false)?;
        let day_of_month = parse_field(fields[2], "day of month", 1, 31, None, false)?;
        let month = parse_field(fields[3], "month", 1, 12, Some(MONTH_NAMES), false)?;
        let day_of_week = parse_field(fields[4], "day of week", 0, 7, Some(WEEKDAY_NAMES), true)?;

        let normalized = [&minute, &hour, &day_of_month, &month, &day_of_week]
            .iter()
            .map(|field| format_field(field))
            .collect::<Vec<_>>()
            .join(" ");

        Ok(Schedule::Calendar(CalendarSchedule {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            normalized,
        }))
    }

    /// The normalized string form: `@reboot` for startup schedules, or the
    /// normalized five-field calendar expression otherwise.
    pub fn normalized(&self) -> String {
        match self {
            Schedule::Startup => "@reboot".to_string(),
            Schedule::Calendar(calendar) => calendar.normalized.clone(),
        }
    }
}

fn format_field(field: &CronField) -> String {
    if field.wildcard {
        "*".to_string()
    } else {
        field
            .values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_number(
    value: &str,
    label: &str,
    min: u32,
    max: u32,
    names: Option<&[(&str, u32)]>,
) -> Result<u32> {
    let named = names.and_then(|names| {
        let lower = value.to_ascii_lowercase();
        names
            .iter()
            .find(|(name, _)| *name == lower)
            .map(|(_, number)| *number)
    });

    let parsed = match named {
        Some(number) => Some(number),
        None => {
            if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() {
                value.parse::<u32>().ok()
            } else {
                None
            }
        }
    };

    match parsed {
        Some(number) if number >= min && number <= max => Ok(number),
        _ => Err(Error::InvalidCronExpression(format!(
            "{label} value '{value}' must be between {min} and {max}"
        ))),
    }
}

fn add_range(
    values: &mut Vec<u32>,
    start: u32,
    end: u32,
    step: u32,
    label: &str,
    sunday_alias: bool,
) -> Result<()> {
    if start > end {
        return Err(Error::InvalidCronExpression(format!(
            "{label} range must be ascending (use a list for wrap-around)"
        )));
    }
    let mut value = start;
    while value <= end {
        values.push(if sunday_alias && value == 7 { 0 } else { value });
        value += step;
    }
    Ok(())
}

fn parse_field(
    source: &str,
    label: &str,
    min: u32,
    max: u32,
    names: Option<&[(&str, u32)]>,
    sunday_alias: bool,
) -> Result<CronField> {
    let mut values: Vec<u32> = Vec::new();

    for part in source.split(',') {
        if part.is_empty() {
            return Err(Error::InvalidCronExpression(format!(
                "empty {label} list item"
            )));
        }

        let step_parts: Vec<&str> = part.split('/').collect();
        if step_parts.len() > 2 || step_parts[0].is_empty() {
            return Err(Error::InvalidCronExpression(format!(
                "invalid {label} step"
            )));
        }

        let step_source = step_parts.get(1).copied();
        if let Some(step_str) = step_source {
            if step_str.is_empty() || !step_str.chars().all(|c| c.is_ascii_digit()) {
                return Err(Error::InvalidCronExpression(format!(
                    "{label} step must be a positive integer"
                )));
            }
        }
        let step: u32 = match step_source {
            None => 1,
            Some(step_str) => step_str.parse().map_err(|_| {
                Error::InvalidCronExpression(format!("{label} step must be a positive integer"))
            })?,
        };
        if step == 0 {
            return Err(Error::InvalidCronExpression(format!(
                "{label} step must be a positive integer"
            )));
        }

        let base = step_parts[0];
        if base == "*" {
            add_range(&mut values, min, max, step, label, sunday_alias)?;
            continue;
        }

        let range_parts: Vec<&str> = base.split('-').collect();
        if range_parts.len() > 2 || range_parts.iter().any(|value| value.is_empty()) {
            return Err(Error::InvalidCronExpression(format!(
                "invalid {label} range"
            )));
        }

        let start = parse_number(range_parts[0], label, min, max, names)?;
        let end = if range_parts.len() == 1 {
            if step_source.is_none() {
                start
            } else {
                max
            }
        } else {
            parse_number(range_parts[1], label, min, max, names)?
        };
        add_range(&mut values, start, end, step, label, sunday_alias)?;
    }

    values.sort_unstable();
    values.dedup();
    let wildcard = source == "*" || source == "*/1";

    Ok(CronField { values, wildcard })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_the_supported_five_field_syntax() {
        let schedule = Schedule::parse("*/15 9-17 * JAN,JUN MON-FRI").unwrap();
        let calendar = match schedule {
            Schedule::Calendar(calendar) => calendar,
            _ => panic!("expected a calendar schedule"),
        };

        assert_eq!(calendar.minute.values, vec![0, 15, 30, 45]);
        assert_eq!(
            calendar.hour.values,
            vec![9, 10, 11, 12, 13, 14, 15, 16, 17]
        );
        assert_eq!(calendar.month.values, vec![1, 6]);
        assert_eq!(calendar.day_of_week.values, vec![1, 2, 3, 4, 5]);
        assert_eq!(
            calendar.normalized,
            "0,15,30,45 9,10,11,12,13,14,15,16,17 * 1,6 1,2,3,4,5"
        );
    }

    #[test]
    fn supports_nicknames_full_names_lists_ranges_and_sunday_aliases() {
        assert_eq!(
            Schedule::parse("@annually").unwrap().normalized(),
            "0 0 1 1 *"
        );
        assert_eq!(
            Schedule::parse("0 2 * January Sunday,7")
                .unwrap()
                .normalized(),
            "0 2 * 1 0"
        );
        assert_eq!(
            Schedule::parse("5/20 * * * *").unwrap().normalized(),
            "5,25,45 * * * *"
        );
        assert_eq!(
            Schedule::parse("0 0 15 * 0-6").unwrap().normalized(),
            "0 0 15 * 0,1,2,3,4,5,6"
        );
        let wildcard = Schedule::parse("0 0 */1 * MON").unwrap();
        match wildcard {
            Schedule::Calendar(calendar) => assert!(calendar.day_of_month.wildcard),
            _ => panic!("expected a calendar schedule"),
        }
    }

    #[test]
    fn supports_reboot_and_login_as_aliases_for_startup() {
        assert_eq!(Schedule::parse("@reboot").unwrap(), Schedule::Startup);
        assert_eq!(Schedule::parse("@LOGIN").unwrap(), Schedule::Startup);
    }

    #[test]
    fn rejects_malformed_out_of_range_and_six_field_expressions() {
        assert!(Schedule::parse("0 0 * *").is_err());
        assert!(Schedule::parse("0 0 0 * *").is_err());
        assert!(Schedule::parse("0 0 * * 5-1").is_err());
        assert!(Schedule::parse("0 0 0 * * *").is_err());
        assert!(Schedule::parse("*/0 * * * *").is_err());
        assert!(Schedule::parse("*/1e1 * * * *").is_err());
        assert!(Schedule::parse("*/0x10 * * * *").is_err());
    }
}
