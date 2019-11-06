/*
 * Copyright 2019 fsyncd, Berlin, Germany.
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

//! A crate for consuming the runc binary in your Rust applications.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Write};
use std::iter::FromIterator;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{env, fs, io};

use chrono::{DateTime, Utc};
use futures::future::{err, ok};
use futures::{try_ready, Async, Future, Stream};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncRead;
use tokio::prelude::FutureExt;
use tokio_process::{Child, ChildStderr, ChildStdout, CommandExt};
use uuid::Uuid;

use crate::events::{Event, Stats};
use crate::specs::{LinuxResources, Process};

/// Container PTY terminal
pub mod console;
/// Container events
pub mod events;
/// OCI runtime specification
pub mod specs;

/// Results of top command
pub type TopResults = Vec<HashMap<String, String>>;

/// Runc client error
#[derive(Debug)]
pub enum Error {
    Unknown,
    CommandTimeout,
    UnicodeError,
    IoError(io::Error),
    JsonError(serde_json::error::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::IoError(e)
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(_: std::convert::Infallible) -> Self {
        Error::UnicodeError
    }
}

impl From<std::string::FromUtf8Error> for Error {
    fn from(_: std::string::FromUtf8Error) -> Self {
        Error::UnicodeError
    }
}

impl From<serde_json::error::Error> for Error {
    fn from(e: serde_json::error::Error) -> Self {
        Error::JsonError(e)
    }
}

impl From<tokio::timer::timeout::Error<Error>> for Error {
    fn from(e: tokio::timer::timeout::Error<Error>) -> Self {
        if e.is_inner() {
            match e.into_inner() {
                Some(inner_error) => inner_error,
                None => Error::Unknown,
            }
        } else {
            Error::CommandTimeout
        }
    }
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
    /// Delete the runc binary and root directory after use (TEST USE ONLY)
    pub should_cleanup: bool,
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
    should_cleanup: bool,
}

trait Args {
    fn args(&self) -> Result<Vec<String>, Error>;
}

impl Runc {
    /// Create a new runc client from the supplied configuration
    pub fn new(config: RuncConfiguration) -> Result<Self, Error> {
        let command = config.command.or_else(Self::runc_binary).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unable to locate runc binary")
        })?;
        let timeout = match config.timeout.or(Some(Duration::from_millis(5000))) {
            Some(timeout) => timeout,
            None => return Err(Error::Unknown),
        };
        Ok(Self {
            command,
            timeout,
            root: config.root,
            debug: config.debug,
            log: config.log,
            log_format: config.log_format,
            systemd_cgroup: config.systemd_cgroup,
            rootless: config.rootless,
            should_cleanup: config.should_cleanup,
        })
    }

    /// Create a new container
    pub fn create(
        self,
        id: &str,
        bundle: &PathBuf,
        opts: Option<&CreateOpts>,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let mut args = vec![String::from("create")];
        if let Err(e) = Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args)) {
            return Box::new(err(e));
        }
        let bundle: String = match bundle.canonicalize() {
            Ok(path) => match path.to_string_lossy().parse() {
                Ok(path) => path,
                Err(e) => {
                    return Box::new(err(e.into()));
                }
            },
            Err(e) => {
                return Box::new(err(e.into()));
            }
        };
        args.push(String::from("--bundle"));
        args.push(bundle);
        args.push(String::from(id));
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// Delete a container
    pub fn delete(
        self,
        id: &str,
        opts: Option<&DeleteOpts>,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let mut args = vec![String::from("delete")];
        if let Err(e) = Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args)) {
            return Box::new(err(e));
        }
        args.push(String::from(id));
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// Return an event stream of container notifications
    pub fn events(
        self,
        id: &str,
        interval: &Duration,
    ) -> Box<dyn Future<Item = (Self, EventStream), Error = Error> + Send> {
        let args = vec![
            String::from("events"),
            format!("--interval={}s", interval.as_secs()),
            String::from(id),
        ];
        Box::new(
            self.command_with_streaming_output(&args, false)
                .and_then(|(runc, console_stream)| Ok((runc, EventStream::new(console_stream)))),
        )
    }

    /// Execute an additional process inside the container
    pub fn exec(
        self,
        id: &str,
        spec: &Process,
        opts: Option<&ExecOpts>,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let temp_file = env::var_os("XDG_RUNTIME_DIR").and_then(|temp_dir| {
            match temp_dir.to_string_lossy().parse() as Result<String, _> {
                Ok(temp_dir) => Some(PathBuf::from(format!(
                    "{}/runc-process-{}",
                    temp_dir,
                    Uuid::new_v4()
                ))),
                Err(_) => None,
            }
        });

        let temp_file = match temp_file {
            Some(p) => p,
            None => {
                return Box::new(err(Error::IoError(io::Error::new(
                    io::ErrorKind::NotFound,
                    "unable to create temporary spec file",
                ))))
            }
        };

        {
            let spec_json = match serde_json::to_string(spec) {
                Ok(spec_json) => spec_json,
                Err(e) => return Box::new(err(e.into())),
            };

            let mut f = match File::create(temp_file.clone()) {
                Ok(f) => f,
                Err(e) => return Box::new(err(e.into())),
            };

            if let Err(e) = f.write(spec_json.as_bytes()) {
                return Box::new(err(e.into()));
            }
            if let Err(e) = f.flush() {
                return Box::new(err(e.into()));
            }
        }

        let temp_file = match temp_file.to_string_lossy().parse() as Result<String, _> {
            Ok(p) => p,
            Err(_) => return Box::new(err(Error::UnicodeError)),
        };

        let mut args = vec![
            String::from("exec"),
            String::from("--process"),
            temp_file.clone(),
        ];
        if let Err(e) = Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args)) {
            return Box::new(err(e));
        }
        args.push(String::from(id));

        Box::new(
            Box::new(self.command(&args, true).map(|(runc, _)| runc)).then(move |res| {
                if let Err(e) = fs::remove_file(temp_file) {
                    return Err(e.into());
                }
                res
            }),
        )
    }

    /// Send the specified signal to processes inside the container
    pub fn kill(
        self,
        id: &str,
        sig: i32,
        opts: Option<&KillOpts>,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let mut args = vec![String::from("kill")];
        if let Err(e) = Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args)) {
            return Box::new(err(e));
        }
        args.push(String::from(id));
        args.push(format!("{}", sig));
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// List all containers associated with this runc instance
    pub fn list(self) -> Box<dyn Future<Item = (Self, Vec<Container>), Error = Error> + Send> {
        let args = vec![String::from("list"), String::from("--format=json")];
        Box::new(self.command(&args, false).and_then(|(runc, output)| {
            let output = output.trim();
            // Ugly hack to work around golang
            if output == "null" {
                return Ok((runc, Vec::new()));
            }
            Ok((runc, serde_json::from_str(&output)?))
        }))
    }

    /// Pause a container
    pub fn pause(self, id: &str) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let args = vec![String::from("pause"), String::from(id)];
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// List processes inside a container, returning their pids
    pub fn ps(self, id: &str) -> Box<dyn Future<Item = (Self, Vec<usize>), Error = Error> + Send> {
        let args = vec![
            String::from("ps"),
            String::from("--format=json"),
            String::from(id),
        ];
        Box::new(self.command(&args, false).and_then(|(runc, output)| {
            let output = output.trim();
            // Ugly hack to work around golang
            if output == "null" {
                return Ok((runc, Vec::new()));
            }
            Ok((runc, serde_json::from_str(&output)?))
        }))
    }

    /// Resume a container
    pub fn resume(self, id: &str) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let args = vec![String::from("resume"), String::from(id)];
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// Run the create, start, delete lifecycle of the container and return its exit status
    pub fn run(
        self,
        id: &str,
        bundle: &PathBuf,
        opts: Option<&CreateOpts>,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let mut args = vec![String::from("run")];
        if let Err(e) = Self::append_opts(&mut args, opts.map(|opts| opts as &dyn Args)) {
            return Box::new(err(e));
        }
        let bundle: String = match bundle.canonicalize() {
            Ok(path) => match path.to_string_lossy().parse() {
                Ok(path) => path,
                Err(e) => {
                    return Box::new(err(e.into()));
                }
            },
            Err(e) => {
                return Box::new(err(e.into()));
            }
        };
        args.push(String::from("--bundle"));
        args.push(bundle);
        args.push(String::from(id));
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// Start an already created container
    pub fn start(self, id: &str) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let args = vec![String::from("start"), String::from(id)];
        Box::new(self.command(&args, true).map(|(runc, _)| runc))
    }

    /// Return the state of a container
    pub fn state(
        self,
        id: &str,
    ) -> Box<dyn Future<Item = (Self, Container), Error = Error> + Send> {
        let args = vec![String::from("state"), String::from(id)];
        Box::new(
            self.command(&args, true)
                .and_then(|(runc, output)| Ok((runc, serde_json::from_str(&output)?))),
        )
    }

    /// Return the latest statistics for a container
    pub fn stats(self, id: &str) -> Box<dyn Future<Item = (Self, Stats), Error = Error> + Send> {
        let args = vec![
            String::from("events"),
            String::from("--stats"),
            String::from(id),
        ];
        Box::new(self.command(&args, true).and_then(|(runc, output)| {
            let ev: Event = serde_json::from_str(&output)?;
            if let Some(stats) = ev.stats {
                Ok((runc, stats))
            } else {
                Err(Error::IoError(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "missing stats",
                )))
            }
        }))
    }

    /// List all processes inside the container, returning the full ps data
    pub fn top(
        self,
        id: &str,
        ps_options: Option<&str>,
    ) -> Box<dyn Future<Item = (Self, TopResults), Error = Error> + Send> {
        let mut args = vec![
            String::from("ps"),
            String::from("--format"),
            String::from("table"),
            String::from(id),
        ];
        if let Some(ps_options) = ps_options {
            args.push(String::from(ps_options));
        }
        Box::new(self.command(&args, false).and_then(|(runc, output)| {
            let lines: Vec<&str> = output.split('\n').collect();
            if lines.is_empty() {
                return Err(Error::IoError(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected short response from top",
                )));
            }

            let header_line = match lines.first() {
                Some(header_line) => header_line,
                None => {
                    return Err(Error::IoError(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unable to find top header",
                    )))
                }
            };
            let headers: Vec<String> = header_line.split_whitespace().map(String::from).collect();
            if let Some(pid_index) = headers.iter().position(|x| x == "PID") {
                let mut processes: TopResults = Vec::new();

                for line in lines.iter().skip(1) {
                    if line.is_empty() {
                        continue;
                    }
                    let fields: Vec<&str> = line.split_whitespace().collect();
                    if fields[pid_index] == "-" {
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
                Ok((runc, processes))
            } else {
                Err(Error::IoError(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unable to locate pid header",
                )))
            }
        }))
    }

    /// Update a container with the provided resource spec
    pub fn update(
        self,
        id: &str,
        resources: &LinuxResources,
    ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
        let temp_file = env::var_os("XDG_RUNTIME_DIR").and_then(|temp_dir| {
            match temp_dir.to_string_lossy().parse() as Result<String, _> {
                Ok(temp_dir) => Some(PathBuf::from(format!(
                    "{}/runc-process-{}",
                    temp_dir,
                    Uuid::new_v4()
                ))),
                Err(_) => None,
            }
        });

        let temp_file = match temp_file {
            Some(p) => p,
            None => {
                return Box::new(err(Error::IoError(io::Error::new(
                    io::ErrorKind::NotFound,
                    "unable to create temporary spec file",
                ))))
            }
        };

        {
            let spec_json = match serde_json::to_string(resources) {
                Ok(spec_json) => spec_json,
                Err(e) => return Box::new(err(e.into())),
            };

            let mut f = match File::create(temp_file.clone()) {
                Ok(f) => f,
                Err(e) => return Box::new(err(e.into())),
            };

            if let Err(e) = f.write(spec_json.as_bytes()) {
                return Box::new(err(e.into()));
            }
            if let Err(e) = f.flush() {
                return Box::new(err(e.into()));
            }
        }

        let temp_file = match temp_file.to_string_lossy().parse() as Result<String, _> {
            Ok(p) => p,
            Err(_) => return Box::new(err(Error::UnicodeError)),
        };

        let args = vec![
            String::from("update"),
            String::from("--resources"),
            temp_file.clone(),
            String::from(id),
        ];
        Box::new(
            Box::new(self.command(&args, true).map(|(runc, _)| runc)).then(move |res| {
                if let Err(e) = fs::remove_file(temp_file) {
                    return Err(e.into());
                }
                res
            }),
        )
    }

    /// Return the version of runc
    pub fn version(self) -> Box<dyn Future<Item = (Self, Version), Error = Error> + Send> {
        Box::new(
            self.command(&[String::from("--version")], false)
                .and_then(|(runc, output)| {
                    let mut version = Version {
                        runc_version: None,
                        spec_version: None,
                        commit: None,
                    };
                    for line in output.split('\n').take(3).map(|line| line.trim()) {
                        if line.contains("version") {
                            version.runc_version = Some(match line.split("version ").nth(1) {
                                Some(runc) => String::from(runc),
                                None => {
                                    return Err(Error::IoError(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "unable to parse runc version",
                                    )))
                                }
                            });
                        } else if line.contains("spec") {
                            version.spec_version = Some(match line.split(": ").nth(1) {
                                Some(spec) => String::from(spec),
                                None => {
                                    return Err(Error::IoError(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "unable to parse spec version",
                                    )))
                                }
                            });
                        } else if line.contains("commit") {
                            version.commit = Some(match line.split(": ").nth(1) {
                                Some(commit) => String::from(commit),
                                None => {
                                    return Err(Error::IoError(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "unable to parse commit hash",
                                    )))
                                }
                            });
                        }
                    }
                    Ok((runc, version))
                }),
        )
    }

    fn command(
        self,
        args: &[String],
        combined_output: bool,
    ) -> Box<dyn Future<Item = (Self, String), Error = Error> + Send> {
        let args = match self.concat_args(args) {
            Ok(a) => a,
            Err(e) => return Box::new(err(e)),
        };
        let process = Command::new(&self.command)
            .args(&args.clone())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_async();
        let timeout = self.timeout;
        match process {
            Ok(process) => Box::new(
                process
                    .wait_with_output()
                    .from_err::<Error>()
                    .and_then(move |result| {
                        if result.status.success() {
                            Ok((
                                self,
                                String::from_utf8(if combined_output {
                                    let mut combined = result.stdout.clone();
                                    combined.append(&mut result.stderr.clone());
                                    combined
                                } else {
                                    result.stdout
                                })?,
                            ))
                        } else {
                            let stdout = String::from_utf8(result.stdout.clone())?;
                            let stderr = String::from_utf8(result.stderr.clone())?;
                            error!("runc command execution failed, args = {:?}, stdout = '{}', stderr = '{}'", args, stdout, stderr);
                            Err(Error::IoError(io::Error::new(
                                io::ErrorKind::Other,
                                "runc command execution failed",
                            )))
                        }
                    })
                    .timeout(timeout)
                    .from_err::<Error>(),
            ),
            Err(e) => Box::new(err(Error::IoError(e))),
        }
    }

    fn command_with_streaming_output(
        self,
        args: &[String],
        combined_output: bool,
    ) -> Box<dyn Future<Item = (Self, ConsoleStream), Error = Error> + Send> {
        let process = Command::new(&self.command)
            .args(match self.concat_args(args) {
                Ok(a) => a,
                Err(e) => return Box::new(err(e)),
            })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn_async();
        match process {
            Ok(process) => {
                let console_stream = match ConsoleStream::new(process, combined_output) {
                    Ok(console_stream) => console_stream,
                    Err(e) => return Box::new(err(e)),
                };
                Box::new(ok((self, console_stream)))
            }
            Err(e) => Box::new(err(Error::IoError(e))),
        }
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
            args.push(root.canonicalize()?.to_string_lossy().parse()?);
        }
        if self.debug {
            args.push(String::from("--debug"));
        }
        if let Some(log) = self.log.clone() {
            args.push(String::from("--log"));
            args.push(log.to_string_lossy().parse()?);
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
impl Drop for Runc {
    fn drop(&mut self) {
        if self.should_cleanup {
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
            args.push(pid_file.to_string_lossy().parse()?)
        }
        if let Some(console_socket) = self.console_socket.clone() {
            args.push(String::from("--console-socket"));
            args.push(console_socket.canonicalize()?.to_string_lossy().parse()?)
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
            args.push(console_socket.canonicalize()?.to_string_lossy().parse()?);
        }
        if self.detach {
            args.push(String::from("--detach"));
        }
        if let Some(pid_file) = self.pid_file.clone() {
            args.push(String::from("--pid-file"));
            args.push(pid_file.to_string_lossy().parse()?);
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
    type Item = Event;
    type Error = Error;

    fn poll(&mut self) -> Result<Async<Option<Self::Item>>, Self::Error> {
        let line = try_ready!(self.inner.poll());
        if let Some(line) = line {
            let ev: Event = serde_json::from_str(&line)?;
            Ok(Async::Ready(Some(ev)))
        } else {
            Ok(Async::Ready(None))
        }
    }
}

struct ConsoleStream {
    process: Child,
    combined_output: bool,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
}

impl ConsoleStream {
    fn new(mut process: Child, combined_output: bool) -> Result<Self, Error> {
        let stdout = if let Some(stdout) = process.stdout().take() {
            BufReader::new(stdout)
        } else {
            return Err(Error::IoError(io::Error::new(
                io::ErrorKind::NotFound,
                "missing stdout handle",
            )));
        };
        let stderr = if let Some(stderr) = process.stderr().take() {
            BufReader::new(stderr)
        } else {
            return Err(Error::IoError(io::Error::new(
                io::ErrorKind::NotFound,
                "missing stderr handle",
            )));
        };
        Ok(Self {
            process,
            combined_output,
            stdout,
            stderr,
            stdout_buf: vec![],
            stderr_buf: vec![],
        })
    }
}

impl Stream for ConsoleStream {
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
}

impl Drop for ConsoleStream {
    fn drop(&mut self) {
        if let Err(e) = self.process.kill() {
            warn!("failed to kill container: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::ops::Add;
    use std::thread;
    use std::time::Instant;

    use flate2::read::GzDecoder;
    use futures::future::FutureResult;
    use futures::lazy;
    use tar::Archive;
    use tokio::runtime::Runtime;
    use tokio::timer::Delay;

    use crate::console::ReceivePtyMaster;
    use crate::specs::{LinuxCapabilities, LinuxMemory, POSIXRlimit, User};

    use super::*;

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                Box::new(
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
                    .and_then(move |runc| runc.state(&id)),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, container) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<
                    dyn Future<Item = (Runc, String, Vec<Container>), Error = Error> + Send,
                > {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    let delete_id = container.id.clone();
                    Box::new(
                        runc.kill(&container.id, libc::SIGKILL, None)
                            .and_then(move |runc| {
                                thread::sleep(Duration::from_millis(500));
                                runc.delete(&delete_id, None)
                            })
                            .and_then(move |runc| {
                                runc.list().and_then(move |(runc, containers)| {
                                    // Hack to keep the container from being prematurely dropped
                                    Ok((runc, container.id.clone(), containers))
                                })
                            }),
                    )
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, _, containers) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<
                    dyn Future<Item = (Runc, String, Vec<Event>), Error = Error> + Send,
                > {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    Box::new(
                        runc.events(&container.id, &Duration::from_secs(1))
                            .and_then(|(runc, events)| {
                                events.take(3).collect().and_then(move |events| {
                                    Ok((runc, container.id.clone(), events))
                                })
                            }),
                    )
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, _, events) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, TopResults), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let additional_console_socket =
                    env::temp_dir().join(&id).with_extension("console2");
                let receive_additional_pty_master =
                    match ReceivePtyMaster::new(&additional_console_socket) {
                        Ok(receive_additional_pty_master) => receive_additional_pty_master,
                        Err(e) => return Box::new(err(e)),
                    };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_additional_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                let capabilities = Some(vec![
                    String::from("CAP_AUDIT_WRITE"),
                    String::from("CAP_KILL"),
                    String::from("CAP_NET_BIND_SERVICE"),
                ]);

                Box::new(
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
                        .and_then(move |runc| {
                            runc.exec(&id, &Process{
                                terminal: Some(true),
                                console_size: None,
                                user: Some(User{
                                    uid: Some(0),
                                    gid: Some(0),
                                    additional_gids: None,
                                    username: None
                                }),
                                args: Some(vec![String::from("sleep"), String::from("10")]),
                                command_line: None,
                                env: Some(vec![String::from("PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"), String::from("TERM=xterm")]),
                                cwd: Some(String::from("/")),
                                capabilities: Some(LinuxCapabilities{
                                    bounding: capabilities.clone(),
                                    effective: capabilities.clone(),
                                    inheritable: capabilities.clone(),
                                    permitted: capabilities.clone(),
                                    ambient: capabilities.clone()
                                }),
                                rlimits: Some(vec![POSIXRlimit{
                                    limit_type: Some(String::from("RLIMIT_NOFILE")),
                                    hard: Some(1024),
                                    soft: Some(1024)
                                }]),
                                no_new_privileges: Some(false),
                                app_armor_profile: None,
                                oom_score_adj: None,
                                selinux_label: None
                            }, Some(&ExecOpts{
                                pid_file: Some(PathBuf::from("/tmp/bang.pid")),
                                console_socket: Some(additional_console_socket),
                                detach: true
                            })).and_then(move |runc| {
                                thread::sleep(Duration::from_millis(500));
                                runc.top(&id, None).and_then(move |(runc, processes)| runc.kill(&id, libc::SIGKILL, None).and_then(move |runc| Ok((runc, processes))))
                            })
                        }),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, processes) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<
                    dyn Future<Item = (Runc, String, Container), Error = Error> + Send,
                > {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };
                    Box::new(
                        runc.kill(&container.id, libc::SIGKILL, None)
                            .and_then(move |runc| {
                                thread::sleep(Duration::from_millis(500));
                                runc.state(&container.id).and_then(move |(runc, state)| {
                                    // container reference here is a kludge to avoid dropping the object early
                                    Ok((runc, container.id.clone(), state))
                                })
                            }),
                    )
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, _, state) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<dyn Future<Item = Runc, Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    Box::new(runc.list().and_then(move |(runc, containers)| {
                        if containers.len() != 1 {
                            return Err(Error::IoError(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "expected a single container",
                            )));
                        }
                        if let Some(container_item) = containers.get(0) {
                            if let Some(id) = container_item.id.clone() {
                                if id == container.id {
                                    return Ok(runc);
                                }
                            }
                        }
                        Err(Error::IoError(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "expected container to match",
                        )))
                    }))
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on_all(task).expect("test failed");
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(&runc_path, &runc_root, &PathBuf::from("test_fixture/busybox.tar.gz"))
                .and_then(move |container| -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    Box::new(runc.pause(&container.id).and_then(move |runc| runc.state(&container.id)))
                })
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, container) = runtime.block_on_all(task).expect("test failed");

        assert_eq!(container.status, Some(String::from("paused")));
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<dyn Future<Item = Runc, Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    // Time for shell to spawn
                    thread::sleep(Duration::from_millis(100));

                    Box::new(runc.ps(&container.id).and_then(|(runc, processes)| {
                        if processes.len() != 1 {
                            Err(Error::IoError(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "expected a single shell process",
                            )))
                        } else if let Some(pid) = processes.get(0) {
                            if *pid > 0 && *pid < 32768 {
                                Ok(runc)
                            } else {
                                Err(Error::IoError(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "invalid pid number",
                                )))
                            }
                        } else {
                            Err(Error::Unknown)
                        }
                    }))
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on_all(task).expect("test failed");
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(&runc_path, &runc_root, &PathBuf::from("test_fixture/busybox.tar.gz"))
                .and_then(move |container| -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    Box::new(runc.pause(&container.id).and_then(move |runc| runc.state(&container.id).and_then(move |(runc, state)| {
                        if let Some(status) = state.status {
                            if status == "paused" {
                                return Ok((runc, container.id.clone()));
                            }
                        }
                        Err(Error::IoError(io::Error::new(io::ErrorKind::InvalidData, "expected container to be paused")))
                    }).and_then(move |(runc, id)| runc.resume(&id).and_then(move |runc| runc.state(&id)))))
                })
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, container) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                Box::new(
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
                    .and_then(move |runc| {
                        thread::sleep(Duration::from_millis(500));
                        runc.state(&id)
                    }),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, container) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                let state_id = id.clone();
                let kill_id = id.clone();
                Box::new(
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
                    .and_then(move |runc| runc.start(&id))
                    .and_then(move |runc| {
                        thread::sleep(Duration::from_millis(500));
                        runc.state(&state_id)
                    })
                    .and_then(move |(runc, state)| {
                        runc.kill(&kill_id, libc::SIGKILL, None)
                            .and_then(move |runc| Ok((runc, state)))
                    }),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, container) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(&runc_path, &runc_root, &PathBuf::from("test_fixture/busybox.tar.gz"))
                .and_then(move |container| -> Box<dyn Future<Item = (Runc, Container), Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };
                    Box::new(runc.state(&container.id))
                })
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, state) = runtime.block_on_all(task).expect("test failed");

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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<dyn Future<Item = Runc, Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    Box::new(runc.stats(&container.id).and_then(|(runc, stats)| {
                        if let Some(memory) = stats.memory.clone() {
                            if let Some(usage) = memory.usage {
                                if let Some(usage) = usage.usage {
                                    if usage > 0 {
                                        return Ok(runc);
                                    }
                                }
                            }
                        }
                        Err(Error::IoError(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "missing memory usage statistics",
                        )))
                    }))
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on_all(task).expect("test failed");
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(|| {
            ManagedContainer::new(
                &runc_path,
                &runc_root,
                &PathBuf::from("test_fixture/busybox.tar.gz"),
            )
            .and_then(
                move |container| -> Box<dyn Future<Item = Runc, Error = Error> + Send> {
                    let mut config: RuncConfiguration = Default::default();
                    config.command = Some(runc_path.clone());
                    config.root = Some(runc_root.clone());
                    config.should_cleanup = true;
                    let runc = match Runc::new(config) {
                        Ok(runc) => runc,
                        Err(e) => return Box::new(err(e)),
                    };

                    // Time for shell to spawn
                    thread::sleep(Duration::from_millis(100));

                    Box::new(runc.top(&container.id, None).and_then(|(runc, processes)| {
                        if processes.len() != 1 {
                            return Err(Error::IoError(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "expected a single shell process",
                            )));
                        }
                        if let Some(process) = processes.get(0) {
                            if process["CMD"] != "[sh]" {
                                return Err(Error::IoError(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "expected shell",
                                )));
                            }
                        }
                        Ok(runc)
                    }))
                },
            )
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        runtime.block_on_all(task).expect("test failed");
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, Stats), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                // As an ugly hack leak the pty master handle for the lifecycle of the test
                // we can't close it and we also don't want to block on it (can interfere with deletes)
                tokio::spawn(
                    receive_pty_master
                        .and_then(|pty_master| {
                            Box::leak(Box::new(pty_master));
                            Ok(())
                        })
                        .map_err(|_| ()),
                );

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                let stats_id = id.clone();
                Box::new(
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
                    .and_then(move |runc| {
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
                    })
                    .and_then(move |runc| runc.stats(&stats_id)),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, stats) = runtime.block_on_all(task).expect("test failed");

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
        extract_tarball(
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let mut config: RuncConfiguration = Default::default();
        config.command = Some(runc_path);
        config.should_cleanup = true;
        let runc = Runc::new(config).expect("unable to build runc client");

        let task = runc.version();

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, version) = runtime.block_on_all(task).expect("test failed");

        assert_eq!(version.runc_version, Some(String::from("1.0.0-rc9")));
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
            &PathBuf::from("test_fixture/runc_v1.0.0-rc9.tar.gz"),
            &env::temp_dir().join(&runc_id),
        )
        .expect("unable to extract runc");

        let task = lazy(
            move || -> Box<dyn Future<Item = (Runc, tokio::fs::File), Error = Error> + Send> {
                let mut config: RuncConfiguration = Default::default();
                config.command = Some(runc_path.clone());
                config.root = Some(runc_root.clone());
                config.should_cleanup = true;
                let runc = match Runc::new(config) {
                    Ok(runc) => runc,
                    Err(e) => return Box::new(err(e)),
                };

                let id = format!("{}", Uuid::new_v4());
                let console_socket = env::temp_dir().join(&id).with_extension("console");
                let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                    Ok(receive_pty_master) => receive_pty_master,
                    Err(e) => return Box::new(err(e)),
                };

                let bundle = env::temp_dir().join(&id);
                if let Err(e) =
                    extract_tarball(&PathBuf::from("test_fixture/busybox.tar.gz"), &bundle)
                {
                    return Box::new(err(e.into()));
                }

                Box::new(
                    Delay::new(Instant::now().add(Duration::from_millis(100)))
                        .map_err(|_| Error::Unknown)
                        .and_then(move |_| {
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
                            .join(receive_pty_master)
                        }),
                )
            },
        );

        let runtime = Runtime::new().expect("unable to create runtime");
        let (runc, mut pty_master): (Runc, tokio::fs::File) =
            runtime.block_on_all(task).expect("test failed");

        let task = lazy(move || -> FutureResult<(Runc, String), Error> {
            let mut response = [0u8; 160];
            // Clear cursor
            if let Err(e) = pty_master.read(&mut response) {
                return err(e.into());
            }

            if let Err(e) = pty_master.write("uname -a && exit\n".as_bytes()) {
                return err(e.into());
            }

            thread::sleep(Duration::from_millis(500));

            match pty_master.read(&mut response) {
                Ok(len) => match String::from_utf8(Vec::from(&response[..len])) {
                    Ok(response) => ok((runc, response)),
                    Err(e) => err(e.into()),
                },
                Err(e) => err(e.into()),
            }
        });

        let runtime = Runtime::new().expect("unable to create runtime");
        let (_, response) = runtime.block_on_all(task).expect("test failed");

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
        fn new(
            runc_path: &PathBuf,
            runc_root: &PathBuf,
            compressed_bundle: &PathBuf,
        ) -> Box<dyn Future<Item = Self, Error = Error> + Send> {
            let id = format!("{}", Uuid::new_v4());
            let bundle = env::temp_dir().join(&id);
            if let Err(e) = extract_tarball(compressed_bundle, &bundle) {
                return Box::new(err(e.into()));
            }

            let mut config: RuncConfiguration = Default::default();
            config.command = Some(runc_path.clone());
            config.root = Some(runc_root.clone());
            let runc = match Runc::new(config) {
                Ok(runc) => runc,
                Err(e) => return Box::new(err(e)),
            };

            let console_socket = env::temp_dir().join(id.clone()).with_extension("console");
            let receive_pty_master = match ReceivePtyMaster::new(&console_socket) {
                Ok(receive_pty_master) => receive_pty_master,
                Err(e) => return Box::new(err(e)),
            };

            // As an ugly hack leak the pty master handle for the lifecycle of the test
            // we can't close it and we also don't want to block on it (can interfere with deletes)
            tokio::spawn(
                receive_pty_master
                    .and_then(|pty_master| {
                        Box::leak(Box::new(pty_master));
                        Ok(())
                    })
                    .map_err(|_| ()),
            );

            let start_id = id.clone();
            Box::new(
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
                .and_then(move |runc| runc.start(&start_id))
                .and_then(move |runc| {
                    Ok(Self {
                        id,
                        runc: Some(runc),
                    })
                }),
            )
        }
    }

    impl Drop for ManagedContainer {
        fn drop(&mut self) {
            if let Some(runc) = self.runc.take() {
                let bundle = env::temp_dir().join(&self.id);
                tokio::spawn(
                    runc.delete(&self.id, Some(&DeleteOpts { force: true }))
                        .and_then(move |_| {
                            fs::remove_dir_all(&bundle)?;
                            Ok(())
                        })
                        .map_err(|_| ()),
                );
            }
        }
    }
}
