#![cfg(test)]

use std::sync::Mutex;

use crate::error::Result;
use crate::process::{CommandRunner, ProcessOutput};

type Responder = dyn Fn(&str, &[&str]) -> ProcessOutput + Send + Sync;

pub struct FakeRunner {
    commands: Mutex<Vec<(String, Vec<String>)>>,
    respond: Box<Responder>,
}

impl FakeRunner {
    pub fn new(respond: impl Fn(&str, &[&str]) -> ProcessOutput + Send + Sync + 'static) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            respond: Box::new(respond),
        }
    }

    pub fn always_success() -> Self {
        Self::new(|_, _| success())
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, command: &str, args: &[&str]) -> Result<ProcessOutput> {
        self.commands.lock().unwrap().push((
            command.to_string(),
            args.iter().map(|arg| arg.to_string()).collect(),
        ));
        Ok((self.respond)(command, args))
    }
}

pub fn success() -> ProcessOutput {
    ProcessOutput {
        code: 0,
        stdout: String::new(),
        stderr: String::new(),
    }
}
