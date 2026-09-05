# native-cron

[![crates.io](https://img.shields.io/crates/v/native-cron)](https://crates.io/crates/native-cron)
[![docs.rs](https://img.shields.io/docsrs/native-cron)](https://docs.rs/native-cron)
[![license](https://img.shields.io/crates/l/native-cron)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.98%2B-orange)](https://www.rust-lang.org)

Cross-platform OS-level cron for Rust.

`native-cron` registers commands with the scheduler already built into the operating system: launchd on macOS, systemd user timers on Linux, and Task Scheduler on Windows. There is no daemon, no timer loop, and no resident process. Registration returns immediately, and the OS starts a fresh process whenever the schedule fires. Your program can exit; the job keeps running.

This crate is deliberately not an in-process scheduler. If you want callbacks inside a long-lived server, use a tokio interval task or similar. If you want a command to run at 2am whether or not your program is running, use this.

> Heads up! Some portions of this project are AI-generated.
> This project is inspired by https://github.com/bndnsmth/native-cron.

## Installation

```sh
cargo add native-cron
```

## Quick start

```rust
use native_cron::CronOptions;

fn main() -> native_cron::Result<()> {
    let job = native_cron::register(CronOptions::new(
        "backup",
        "0 2 * * *",
        ["/usr/local/bin/backup"],
    ))?;

    println!("{:?}", job.status()?.state);
    Ok(())
}
```

After `register` returns, this process can exit. The command runs at 02:00 local time, started by launchd, systemd, or Task Scheduler.

## Reference

### `register`

`native_cron::register(CronOptions) -> Result<Job>` writes native configuration for one job and enables it. Only `id`, `command`, and exactly one of `cron` or `run_at_startup` are required:

```rust
use native_cron::CronOptions;

let job = native_cron::register(CronOptions::new(
    "backup",
    "0 2 * * *",
    ["/usr/local/bin/backup"],
))?;
```

| Field            | Type                              | Required   | Meaning                                                                                                                           |
| ---------------- | --------------------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------- |
| `id`             | `String`                          | yes        | Unique name, 1 to 100 characters of letters, numbers, hyphens, and underscores. Unique within `native-cron` on that user account. |
| `command`        | `Vec<String>`                     | yes        | First element is the executable, the rest are its arguments. Must be non-empty.                                                   |
| `cron`           | `Option<String>`                  | one of two | Five-field expression or nickname. Mutually exclusive with `run_at_startup`.                                                      |
| `run_at_startup` | `bool`                            | one of two | Run at user-session start instead of on a schedule. Mutually exclusive with `cron`.                                               |
| `cwd`            | `Option<PathBuf>`                 | no         | Working directory for the command. Must exist at registration time.                                                               |
| `env`            | `Option<HashMap<String, String>>` | no         | Extra environment variables. Names must match `[A-Za-z_][A-Za-z0-9_]*`.                                                           |
| `stdout`         | `Option<PathBuf>`                 | no         | File to append standard output to.                                                                                                |
| `stderr`         | `Option<PathBuf>`                 | no         | File to append standard error to.                                                                                                 |
| `overwrite`      | `bool`                            | no         | Replace an existing job with the same id instead of failing.                                                                      |

Everything optional is written into the native configuration only when you supply it. Nothing is defaulted for you.

`CronOptions` has two constructors and chainable setters matching the fields above:

```rust
use native_cron::CronOptions;

let job = native_cron::register(
    CronOptions::new("backup", "0 2 * * *", ["/usr/local/bin/backup"])
        .cwd("/srv/my-app")
        .env("APP_ENV", "production")
        .stdout("/srv/my-app/logs/backup.log")
        .stderr("/srv/my-app/logs/backup.err.log")
        .overwrite(false),
)?;
```

A startup job (runs when the user's scheduler starts a session, and immediately on registration and re-enable) uses `at_startup` instead of a cron expression:

```rust
use native_cron::CronOptions;

let job = native_cron::register(CronOptions::at_startup("agent", ["/usr/local/bin/agent"]))?;
```

`@reboot` and `@login` are accepted as schedule strings and mean the same thing, but `at_startup` is the preferred spelling.

Registering an `id` that already exists returns `Error::AlreadyExists` unless `overwrite(true)` is set, in which case the existing schedule and command are replaced and the job is restarted.

#### Path resolution

The executable and any relative `cwd`, `stdout`, or `stderr` paths are resolved to absolute paths during registration:

- A value that looks like a path (absolute, or starting with `.`, or containing a separator) is resolved against `cwd` if supplied, or the current directory otherwise. The executable must exist and be a file; on Unix it must have an execute bit.
- A bare executable name such as `backup` is looked up in `PATH`.
- Parent directories of `stdout` and `stderr` targets are created automatically.

### Managing jobs

The `Job` returned by `register` operates on the persistent native job:

```rust
job.enable()?;  // load and start again
job.disable()?; // disable/unload, keeping the configuration
job.remove()?;  // unregister and delete configuration, idempotent
job.status()?;  // report current native state
```

A handle to a job registered by another process can be fetched by id with `native_cron::job`, and a job can be removed outright with `native_cron::remove`:

```rust
let job = native_cron::job("backup")?;
job.disable()?;

native_cron::remove("backup")?;
```

A handle from `native_cron::job` never saw the job's configuration, so its `status()` reports `None` for the configuration fields. A handle from `register` reports everything it registered.

`Job::status` returns a `JobStatus`:

```rust
pub struct JobStatus {
    pub id: String,
    pub platform: Platform, // Darwin | Linux | Windows
    pub state: JobState,    // Active | Inactive | Missing
    pub config_paths: Vec<PathBuf>,
    pub cron: Option<String>,       // normalized expression, if known
    pub run_at_startup: bool,
    pub command: Option<Vec<String>>, // resolved, if known
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    pub stdout: Option<PathBuf>,
    pub stderr: Option<PathBuf>,
}
```

`Active` means registered and enabled, `Inactive` means registered but disabled, and `Missing` means not registered.

### `validate`

`native_cron::validate(CronOptions) -> Result<()>` parses the schedule, resolves paths, and renders the native configuration without writing anything to disk or invoking any native command. Use it to fail fast on a batch of registrations before committing any of them:

```rust
use native_cron::CronOptions;

native_cron::validate(CronOptions::new("backup", "0 2 * * *", ["/usr/local/bin/backup"]))?;
```

### Schedule syntax

Calendar schedules use the standard five-field cron format in the operating system's local time zone:

```txt
minute hour day-of-month month day-of-week
```

| Field        | Values                | Operators       |
| ------------ | --------------------- | --------------- |
| Minute       | `0`-`59`              | `*` `,` `-` `/` |
| Hour         | `0`-`23`              | `*` `,` `-` `/` |
| Day of month | `1`-`31`              | `*` `,` `-` `/` |
| Month        | `1`-`12`, `JAN`-`DEC` | `*` `,` `-` `/` |
| Day of week  | `0`-`7`, `SUN`-`SAT`  | `*` `,` `-` `/` |

Rules, matching standard cron:

- Names are case-insensitive and may be abbreviated or written in full (`mon` and `monday` both work).
- `0` and `7` both mean Sunday.
- Ranges must be ascending. Use a list for wrap-around, for example `22,23,0` rather than `22-0`.
- A step applies from its start value to the field maximum: `5/20` means `5,25,45`, `9-17/2` means `9,11,13,15,17`.
- When day-of-month and day-of-week are both restricted, either match fires the job (the POSIX rule). This is preserved on all three backends.
- There is no seconds field. Six-field expressions are rejected.

Supported nicknames:

| Nickname               | Equivalent  |
| ---------------------- | ----------- |
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly`             | `0 0 1 * *` |
| `@weekly`              | `0 0 * * 0` |
| `@daily`, `@midnight`  | `0 0 * * *` |
| `@hourly`              | `0 * * * *` |

### Errors

All fallible operations return `Result<T>` with `native_cron::Error`:

| Variant                             | When                                                                                                            |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `InvalidId`                         | The id is empty, over 100 characters, or uses characters other than letters, numbers, hyphens, and underscores. |
| `InvalidCronExpression(String)`     | The schedule does not parse. The string says what was wrong.                                                    |
| `EmptyCommand`                      | The command list is empty.                                                                                      |
| `InvalidText`                       | An argument, schedule, or environment value is empty or contains a newline or NUL.                              |
| `InvalidEnvName(String)`            | An environment variable name is not `[A-Za-z_][A-Za-z0-9_]*`.                                                   |
| `MissingCwd(PathBuf)`               | The working directory does not exist.                                                                           |
| `MissingExecutable(PathBuf)`        | The executable path does not exist or is not a file.                                                            |
| `NotExecutable(PathBuf)`            | The executable has no execute bit (Unix).                                                                       |
| `ExecutableNotFound(String)`        | A bare executable name was not found in `PATH`.                                                                 |
| `AmbiguousTrigger`                  | Both `cron` and `run_at_startup` are set.                                                                       |
| `MissingTrigger`                    | Neither `cron` nor `run_at_startup` is set.                                                                     |
| `AlreadyExists(String)`             | The id is registered and `overwrite` was not set.                                                               |
| `NotRegistered(String)`             | `enable` or `disable` was called on an id that is not registered.                                               |
| `TooManyIntervals(usize)`           | The expression expands to more than 10,000 launchd calendar entries.                                            |
| `TooManyWindowsTriggers(usize)`     | The expression needs more than 48 Task Scheduler triggers.                                                      |
| `MissingUserId`                     | The Windows user identity could not be determined.                                                              |
| `UnsupportedPlatform`               | The operating system is not macOS, Linux, or Windows.                                                           |
| `CommandFailed`                     | A native command (`launchctl`, `systemctl`, `schtasks`) exited non-zero.                                        |
| `Spawn`                             | A native command could not be started.                                                                          |
| `Io`, `Plist`, `Xml`, `SystemdUnit` | Filesystem or encoding failures while writing configuration.                                                    |

### Native backends

| Platform | Backend                | Installed configuration                                               |
| -------- | ---------------------- | --------------------------------------------------------------------- |
| macOS    | Per-user launchd agent | `~/Library/LaunchAgents/native-cron.<id>.plist`                       |
| Linux    | Per-user systemd units | `~/.config/systemd/user/native-cron-<id>.{service,timer}`             |
| Windows  | Task Scheduler, S4U    | Task `native-cron-<id>` plus files under `%LOCALAPPDATA%\native-cron` |

Configuration is generated with dedicated format libraries rather than hand-built strings: [`plist`](https://docs.rs/plist) for launchd, [`systemd-unit-edit`](https://docs.rs/systemd-unit-edit) for systemd units, and [`quick-xml`](https://docs.rs/quick-xml) for Task Scheduler task definitions. On Linux, `XDG_CONFIG_HOME` is respected in place of `~/.config`.

#### macOS

The agent uses `StartCalendarInterval` for calendar schedules and `RunAtLoad` for startup jobs. Complex expressions expand into one calendar entry per matching combination of field values, capped at 10,000 entries (`Error::TooManyIntervals`). `disable()` unloads the agent while keeping its plist; `enable()` loads it again.

```sh
launchctl print gui/$(id -u)/native-cron.backup
```

#### Linux

Calendar jobs are a `Type=oneshot` service plus a `Persistent=true` timer. Startup jobs are an enabled service under `default.target` instead, with no timer. Output goes to the user journal unless file paths were configured.

```sh
systemctl --user status native-cron-backup.timer
journalctl --user -u native-cron-backup.service
```

For headless machines where jobs must run after reboot without login:

```sh
loginctl enable-linger "$USER"
```

#### Windows

The task runs a private PowerShell wrapper script under the registering user with S4U logon. No password is stored, and calendar tasks can run while the user is logged out. S4U tasks cannot access Windows-authenticated network resources such as SMB shares or mapped drives.

Task Scheduler permits at most 48 triggers per task. Evenly spaced minute or hour intervals are compressed into a single repetition trigger (`*/15 * * * *` becomes one trigger with `PT15M`), and expressions that would still exceed the limit are rejected with `Error::TooManyWindowsTriggers`.

```powershell
schtasks /query /tn "native-cron-backup" /v
```

### Behavior

- Lifecycle methods report success only after the underlying native command succeeds.
- Commands do not overlap. Native behavior applies: launchd coalesces, systemd does not start a second active oneshot service, and Windows tasks use `IgnoreNew`.
- Missed executions follow the native scheduler: launchd coalesces after wake, systemd timers use `Persistent=true`, and Windows tasks use `StartWhenAvailable`.
- Schedules use local wall-clock time and follow the operating system's daylight saving behavior.
- A `run_at_startup` job runs when registered, when enabled after being disabled, and when the user's scheduler starts a future session.

### Security

- Arguments are passed as native argument arrays or platform-escaped values; no POSIX shell is involved.
- Configuration is written atomically with user-only `0600` permissions on POSIX systems. Windows configuration inherits the current user's `%LOCALAPPDATA%` access controls.
- `env`, arguments, and output paths may contain secrets and are stored on disk in native configuration or the wrapper script. Do not put credentials here unless that persistence is acceptable.
- Scheduled commands run with the registering user's privileges. This crate never elevates.
- Use absolute paths for scripts and data. Native schedulers provide a smaller environment than an interactive terminal.

## Contributing

Development setup, project layout, and testing notes live in [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
