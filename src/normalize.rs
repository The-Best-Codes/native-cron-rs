use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::files::resolve_from;
use crate::schedule::Schedule;
use crate::types::CronOptions;

const ID_PATTERN_MAX_LEN: usize = 100;

fn validate_id_impl(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= ID_PATTERN_MAX_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidId)
    }
}

/// Validates a job id. Exposed for callers that only have an id, such as
/// [`crate::job`] and [`crate::remove`].
pub fn validate_id(id_public: &str) -> Result<()> {
    validate_id_impl(id_public)
}

fn validate_text(value: &str, label: &'static str) -> Result<()> {
    if value.is_empty() || value.contains('\0') || value.contains('\r') || value.contains('\n') {
        Err(Error::InvalidText { label })
    } else {
        Ok(())
    }
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .map(|c| c.is_ascii_alphabetic() || c == '_')
        .unwrap_or(false);
    let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if first_ok && rest_ok {
        Ok(())
    } else {
        Err(Error::InvalidEnvName(name.to_string()))
    }
}

/// A fully validated, resolved job ready for a platform driver to register.
///
/// Every optional field mirrors [`CronOptions`]: it is only `Some` when the
/// caller supplied it, so drivers only add the corresponding native
/// configuration (working directory, environment, output redirection) when
/// asked to.
#[derive(Debug, Clone)]
pub struct NormalizedJob {
    pub id: String,
    /// The resolved command: the executable is an absolute path.
    pub command: Vec<String>,
    pub schedule: Schedule,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
    pub overwrite: bool,
}

/// Validates and resolves [`CronOptions`] into a [`NormalizedJob`].
///
/// Relative executable, `stdout`, and `stderr` paths are resolved against
/// `cwd` if one was supplied, or the current directory otherwise. The
/// resolved `cwd` is only kept in the normalized job (and therefore only
/// written into native configuration) if the caller supplied one explicitly.
pub fn normalize(options: CronOptions) -> Result<NormalizedJob> {
    validate_id_impl(&options.id)?;

    let schedule = match (&options.cron, options.run_at_startup) {
        (Some(_), true) => return Err(Error::AmbiguousTrigger),
        (None, false) => return Err(Error::MissingTrigger),
        (Some(cron), false) => {
            validate_text(cron, "Cron schedule")?;
            Schedule::parse(cron)?
        }
        (None, true) => Schedule::Startup,
    };

    if options.command.is_empty() {
        return Err(Error::EmptyCommand);
    }
    for argument in &options.command {
        validate_text(argument, "Command argument")?;
    }

    let current_dir = env::current_dir()?;
    let resolution_base: PathBuf = match &options.cwd {
        Some(cwd) => resolve_from(&current_dir, cwd),
        None => current_dir,
    };
    if !resolution_base.is_dir() {
        return Err(Error::MissingCwd(resolution_base));
    }

    let executable = &options.command[0];
    let resolved_executable = resolve_executable(executable, &resolution_base)?;
    let mut command = vec![path_to_string(&resolved_executable)];
    command.extend(options.command[1..].iter().cloned());

    let mut env = HashMap::new();
    if let Some(supplied) = options.env {
        for (key, value) in supplied {
            validate_env_name(&key)?;
            validate_text(&value, "Environment variable value")?;
            env.insert(key, value);
        }
    }

    let stdout = options
        .stdout
        .as_ref()
        .map(|path| resolve_from(&resolution_base, path));
    let stderr = options
        .stderr
        .as_ref()
        .map(|path| resolve_from(&resolution_base, path));

    Ok(NormalizedJob {
        id: options.id,
        command,
        schedule,
        cwd: options.cwd.map(|cwd| resolve_from(&resolution_base, &cwd)),
        env,
        stdout,
        stderr,
        overwrite: options.overwrite,
    })
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn looks_like_path(executable: &str) -> bool {
    Path::new(executable).is_absolute()
        || executable.starts_with('.')
        || executable.contains(std::path::MAIN_SEPARATOR)
        || executable.contains('/')
        || executable.contains('\\')
}

fn resolve_executable(executable: &str, base: &Path) -> Result<PathBuf> {
    if looks_like_path(executable) {
        let candidate = resolve_from(base, Path::new(executable));
        validate_executable(&candidate)
    } else {
        which::which_in(executable, env::var_os("PATH"), base)
            .map_err(|_| Error::ExecutableNotFound(executable.to_string()))
    }
}

fn validate_executable(path: &Path) -> Result<PathBuf> {
    let metadata =
        std::fs::metadata(path).map_err(|_| Error::MissingExecutable(path.to_path_buf()))?;
    if !metadata.is_file() {
        return Err(Error::MissingExecutable(path.to_path_buf()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(Error::NotExecutable(path.to_path_buf()));
        }
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_options(id: &str) -> CronOptions {
        CronOptions::new(
            id,
            "@daily",
            [std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
        )
    }

    #[test]
    fn rejects_invalid_ids() {
        let mut options = valid_options("../bad");
        options.id = "../bad".to_string();
        assert!(matches!(normalize(options), Err(Error::InvalidId)));
    }

    #[test]
    fn rejects_malformed_schedules() {
        let mut options = valid_options("bad-schedule");
        options.cron = Some("* * *".to_string());
        assert!(matches!(
            normalize(options),
            Err(Error::InvalidCronExpression(_))
        ));
    }

    #[test]
    fn rejects_invalid_environment_variable_names() {
        let mut options = valid_options("bad-env");
        options.env = Some(HashMap::from([("A-B".to_string(), "x".to_string())]));
        assert!(matches!(normalize(options), Err(Error::InvalidEnvName(_))));
    }

    #[test]
    fn rejects_missing_working_directories() {
        let mut options = valid_options("bad-cwd");
        options.cwd = Some(PathBuf::from("/does/not/exist"));
        assert!(matches!(normalize(options), Err(Error::MissingCwd(_))));
    }

    #[test]
    fn rejects_ambiguous_and_missing_triggers() {
        let mut ambiguous = valid_options("ambiguous");
        ambiguous.run_at_startup = true;
        assert!(matches!(normalize(ambiguous), Err(Error::AmbiguousTrigger)));

        let mut missing = valid_options("missing");
        missing.cron = None;
        assert!(matches!(normalize(missing), Err(Error::MissingTrigger)));
    }

    #[test]
    fn normalizes_login_schedules_to_the_reboot_alias() {
        let mut options = valid_options("login-task");
        options.cron = Some("@login".to_string());
        let job = normalize(options).unwrap();
        assert_eq!(job.schedule.normalized(), "@reboot");
    }
}
