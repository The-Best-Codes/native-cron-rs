# native-cron

> **Cross-platform OS-level cron for Rust.**
>
> No daemon. No timers. No resident process.

`native-cron` registers commands with the scheduler already built into the operating system. Registration returns immediately; launchd, systemd, or Windows Task Scheduler starts a fresh process when the schedule fires.

```sh
cargo add native-cron
```

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

After that call returns, this process can exit. The OS scheduler runs the command later, in a fresh process.

`native-cron` is intentionally not an in-process scheduling library. If the operating system is not scheduling the command, it is outside this crate's scope.

## Why

Popular Rust scheduling crates run callbacks from a long-lived process (e.g. a tokio task woken on an interval). That's useful for servers, but it means your application is also the scheduler, and stops scheduling the moment it exits.

`native-cron` instead hands the job to the OS. Nothing needs to keep running for the job to fire.

## API

### Register

[`register`] registers one job. Only `id`, `command`, and exactly one of `cron`/`run_at_startup` are required:

```rust
use native_cron::CronOptions;

let job = native_cron::register(CronOptions::new(
    "backup",
    "0 2 * * *",
    ["/usr/local/bin/backup"],
))?;
# Ok::<(), native_cron::Error>(())
```

Registering an `id` that already exists returns [`Error::AlreadyExists`] unless you opt in to replacing it:

```rust
use native_cron::CronOptions;

let job = native_cron::register(
    CronOptions::new("backup", "0 2 * * *", ["/usr/local/bin/backup"]).overwrite(true),
)?;
# Ok::<(), native_cron::Error>(())
```

The executable is resolved to an absolute path during registration (via `PATH` if given as a bare name). Relative `cwd`, `stdout`, and `stderr` paths are resolved from the working directory in effect at registration time, or from an explicit `cwd` if one is supplied.

`cwd`, `env`, `stdout`, and `stderr` are all optional. Each is only written into the native configuration (plist, systemd unit, or Task Scheduler XML/PowerShell wrapper) when you supply it — nothing is defaulted in for you:

```rust
use native_cron::CronOptions;

let job = native_cron::register(
    CronOptions::new("backup", "0 2 * * *", ["/usr/local/bin/backup"])
        .cwd("/srv/my-app")
        .env("APP_ENV", "production")
        .stdout("/srv/my-app/logs/backup.log")
        .stderr("/srv/my-app/logs/backup.err.log"),
)?;
# Ok::<(), native_cron::Error>(())
```

A job that should run at user-session startup (instead of on a calendar schedule) uses `CronOptions::at_startup` instead of a cron expression:

```rust
use native_cron::CronOptions;

let job = native_cron::register(CronOptions::at_startup("agent", ["/usr/local/bin/agent"]))?;
# Ok::<(), native_cron::Error>(())
```

### Lifecycle

The returned [`Job`] operates on the persistent native job, using standard OS terminology:

```rust
# use native_cron::CronOptions;
# let job = native_cron::register(CronOptions::new("backup", "@daily", ["/bin/true"]).overwrite(true))?;
job.disable()?; // disable/unload, preserving configuration
job.enable()?; // enable/load again
job.remove()?; // unregister and delete configuration
# Ok::<(), native_cron::Error>(())
```

Retrieve a handle in another process by id with [`native_cron::job`], or remove one outright with [`native_cron::remove`]:

```rust
let job = native_cron::job("backup")?;
job.disable()?;
job.enable()?;

native_cron::remove("backup")?;
# Ok::<(), native_cron::Error>(())
```

A handle from [`native_cron::job`] doesn't know the job's schedule or command (it was never given them), so [`Job::status`] omits those fields; a handle from [`register`] knows everything it registered.

[`Job::status`] returns a [`JobStatus`]:

```rust
pub struct JobStatus {
    pub id: String,
    pub platform: Platform,
    pub state: JobState, // Active | Inactive | Missing
    pub config_paths: Vec<std::path::PathBuf>,
    pub cron: Option<String>,
    pub run_at_startup: bool,
    pub command: Option<Vec<String>>,
    pub cwd: Option<std::path::PathBuf>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub stdout: Option<std::path::PathBuf>,
    pub stderr: Option<std::path::PathBuf>,
}
```

### Validating without registering

[`validate`] checks that a [`CronOptions`] can be turned into native configuration — including resolving the executable and rendering the plist/unit/task XML — without writing anything or calling any native command:

```rust
use native_cron::CronOptions;

native_cron::validate(CronOptions::new("backup", "0 2 * * *", ["/usr/local/bin/backup"]))?;
# Ok::<(), native_cron::Error>(())
```

## Schedule Syntax

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

Names are case-insensitive and may be abbreviated or written in full. Both `0` and `7` mean Sunday.

Supported nicknames:

| Nickname               | Equivalent  |
| ---------------------- | ----------- |
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly`             | `0 0 1 * *` |
| `@weekly`              | `0 0 * * 0` |
| `@daily`, `@midnight`  | `0 0 * * *` |
| `@hourly`              | `0 * * * *` |

`@reboot` and `@login` are recognized as aliases when passed to `cron`, but the preferred way to express a startup job is `CronOptions::at_startup`, which sets `run_at_startup: true` directly rather than smuggling it through the schedule string.

When day-of-month and day-of-week are both restricted, either match fires the job. This is standard POSIX cron behavior and is preserved on all three backends.

## Native Backends

| Platform | Backend                   | Installed configuration                                                  |
| -------- | -------------------------- | ------------------------------------------------------------------------ |
| macOS    | Per-user launchd agent     | `~/Library/LaunchAgents/native-cron.<id>.plist`                          |
| Linux    | Per-user systemd timer     | `~/.config/systemd/user/native-cron-<id>.{service,timer}`                |
| Windows  | Task Scheduler, S4U logon  | Task `native-cron-<id>` plus files under `%LOCALAPPDATA%\native-cron`    |

Native configuration is generated with dedicated format libraries rather than hand-built strings: [`plist`](https://docs.rs/plist) for launchd, [`systemd-unit-edit`](https://docs.rs/systemd-unit-edit) for systemd units, and [`quick-xml`](https://docs.rs/quick-xml) for the Task Scheduler task definition.

### macOS

The launchd agent uses `StartCalendarInterval` for calendar schedules and `RunAtLoad` for startup jobs, plus an argument array and, only when supplied, a working directory, environment variables, and output redirection. `disable()` disables and unloads the agent while retaining its plist. `enable()` loads it again.

```sh
launchctl print gui/$(id -u)/native-cron.backup
```

### Linux

For calendar schedules, a `Type=oneshot` service and a `Persistent=true` timer are written. For startup jobs, an enabled user service under `default.target` is written instead. stdout and stderr go to the user journal unless file paths are configured.

```sh
systemctl --user status native-cron-backup.timer
journalctl --user -u native-cron-backup.service
```

For headless jobs that must run before login after reboot:

```sh
loginctl enable-linger "$USER"
```

### Windows

Task Scheduler receives an XML task definition with calendar triggers or a logon trigger, running a private PowerShell wrapper under the registering user with S4U logon. No password is stored; calendar tasks can run while the user is logged out. S4U tasks cannot access Windows-authenticated network resources such as SMB shares or mapped drives.

Task Scheduler permits at most 48 triggers per task. `native-cron` compresses regular minute/hour intervals into one repetition trigger and rejects expressions whose expansion would exceed that limit ([`Error::TooManyWindowsTriggers`]).

```powershell
schtasks /query /tn "native-cron-backup" /v
```

## Operational Semantics

- Registration and lifecycle methods succeed only after the underlying native command succeeds.
- A job id contains only letters, numbers, hyphens, and underscores and is unique within `native-cron` on that user account.
- Registering an existing id without `overwrite: true` returns [`Error::AlreadyExists`]; with it, the existing schedule and command are replaced and the job restarted.
- `disable()` preserves configuration; `remove()` deletes it.
- Commands do not overlap by crate-level coordination. Native behavior applies: launchd coalesces demand, systemd does not start a second active oneshot service, and Windows uses `IgnoreNew`.
- Missed executions follow the native scheduler: launchd coalesces after wake, systemd timers use `Persistent=true`, and Windows tasks use `StartWhenAvailable`.
- Schedules use local wall-clock time and therefore follow operating-system daylight-saving behavior.
- A `run_at_startup` job runs when registered, when enabled after being disabled, and when the user's native scheduler starts in a future session.

## Security

- Command arguments are passed as native argument arrays or platform-specific escaped values; no POSIX shell is involved.
- Configuration is written atomically with user-only `0600` permissions where the platform supports POSIX modes.
- Windows configuration inherits the current user's `%LOCALAPPDATA%` access controls.
- `env`, arguments, and output paths may contain secrets and are stored on disk in native configuration or the private wrapper. Do not put credentials in this API unless that persistence is acceptable.
- Scheduled commands run with the registering user's privileges. `native-cron` does not elevate privileges.
- Use absolute script/data paths. Native schedulers provide a smaller environment than an interactive terminal.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
```

## License

MIT.
