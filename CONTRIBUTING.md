# Contributing to native-cron

Thanks for helping out. This document covers building, testing, and the layout of the codebase.

## Building

```sh
cargo build
```

The crate compiles the platform driver that matches the host OS only. Cross-platform correctness comes from unit tests that exercise all three drivers with fakes, so builds on any single OS cover most of the logic.

## Testing

```sh
cargo test
cargo clippy --all-targets
```

Tests never touch the real scheduler. Every driver takes a `CommandRunner` trait object, and tests inject a `FakeRunner` (see `src/test_support.rs`) that records calls and returns canned output. Platform contexts such as the home directory or config root are likewise injectable (`DarwinContext`, `LinuxContext`, `WindowsContext`), so tests write to tempdirs from `tempfile` instead of your real configuration directories.

Plist, systemd unit, and task XML rendering functions (`render_plist`, `render_service`, `render_timer`, `render_task_xml`) are pure and are tested by asserting on their output strings. If you change what gets written to native configuration, update those assertions.

When adding a feature, add tests at the level that can run on the machine you have: parsing and normalization, rendering in the driver, and lifecycle behavior against the `FakeRunner`.

## Conventions

- Errors are defined in `src/error.rs` and returned as `Result<T, Error>`; add a variant there rather than stringifying failures.
- Optional `CronOptions` fields must stay truly optional end to end: drivers only write native configuration for fields the caller supplied, and nothing gets a default.
- The lifecycle uses standard OS terminology (`enable`/`disable`/`remove`). Keep new operations in those terms.
- Any change to what lands in native configuration should come with a rendering test for each affected backend.

## License

By contributing you agree that your contributions are licensed under the MIT license.
