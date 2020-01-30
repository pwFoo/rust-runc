/*
 * Copyright 2020 fsyncd, Berlin, Germany.
 * Additional material, copyright of the containerd authors.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use crate::events::{Event, Stats};
use crate::specs::{LinuxResources, Process};
use chrono::{DateTime, Utc};
use futures::ready;
use futures::task::{Context, Poll};
use log::warn;
use serde::{Deserialize, Serialize};
use snafu::{ensure, OptionExt, ResultExt, Snafu};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::iter::FromIterator;
use std::path::PathBuf;
use std::process::Stdio;
use std::string::FromUtf8Error;
use std::time::Duration;
use std::{env, fs, io};
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::macros::support::Pin;
use tokio::process::Child;
use tokio::process::Command;
use tokio::stream::Stream;
use tokio::stream::StreamExt;
use tokio::time::timeout;
use uuid::Uuid;

/// Container PTY terminal
pub mod console;
/// Container events
pub mod events;
/// OCI runtime specification
pub mod specs;

/// Results of top command
pub type TopResults = Vec<HashMap<String, String>>;

/// Runc client error
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Unable to locate the runc binary"))]
    RuncNotFoundError {},

    #[snafu(display("Invalid path: {}", source))]
    InvalidPathError { source: io::Error },

    #[snafu(display("Unable to spawn process: {}", source))]
    ProcessSpawnError { source: io::Error },

    #[snafu(display("Runc command error: {}", source))]
    RuncCommandError { source: io::Error },

    #[snafu(display("Runc command timed out: {}", source))]
    RuncCommandTimeoutError { source: tokio::time::Elapsed },

    #[snafu(display("Unicode conversion error: {}", source))]
    UnicodeConversionError { source: FromUtf8Error },

    #[snafu(display(
        "Runc returned with an error, stdout: \"{}\", stderr: \"{}\"",
        stdout,
        stderr
    ))]
    RuncCommandFailedError { stdout: String, stderr: String },

    #[snafu(display("JSON deserialization error: {}", source))]
    JsonDeserializationError { source: serde_json::error::Error },

    #[snafu(display("Missing container statistics"))]
    MissingContainerStatsError {},

    #[snafu(display("Top command is missing a pid header"))]
    TopMissingPidHeaderError {},

    #[snafu(display("Top command returned an empty response"))]
    TopShortResponseError {},

    #[snafu(display("Failed to find valid path for spec file"))]
    SpecFilePathError {},

    #[snafu(display("Failed to create spec file: {}", source))]
    SpecFileCreationError { source: io::Error },

    #[snafu(display("Failed to cleanup spec file: {}", source))]
    SpecFileCleanupError { source: io::Error },

    #[snafu(display("Unable to parse runc version"))]
    RuncInvalidVersionError {},

    #[snafu(display("Unable to parse runc spec version"))]
    RuncInvalidSpecVersionError {},

    #[snafu(display("Unable to bind to unix socket: {}", source))]
    UnixSocketOpenError { source: io::Error },

    #[snafu(display("Unix socket unexpectedly closed"))]
    UnixSocketUnexpectedCloseError {},

    #[snafu(display("Unix socket connection error: {}", source))]
    UnixSocketConnectError { source: io::Error },

    #[snafu(display("Unix socket connection error"))]
    UnixSocketReceiveMessageError {},

    #[snafu(display("Unable to extract test files: {}", source))]
    BundleExtractError { source: io::Error },
}

/// Runc container
#[derive(Debug, Serialize, Deserialize)]
pub struct Container {
    /// Container id
    pub id: Option<String>,
    /// Process id
    pub pid: Option<usize>,
    /// Current status
    pub status: Option<String>,
    /// OCI bundle path
    pub bundle: Option<String>,
    /// Root filesystem path
    pub rootfs: Option<String>,
    /// Creation time
    pub created: Option<DateTime<Utc>>,
    /// Annotations
    pub annotations: Option<HashMap<String, String>>,
}

/// Runc version information
#[derive(Debug, Clone)]
pub struct Version {
    /// Runc version
    pub runc_version: Option<String>,
    /// OCI specification version
    pub spec_version: Option<String>,
    /// Commit hash (non-release builds)
    pub commit: Option<String>,
}

/// Runc logging format
#[derive(Debug, Clone)]
pub enum RuncLogFormat {
    Json,
    Text,
}

/// Runc client configuration
#[derive(Debug, Clone, Default)]
pub struct RuncConfiguration {
    /// Path to a runc binary (optional)
    pub command: Option<PathBuf>,
    /// Runc command timeouts
    pub timeout: Option<Duration>,
    /// Path to runc root directory
    pub root: Option<PathBuf>,
    /// Enable runc debug logging
    pub debug: bool,
    /// Path to write runc logs
    pub log: Option<PathBuf>,
    /// Write runc logs in text or json format
    pub log_format: Option<RuncLogFormat>,
    /// Use systemd cgroups
    pub systemd_cgroup: bool,
    /// Run in rootless mode
    pub rootless: Option<bool>,
}

/// Runc client
pub struct Runc {
    command: PathBuf,
    timeout: Duration,
    root: Option<PathBuf>,
    debug: bool,
    log: Option<PathBuf>,
    log_format: Option<RuncLogFormat>,
    systemd_cgroup: bool,
    rootless: Option<bool>,
}

trait Args {
    fn args(&self) -> Result<Vec<String>, Error>;
}

impl Runc {
    /// Create a new runc client from the supplied configuration
    pub fn new(config: RuncConfiguration) -> Result<Self, Error> {
        let command = config
            .command
            .or_else(Self::runc_binary)
            .context(RuncNotFoundError {})?;
        let timeout = config
            .timeout
            .or(Some(Duration::from_millis(5000)))
            .unwrap();
        Ok(Self {
            command,
            timeout,
            root: config.root,
            debug: config.debug,
            log: config.log,
            log_format: config.log_format,
            systemd_cgroup: config.systemd_cgroup,
            rootless: config.rootless,
        })
    }

    /// Create a new container
    pub async fn create(
        &self,
        id: &str,
        bundle: &PathBuf,
        opts: Option<&CreateOpts>,
    ) -> Result<(), Error> {
        let mut args = vec![String::from("create")];
        Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args))?;
        let bundle: String = bundle
            .canonicalize()
            .context(InvalidPathError {})?
            .to_string_lossy()
            .parse()
            .unwrap();
        args.push(String::from("--bundle"));
        args.push(bundle);
        args.push(String::from(id));
        self.command(&args, true).await.map(|_| ())
    }

    /// Delete a container
    pub async fn delete(&self, id: &str, opts: Option<&DeleteOpts>) -> Result<(), Error> {
        let mut args = vec![String::from("delete")];
        Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args))?;
        args.push(String::from(id));
        self.command(&args, true).await.map(|_| ())
    }

    /// Return an event stream of container notifications
    pub async fn events(&self, id: &str, interval: &Duration) -> Result<EventStream, Error> {
        let args = vec![
            String::from("events"),
            format!("--interval={}s", interval.as_secs()),
            String::from(id),
        ];
        let console_stream = self.command_with_streaming_output(&args, false).await?;
        Ok(EventStream::new(console_stream))
    }

    /// Execute an additional process inside the container
    pub async fn exec(
        &self,
        id: &str,
        spec: &Process,
        opts: Option<&ExecOpts>,
    ) -> Result<(), Error> {
        let temp_file = env::var_os("XDG_RUNTIME_DIR")
            .and_then(
                |temp_dir| match temp_dir.to_string_lossy().parse() as Result<String, _> {
                    Ok(temp_dir) => Some(PathBuf::from(format!(
                        "{}/runc-process-{}",
                        temp_dir,
                        Uuid::new_v4()
                    ))),
                    Err(_) => None,
                },
            )
            .context(SpecFilePathError {})?;

        {
            let spec_json = serde_json::to_string(spec).context(JsonDeserializationError {})?;
            let mut f = File::create(temp_file.clone()).context(SpecFileCreationError {})?;
            f.write(spec_json.as_bytes())
                .context(SpecFileCreationError {})?;
            f.flush().context(SpecFileCreationError {})?;
        }

        let temp_file: String = temp_file.to_string_lossy().parse().unwrap();
        let mut args = vec![
            String::from("exec"),
            String::from("--process"),
            temp_file.clone(),
        ];
        Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args))?;
        args.push(String::from(id));

        let res = self.command(&args, true).await.map(|_| ());
        fs::remove_file(temp_file).context(SpecFileCleanupError {})?;
        res
    }

    /// Send the specified signal to processes inside the container
    pub async fn kill(&self, id: &str, sig: i32, opts: Option<&KillOpts>) -> Result<(), Error> {
        let mut args = vec![String::from("kill")];
        Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args))?;
        args.push(String::from(id));
        args.push(format!("{}", sig));
        self.command(&args, true).await.map(|_| ())
    }

    /// List all containers associated with this runc instance
    pub async fn list(&self) -> Result<Vec<Container>, Error> {
        let args = vec![String::from("list"), String::from("--format=json")];
        let output = self.command(&args, false).await?;
        let output = output.trim();
        // Ugly hack to work around golang
        Ok(if output == "null" {
            Vec::new()
        } else {
            serde_json::from_str(&output).context(JsonDeserializationError {})?
        })
    }

    /// Pause a container
    pub async fn pause(&self, id: &str) -> Result<(), Error> {
        let args = vec![String::from("pause"), String::from(id)];
        self.command(&args, true).await.map(|_| ())
    }

    /// List processes inside a container, returning their pids
    pub async fn ps(&self, id: &str) -> Result<Vec<usize>, Error> {
        let args = vec![
            String::from("ps"),
            String::from("--format=json"),
            String::from(id),
        ];
        let output = self.command(&args, false).await?;
        let output = output.trim();
        // Ugly hack to work around golang
        Ok(if output == "null" {
            Vec::new()
        } else {
            serde_json::from_str(&output).context(JsonDeserializationError {})?
        })
    }

    /// Resume a container
    pub async fn resume(&self, id: &str) -> Result<(), Error> {
        let args = vec![String::from("resume"), String::from(id)];
        self.command(&args, true).await.map(|_| ())
    }

    /// Run the create, start, delete lifecycle of the container and return its exit status
    pub async fn run(
        &self,
        id: &str,
        bundle: &PathBuf,
        opts: Option<&CreateOpts>,
    ) -> Result<(), Error> {
        let mut args = vec![String::from("run")];
        Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args))?;
        let bundle: String = bundle
            .canonicalize()
            .context(InvalidPathError {})?
            .to_string_lossy()
            .parse()
            .unwrap();
        args.push(String::from("--bundle"));
        args.push(bundle);
        args.push(String::from(id));
        self.command(&args, true).await.map(|_| ())
    }

    /// Start an already created container
    pub async fn start(&self, id: &str) -> Result<(), Error> {
        let args = vec![String::from("start"), String::from(id)];
        self.command(&args, true).await.map(|_| ())
    }

    /// Return the state of a container
    pub async fn state(&self, id: &str) -> Result<Container, Error> {
        let args = vec![String::from("state"), String::from(id)];
        let output = self.command(&args, true).await?;
        Ok(serde_json::from_str(&output).context(JsonDeserializationError {})?)
    }

    /// Return the latest statistics for a container
    pub async fn stats(&self, id: &str) -> Result<Stats, Error> {
        let args = vec![
            String::from("events"),
            String::from("--stats"),
            String::from(id),
        ];
        let output = self.command(&args, true).await?;
        let ev: Event = serde_json::from_str(&output).context(JsonDeserializationError {})?;
        ensure!(ev.stats.is_some(), MissingContainerStatsError {});
        Ok(ev.stats.unwrap())
    }

    /// List all processes inside the container, returning the full ps data
    pub async fn top(&self, id: &str, ps_options: Option<&str>) -> Result<TopResults, Error> {
        let mut args = vec![
            String::from("ps"),
            String::from("--format"),
            String::from("table"),
            String::from(id),
        ];
        if let Some(ps_options) = ps_options {
            args.push(String::from(ps_options));
        }
        let output = self.command(&args, false).await?;
        let lines: Vec<&str> = output.split('\n').collect();
        ensure!(!lines.is_empty(), TopShortResponseError {});

        let headers: Vec<String> = lines[0].split_whitespace().map(String::from).collect();
        let pid_index = headers.iter().position(|x| x == "PID");
        ensure!(pid_index.is_some(), TopMissingPidHeaderError {});

        let mut processes: TopResults = Vec::new();

        for line in lines.iter().skip(1) {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields[pid_index.unwrap()] == "-" {
                continue;
            }

            let mut process: Vec<&str> = Vec::from(&fields[..headers.len() - 1]);
            let process_field = &fields[headers.len() - 1..].join(" ");
            process.push(process_field);

            let mut process_map: HashMap<String, String> = HashMap::new();
            for j in 0..headers.len() {
                if let Some(key) = headers.get(j) {
                    if let Some(&value) = process.get(j) {
                        process_map.insert(key.clone(), String::from(value));
                    }
                }
            }
            processes.push(process_map);
        }
        Ok(processes)
    }

    /// Update a container with the provided resource spec
    pub async fn update(&self, id: &str, resources: &LinuxResources) -> Result<(), Error> {
        let temp_file = env::var_os("XDG_RUNTIME_DIR")
            .and_then(
                |temp_dir| match temp_dir.to_string_lossy().parse() as Result<String, _> {
                    Ok(temp_dir) => Some(PathBuf::from(format!(
                        "{}/runc-process-{}",
                        temp_dir,
                        Uuid::new_v4()
                    ))),
                    Err(_) => None,
                },
            )
            .context(SpecFilePathError {})?;

        {
            let spec_json =
                serde_json::to_string(resources).context(JsonDeserializationError {})?;
            let mut f = File::create(temp_file.clone()).context(SpecFileCreationError {})?;
            f.write(spec_json.as_bytes())
                .context(SpecFileCreationError {})?;
            f.flush().context(SpecFileCreationError {})?;
        }

        let temp_file: String = temp_file.to_string_lossy().parse().unwrap();
        let args = vec![
            String::from("update"),
            String::from("--resources"),
            temp_file.clone(),
            String::from(id),
        ];
        let res = self.command(&args, true).await.map(|_| ());
        fs::remove_file(temp_file).context(SpecFileCleanupError {})?;
        res
    }

    /// Return the version of runc
    pub async fn version(&self) -> Result<Version, Error> {
        let output = self.command(&[String::from("--version")], false).await?;
        let mut version = Version {
            runc_version: None,
            spec_version: None,
            commit: None,
        };
        for line in output.split('\n').take(3).map(|line| line.trim()) {
            if line.contains("version") {
                version.runc_version = Some(
                    line.split("version ")
                        .nth(1)
                        .map(String::from)
                        .context(RuncInvalidVersionError {})?,
                );
            } else if line.contains("spec") {
                version.spec_version = Some(
                    line.split(": ")
                        .nth(1)
                        .map(String::from)
                        .context(RuncInvalidSpecVersionError {})?,
                );
            } else if line.contains("commit") {
                version.commit = line.split(": ").nth(1).map(String::from);
            }
        }
        Ok(version)
    }

    async fn command(&self, args: &[String], combined_output: bool) -> Result<String, Error> {
        let args = self.concat_args(args)?;
        let process = Command::new(&self.command)
            .args(&args.clone())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(ProcessSpawnError {})?;

        let result = timeout(self.timeout, process.wait_with_output())
            .await
            .context(RuncCommandTimeoutError {})?
            .context(RuncCommandError {})?;

        let stdout = String::from_utf8(result.stdout.clone()).context(UnicodeConversionError {})?;
        let stderr = String::from_utf8(result.stderr.clone()).context(UnicodeConversionError {})?;
        ensure!(
            result.status.success(),
            RuncCommandFailedError {
                stdout: stdout,
                stderr: stderr
            }
        );

        Ok(if combined_output {
            let mut combined = String::new();
            combined.push_str(&stdout);
            combined.push_str(&stderr);
            combined
        } else {
            stdout
        })
    }

    async fn command_with_streaming_output(
        &self,
        args: &[String],
        combined_output: bool,
    ) -> Result<ConsoleStream, Error> {
        let args = self.concat_args(args)?;
        let process = Command::new(&self.command)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context(ProcessSpawnError {})?;
        ConsoleStream::new(process, combined_output)
    }

    fn concat_args(&self, args: &[String]) -> Result<Vec<String>, Error> {
        let mut combined = self.args()?;
        combined.append(&mut Vec::from_iter(args.iter().cloned().map(String::from)));
        Ok(combined)
    }

    fn append_opts(args: &mut Vec<String>, opts: Option<&dyn Args>) -> Result<(), Error> {
        if let Some(opts) = opts {
            args.append(&mut opts.args()?);
        }
        Ok(())
    }

    fn runc_binary() -> Option<PathBuf> {
        env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .filter_map(|dir| {
                    let full_path = dir.join("runc");
                    if full_path.is_file() {
                        Some(full_path)
                    } else {
                        None
                    }
                })
                .next()
        })
    }
}

impl Args for Runc {
    fn args(&self) -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = Vec::new();
        if let Some(root) = self.root.clone() {
            args.push(String::from("--root"));
            args.push(
                root.canonicalize()
                    .context(InvalidPathError {})?
                    .to_string_lossy()
                    .parse()
                    .unwrap(),
            );
        }
        if self.debug {
            args.push(String::from("--debug"));
        }
        if let Some(log) = self.log.clone() {
            args.push(String::from("--log"));
            args.push(log.to_string_lossy().parse().unwrap());
        }
        if let Some(log_format) = self.log_format.clone() {
            args.push(String::from("--log-format"));
            args.push(String::from(match log_format {
                RuncLogFormat::Json => "json",
                RuncLogFormat::Text => "text",
            }))
        }
        if self.systemd_cgroup {
            args.push(String::from("--systemd-cgroup"));
        }
        if let Some(rootless) = self.rootless {
            args.push(format!("--rootless={}", rootless));
        }
        Ok(args)
    }
}

// Clean up after tests
#[cfg(test)]
impl Drop for Runc {
    fn drop(&mut self) {
        if let Some(root) = self.root.clone() {
            if let Err(e) = fs::remove_dir_all(&root) {
                warn!("failed to cleanup root directory: {}", e);
            }
        }
        if let Some(system_runc) = Self::runc_binary() {
            if system_runc != self.command {
                if let Err(e) = fs::remove_file(&self.command) {
                    warn!("failed to remove runc binary: {}", e);
                }
            }
        } else if let Err(e) = fs::remove_file(&self.command) {
            warn!("failed to remove runc binary: {}", e);
        }
    }
}

/// Container creation options
#[derive(Debug, Clone)]
pub struct CreateOpts {
    /// Path to where a pid file should be created
    pub pid_file: Option<PathBuf>,
    /// Path to a socket which will receive the console file descriptor
    pub console_socket: Option<PathBuf>,
    /// Do not use pivot root to jail process inside rootfs
    pub no_pivot: bool,
    /// Do not create a new session keyring for the container
    pub no_new_keyring: bool,
    /// Detach from the container's process (only available for run)
    pub detach: bool,
}

impl Args for CreateOpts {
    fn args(&self) -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = Vec::new();
        if let Some(pid_file) = self.pid_file.clone() {
            args.push(String::from("--pid-file"));
            args.push(pid_file.to_string_lossy().parse().unwrap())
        }
        if let Some(console_socket) = self.console_socket.clone() {
            args.push(String::from("--console-socket"));
            args.push(
                console_socket
                    .canonicalize()
                    .context(InvalidPathError {})?
                    .to_string_lossy()
                    .parse()
                    .unwrap(),
            )
        }
        if self.no_pivot {
            args.push(String::from("--no-pivot"));
        }
        if self.no_new_keyring {
            args.push(String::from("--no-new-keyring"));
        }
        if self.detach {
            args.push(String::from("--detach"));
        }
        Ok(args)
    }
}

/// Container deletion options
#[derive(Debug, Clone)]
pub struct DeleteOpts {
    /// Forcibly delete the container if it is still running
    pub force: bool,
}

impl Args for DeleteOpts {
    fn args(&self) -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = Vec::new();
        if self.force {
            args.push(String::from("--force"));
        }
        Ok(args)
    }
}

/// Process execution options
#[derive(Debug, Clone)]
pub struct ExecOpts {
    /// Path to where a pid file should be created
    pub pid_file: Option<PathBuf>,
    /// Path to a socket which will receive the console file descriptor
    pub console_socket: Option<PathBuf>,
    /// Detach from the container's process
    pub detach: bool,
}

impl Args for ExecOpts {
    fn args(&self) -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = Vec::new();
        if let Some(console_socket) = self.console_socket.clone() {
            args.push(String::from("--console-socket"));
            args.push(
                console_socket
                    .canonicalize()
                    .context(InvalidPathError {})?
                    .to_string_lossy()
                    .parse()
                    .unwrap(),
            );
        }
        if self.detach {
            args.push(String::from("--detach"));
        }
        if let Some(pid_file) = self.pid_file.clone() {
            args.push(String::from("--pid-file"));
            args.push(pid_file.to_string_lossy().parse().unwrap());
        }
        Ok(args)
    }
}

/// Container killing options
#[derive(Debug, Clone)]
pub struct KillOpts {
    /// Send the signal to all processes inside the container
    pub all: bool,
}

impl Args for KillOpts {
    fn args(&self) -> Result<Vec<String>, Error> {
        let mut args: Vec<String> = Vec::new();
        if self.all {
            args.push(String::from("--all"))
        }
        Ok(args)
    }
}

/// Stream of container events
pub struct EventStream {
    inner: ConsoleStream,
}

impl EventStream {
    fn new(inner: ConsoleStream) -> Self {
        Self { inner }
    }
}

impl Stream for EventStream {
    type Item = Result<Event, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(Ok(line)) = ready!(Pin::new(&mut self.inner).poll_next(cx)) {
            Poll::Ready(Some(
                serde_json::from_str(&line).context(JsonDeserializationError {}),
            ))
        } else {
            Poll::Ready(None)
        }
    }
}

struct ConsoleStream {
    process: Child,
    inner: Pin<Box<dyn Stream<Item = tokio::io::Result<String>>>>,
}

impl ConsoleStream {
    fn new(mut process: Child, combined_output: bool) -> Result<Self, Error> {
        let stdout = BufReader::new(process.stdout.take().unwrap()).lines();
        let inner: Pin<Box<dyn Stream<Item = tokio::io::Result<String>>>> = if combined_output {
            let stderr = BufReader::new(process.stderr.take().unwrap()).lines();
            Box::pin(stdout.merge(stderr))
        } else {
            Box::pin(stdout)
        };
        Ok(Self { process, inner })
    }
}

impl Stream for ConsoleStream {
    type Item = tokio::io::Result<String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(line) = ready!(self.inner.as_mut().poll_next(cx)) {
            Poll::Ready(Some(line))
        } else {
            Poll::Ready(None)
        }
    }
}

/*impl Stream for ConsoleStream {
    type Item = String;
    type Error = Error;

    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        loop {
            let mut not_ready = 0;
            let mut next_character = [0u8; 1];

            match self.stdout.poll_read(&mut next_character) {
                Ok(Async::Ready(0)) => return Ok(Async::Ready(None)),
                Ok(Async::Ready(_)) => self.stdout_buf.push(next_character[0]),
                Ok(Async::NotReady) => not_ready += 1,
                Err(e) => return Err(e.into()),
            };

            if let Some(last_character) = self.stdout_buf.last() {
                if *last_character == b'\n' {
                    let line = String::from_utf8(self.stdout_buf.clone())?;
                    self.stdout_buf.drain(..);
                    return Ok(Async::Ready(Some(line)));
                }
            }

            if self.combined_output {
                match self.stderr.poll_read(&mut next_character) {
                    Ok(Async::Ready(0)) => return Ok(Async::Ready(None)),
                    Ok(Async::Ready(_)) => self.stderr_buf.push(next_character[0]),
                    Ok(Async::NotReady) => not_ready += 1,
                    Err(e) => return Err(e.into()),
                };

                if let Some(last_character) = self.stderr_buf.last() {
                    if *last_character == b'\n' {
                        let line = String::from_utf8(self.stderr_buf.clone())?;
                        self.stderr_buf.drain(..);
                        return Ok(Async::Ready(Some(line)));
                    }
                }
            }

            if (self.combined_output && not_ready == 2) || (!self.combined_output && not_ready == 1)
            {
                return Ok(Async::NotReady);
            }
        }
    }
}*/

impl Drop for ConsoleStream {
    fn drop(&mut self) {
        if let Err(e) = self.process.kill() {
            warn!("failed to kill container: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::ReceivePtyMaster;
    use crate::specs::{LinuxCapabilities, LinuxMemory, POSIXRlimit, User};
    use flate2::read::GzDecoder;
    use futures::executor::block_on;
    use futures::StreamExt;
    use log::error;
    use tar::Archive;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::runtime::Runtime;
    use tokio::time::delay_for;

    #[test]
    fn test_create() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            // As an ugly hack leak the pty master handle for the lifecycle of the test process
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)?;
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            runc.create(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: false,
                }),
            )
            .await?;

            runc.state(&id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let container = runtime.block_on(task).expect("test failed");

        assert_eq!(container.status, Some(String::from("created")));
    }

    #[test]
    fn test_delete() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;

            runc.kill(&container.id, libc::SIGKILL, None).await?;
            delay_for(Duration::from_millis(500)).await;
            runc.delete(&container.id, None).await?;
            runc.list().await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let containers = runtime.block_on(task).expect("test failed");

        assert!(containers.is_empty());
    }

    #[test]
    fn test_events() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;

            let events = runc.events(&container.id, &Duration::from_secs(1)).await?;
            Ok::<_, Error>(
                events
                    .take(3)
                    .map(|event| event.unwrap())
                    .collect::<Vec<Event>>()
                    .await,
            )
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let events = runtime.block_on(task).expect("test failed");

        assert_eq!(events.len(), 3);

        // Validate all the events contain valid payloads
        for event in events.iter() {
            if let Some(stats) = event.stats.clone() {
                if let Some(memory) = stats.memory.clone() {
                    if let Some(usage) = memory.usage {
                        if let Some(usage) = usage.usage {
                            if usage > 0 {
                                continue;
                            }
                        }
                    }
                }
            }
            panic!("event is missing memory usage statistics");
        }
    }

    #[test]
    fn test_exec() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            // As an ugly hack leak the pty master handle for the lifecycle of the test process
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)?;
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            // As an ugly hack leak the pty master handle for the lifecycle of the test process
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let additional_console_socket = env::temp_dir().join(&id).with_extension("console2");
            let receive_additional_pty_master = ReceivePtyMaster::new(&additional_console_socket)?;
            tokio::spawn(async move {
                match receive_additional_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive additional PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            let capabilities = Some(vec![
                String::from("CAP_AUDIT_WRITE"),
                String::from("CAP_KILL"),
                String::from("CAP_NET_BIND_SERVICE"),
            ]);

            runc.create(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: false,
                }),
            )
            .await?;

            runc.exec(
                &id,
                &Process {
                    terminal: Some(true),
                    console_size: None,
                    user: Some(User {
                        uid: Some(0),
                        gid: Some(0),
                        additional_gids: None,
                        username: None,
                    }),
                    args: Some(vec![String::from("sleep"), String::from("10")]),
                    command_line: None,
                    env: Some(vec![
                        String::from(
                            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                        ),
                        String::from("TERM=xterm"),
                    ]),
                    cwd: Some(String::from("/")),
                    capabilities: Some(LinuxCapabilities {
                        bounding: capabilities.clone(),
                        effective: capabilities.clone(),
                        inheritable: capabilities.clone(),
                        permitted: capabilities.clone(),
                        ambient: capabilities.clone(),
                    }),
                    rlimits: Some(vec![POSIXRlimit {
                        limit_type: Some(String::from("RLIMIT_NOFILE")),
                        hard: Some(1024),
                        soft: Some(1024),
                    }]),
                    no_new_privileges: Some(false),
                    app_armor_profile: None,
                    oom_score_adj: None,
                    selinux_label: None,
                },
                Some(&ExecOpts {
                    pid_file: Some(PathBuf::from("/tmp/bang.pid")),
                    console_socket: Some(additional_console_socket),
                    detach: true,
                }),
            )
            .await?;

            delay_for(Duration::from_millis(500)).await;
            let processes = runc.top(&id, None).await?;
            runc.kill(&id, libc::SIGKILL, None).await?;
            Ok::<_, Error>(processes)
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let processes = runtime.block_on(task).expect("test failed");

        assert_ne!(
            processes
                .iter()
                .find(|process| if let Some(cmd) = process.get("CMD") {
                    cmd == "sleep 10"
                } else {
                    false
                }),
            None
        );
    }

    #[test]
    fn test_kill() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;

            runc.kill(&container.id, libc::SIGKILL, None).await?;
            delay_for(Duration::from_millis(500)).await;
            runc.state(&container.id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let state = runtime.block_on(task).expect("test failed");

        assert_eq!(state.status, Some(String::from("stopped")));
    }

    #[test]
    fn test_list() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await
            .unwrap();

            let containers = runc.list().await.unwrap();
            if containers.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected a single container",
                ));
            }
            if let Some(container_item) = containers.get(0) {
                if let Some(id) = container_item.id.clone() {
                    if id == container.id {
                        return Ok(runc);
                    }
                }
            }
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected container to match",
            ))
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on(task).expect("test failed");
    }

    #[test]
    fn test_pause() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;

            runc.pause(&container.id).await?;
            let container_state = runc.state(&container.id).await?;
            // Can't seem to kill/delete a paused container
            runc.resume(&container.id).await?;
            Ok::<_, Error>(container_state)
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let container_state = runtime.block_on(task).expect("test failed");

        assert_eq!(container_state.status, Some(String::from("paused")));
    }

    #[test]
    fn test_ps() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await
            .unwrap();

            // Time for shell to spawn
            delay_for(Duration::from_millis(100)).await;

            let res = runc.ps(&container.id).await;
            if let Err(err) = res {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("failed to run ps command: {}", err),
                ));
            }

            let processes = res.unwrap();
            if processes.len() != 1 {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected a single shell process",
                ))
            } else if let Some(pid) = processes.get(0) {
                if *pid > 0 && *pid < 32768 {
                    Ok::<_, io::Error>(runc)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid pid number",
                    ))
                }
            } else {
                Err(io::Error::new(io::ErrorKind::Other, ""))
            }
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on(task).expect("test failed");
    }

    #[test]
    fn test_resume() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;

            runc.pause(&container.id).await?;

            let container_state = runc.state(&container.id).await?;
            let status = container_state.status.unwrap();
            assert_eq!(status, "paused");

            runc.resume(&container.id).await?;
            runc.state(&container.id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let container = runtime.block_on(task).expect("test failed");

        assert_eq!(container.status, Some(String::from("running")));
    }

    #[test]
    fn test_run() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            // As an ugly hack leak the pty master handle for the lifecycle of the test process
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)?;
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            runc.run(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: true,
                }),
            )
            .await?;

            delay_for(Duration::from_millis(500)).await;

            runc.state(&id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let container = runtime.block_on(task).expect("test failed");

        assert_eq!(container.status, Some(String::from("running")));
    }

    #[test]
    fn test_start() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            // As an ugly hack leak the pty master handle for the lifecycle of the test process
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)?;
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            runc.create(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: false,
                }),
            )
            .await?;

            runc.start(&id).await?;

            delay_for(Duration::from_millis(500)).await;

            let container_state = runc.state(&id).await?;
            runc.kill(&id, libc::SIGKILL, None).await?;
            Ok::<_, Error>(container_state)
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let container = runtime.block_on(task).expect("test failed");

        assert_eq!(container.status, Some(String::from("running")));
    }

    #[test]
    fn test_state() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await?;
            runc.state(&container.id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let state = runtime.block_on(task).expect("test failed");

        assert_eq!(state.status, Some(String::from("running")));
    }

    #[test]
    fn test_stats() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await
            .unwrap();

            let stats = runc
                .stats(&container.id)
                .await
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))?;
            if let Some(memory) = stats.memory {
                if let Some(usage) = memory.usage {
                    if let Some(usage) = usage.usage {
                        if usage > 0 {
                            return Ok::<_, io::Error>(());
                        }
                    }
                }
            }
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing memory usage statistics",
            ))
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on(task).expect("test failed");
    }

    #[test]
    fn test_top() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path.clone());
        config.root = Some(runc_root.clone());
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let container = ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .await
            .unwrap();

            // Time for shell to spawn
            delay_for(Duration::from_millis(100)).await;

            let processes = runc
                .top(&container.id, None)
                .await
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))?;

            if processes.len() != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected a single shell process",
                ));
            }
            if let Some(process) = processes.get(0) {
                if process["CMD"] != "sh" {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "expected shell"));
                }
            }
            Ok::<_, io::Error>(())
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on(task).expect("test failed");
    }

    #[test]
    fn test_update() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            // As an ugly hack leak the pty master handle for the lifecycle of the test
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)
                .expect("Unable to open pty receiving socket");
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            runc.run(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: true,
                }),
            )
            .await?;

            runc.update(
                &id,
                &LinuxResources {
                    devices: None,
                    memory: Some(LinuxMemory {
                        limit: Some(232_000_000),
                        reservation: None,
                        swap: None,
                        kernel: None,
                        kernel_tcp: None,
                        swappiness: None,
                        disable_oom_killer: None,
                    }),
                    cpu: None,
                    pids: None,
                    block_io: None,
                    hugepage_limits: None,
                    network: None,
                    rdma: None,
                },
            )
            .await?;

            runc.stats(&id).await
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let stats = runtime.block_on(task).expect("test failed");

        if let Some(memory) = stats.memory {
            if let Some(usage) = memory.usage {
                if let Some(limit) = usage.limit {
                    if limit < 233_000_000 && limit > 231_000_000 {
                        // Within the range of our set limit
                        return;
                    }
                }
            }
        }

        panic!("updating memory limit failed");
    }

    #[test]
    fn test_version() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let version = runtime.block_on(runc.version()).expect("test failed");

        assert_eq!(version.runc_version, Some(String::from("1.0.0-rc10")));
        assert_eq!(version.spec_version, Some(String::from("1.0.1-dev")));
    }

    #[test]
    fn test_receive_pty_master() {
        let runc_id = format!("{}", Uuid::new_v4());
        let runc_path = env::temp_dir().join(&runc_id).join("runc.amd64");
        let runc_root =
            PathBuf::from(env::var_os("XDG_RUNTIME_DIR").expect("expected temporary path"))
                .join("rust-runc")
                .join(&runc_id);
        fs::create_dir_all(&runc_root).expect("unable to create runc root");
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc10.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.root = Some(runc_root);
        let runc = Runc::new(config).expect("Unable to create runc instance");

        let task = async move {
            let id = format!("{}", Uuid::new_v4());

            let (fd_sender, fd_receiver) = futures::channel::oneshot::channel::<tokio::fs::File>();
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)?;
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        fd_sender.send(pty_master).unwrap();
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            let bundle = env::temp_dir().join(&id);
            extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                .context(BundleExtractError {})?;

            runc.run(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: true,
                }),
            )
            .await?;

            Ok::<_, Error>(fd_receiver.await.unwrap())
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let mut pty_master = runtime.block_on(task).expect("test failed");

        let task = async move {
            let mut response = [0u8; 160];
            pty_master.read(&mut response).await?;
            pty_master.write(b"uname -a && exit\n").await?;

            delay_for(Duration::from_millis(500)).await;

            let len = pty_master.read(&mut response).await?;
            Ok::<_, io::Error>(String::from_utf8(Vec::from(&response[..len])).unwrap())
        };

        let mut runtime = Runtime::new().expect("unable to create runtime");
        let response = runtime.block_on(task).expect("test failed");

        let response = match response
            .split('\n')
            .find(|line| line.contains("Linux runc"))
        {
            Some(response) => response,
            None => panic!("did not find response to command"),
        };

        assert!(response.starts_with("Linux runc"));
    }

    /// Extract an OCI bundle tarball to a directory
    fn extract_tarball(tarball: &PathBuf, dst: &PathBuf) -> io::Result<()> {
        let tarball = File::open(tarball)?;
        let tar = GzDecoder::new(tarball);
        let mut archive = Archive::new(tar);
        archive.unpack(dst)?;
        Ok(())
    }

    /// A managed lifecycle (create/delete), runc container
    struct ManagedContainer {
        id: String,
        runc: Option<Runc>,
    }

    impl ManagedContainer {
        async fn new(
            runc_path: &PathBuf,
            runc_root: &PathBuf,
            compressed_bundle: &PathBuf,
        ) -> Result<Self, Error> {
            let id = format!("{}", Uuid::new_v4());
            let bundle = env::temp_dir().join(&id);
            extract_tarball(compressed_bundle, &bundle).expect("Unable to extract bundle");

            let mut config: RuncConfiguration = Default::default();
            config.command = Some(runc_path.clone());
            config.root = Some(runc_root.clone());
            let runc = Runc::new(config)?;

            // As an ugly hack leak the pty master handle for the lifecycle of the test
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            let console_socket = env::temp_dir().join(&id).with_extension("console");
            let receive_pty_master = ReceivePtyMaster::new(&console_socket)
                .expect("Unable to open pty receiving socket");
            tokio::spawn(async move {
                match receive_pty_master.receive().await {
                    Ok(pty_master) => {
                        Box::leak(Box::new(pty_master));
                    }
                    Err(err) => {
                        error!("Receive PTY master error: {}", err);
                    }
                }
            });

            runc.create(
                &id,
                &bundle,
                Some(&CreateOpts {
                    pid_file: None,
                    console_socket: Some(console_socket),
                    no_pivot: false,
                    no_new_keyring: false,
                    detach: false,
                }),
            )
            .await?;
            runc.start(&id).await?;
            Ok(Self {
                id,
                runc: Some(runc),
            })
        }
    }

    impl Drop for ManagedContainer {
        fn drop(&mut self) {
            if let Some(runc) = self.runc.take() {
                let bundle = env::temp_dir().join(&self.id);
                block_on(async move {
                    runc.delete(&self.id, Some(&DeleteOpts { force: true }))
                        .await
                        .expect("Unable to delete container");
                    fs::remove_dir_all(&bundle).expect("Unable to delete bundle");
                });
            }
        }
    }
}
