use std::process::Command;

use crate::error::{Error, Result};

/// The result of running a native command.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// Process exit code, or `1` if the process was terminated by a signal.
    pub code: i32,
    /// Captured standard output, decoded as UTF-8 or UTF-16.
    pub stdout: String,
    /// Captured standard error, decoded as UTF-8 or UTF-16.
    pub stderr: String,
}

/// Abstraction over running native commands, so drivers can be tested without
/// touching the real operating system.
pub trait CommandRunner: Send + Sync {
    /// Runs `command` with `args` and returns its output.
    fn run(&self, command: &str, args: &[&str]) -> Result<ProcessOutput>;
}

/// The default [`CommandRunner`] that spawns real child processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<ProcessOutput> {
        let output = Command::new(command)
            .args(args)
            .output()
            .map_err(|source| Error::Spawn {
                command: command.to_string(),
                source,
            })?;

        Ok(ProcessOutput {
            code: output.status.code().unwrap_or(1),
            stdout: decode_process_output(&output.stdout),
            stderr: decode_process_output(&output.stderr),
        })
    }
}

/// Decodes process output that might be UTF-8 or UTF-16 (with or without a
/// byte-order mark), which some Windows tools emit.
pub fn decode_process_output(buffer: &[u8]) -> String {
    if buffer.len() >= 2 && buffer[0] == 0xff && buffer[1] == 0xfe {
        return decode_utf16le(&buffer[2..]);
    }
    if buffer.len() >= 2 && buffer[0] == 0xfe && buffer[1] == 0xff {
        let swapped: Vec<u8> = buffer[2..]
            .chunks(2)
            .flat_map(|pair| {
                if pair.len() == 2 {
                    vec![pair[1], pair[0]]
                } else {
                    vec![pair[0]]
                }
            })
            .collect();
        return decode_utf16le(&swapped);
    }
    if buffer.len() >= 4 && buffer[1] == 0 && buffer[3] == 0 {
        return decode_utf16le(buffer);
    }
    String::from_utf8_lossy(buffer).into_owned()
}

fn decode_utf16le(buffer: &[u8]) -> String {
    let units: Vec<u16> = buffer
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Runs a command and returns an error if it exits with a non-zero status.
pub fn run_checked(
    runner: &dyn CommandRunner,
    command: &str,
    args: &[&str],
) -> Result<ProcessOutput> {
    let output = runner.run(command, args)?;
    if output.code != 0 {
        let detail = if !output.stderr.trim().is_empty() {
            output.stderr.trim().to_string()
        } else if !output.stdout.trim().is_empty() {
            output.stdout.trim().to_string()
        } else {
            format!("exit code {}", output.code)
        };
        return Err(Error::CommandFailed {
            command: format!("{command} {}", args.join(" ")),
            detail,
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf8_and_windows_utf16_process_output() {
        assert_eq!(decode_process_output("hello".as_bytes()), "hello");

        let mut with_bom = vec![0xff, 0xfe];
        with_bom.extend(
            "<Enabled>false</Enabled>"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        assert_eq!(decode_process_output(&with_bom), "<Enabled>false</Enabled>");

        let without_bom: Vec<u8> = "<Enabled>false</Enabled>"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            decode_process_output(&without_bom),
            "<Enabled>false</Enabled>"
        );
    }
}
