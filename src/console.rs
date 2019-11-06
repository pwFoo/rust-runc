/*
 * Copyright 2019 fsyncd, Berlin, Germany.
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

use std::ffi::c_void;
use std::io::ErrorKind;
use std::os::unix::io::AsRawFd;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::{fs, io, ptr};

use futures::{try_ready, Async, Future, Stream};
use log::warn;
use mio::Ready;
use tokio::fs::File;
use tokio::net::{UnixListener, UnixStream};

use crate::Error;

/// Receive a PTY master over the provided unix socket
pub struct ReceivePtyMaster {
    console_socket: PathBuf,
    console_stream: Option<UnixStream>,
    console_stream_future: Box<dyn Future<Item = UnixStream, Error = Error> + Send>,
}

impl ReceivePtyMaster {
    /// A future for a unix socket that will return a PTY master file descriptor
    pub fn new(console_socket: &PathBuf) -> Result<Self, Error> {
        let console_stream_future = Box::new(
            UnixListener::bind(console_socket)?
                .incoming()
                .take(1)
                .into_future()
                .then(|res| match res {
                    Ok((None, _)) => Err(Error::IoError(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "closed without receiving pty",
                    ))),
                    Ok((Some(stream), _)) => Ok(stream),
                    Err((e, _)) => Err(e.into()),
                }),
        );
        Ok(ReceivePtyMaster {
            console_socket: console_socket.clone(),
            console_stream: None,
            console_stream_future,
        })
    }
}

impl Future for ReceivePtyMaster {
    type Item = File;
    type Error = Error;

    // Looks to be a false positive
    #[allow(clippy::cast_ptr_alignment)]
    fn poll(&mut self) -> Result<Async<Self::Item>, Self::Error> {
        if self.console_stream.is_none() {
            self.console_stream
                .replace(try_ready!(self.console_stream_future.poll()));
        }

        loop {
            // 4096 is the max name length from the go-runc implementation
            let mut iov_base = [0u8; 4096];
            let mut message_buf = [0u8; 24];
            let mut io = libc::iovec {
                iov_len: iov_base.len(),
                iov_base: &mut iov_base as *mut _ as *mut c_void,
            };
            let mut msg = libc::msghdr {
                msg_name: ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut io,
                msg_iovlen: 1,
                msg_control: &mut message_buf as *mut _ as *mut c_void,
                msg_controllen: message_buf.len(),
                msg_flags: 0,
            };

            match self.console_stream.take() {
                Some(console_stream) => {
                    match console_stream.poll_read_ready(Ready::readable()) {
                        Ok(Async::Ready(_)) => (),
                        Ok(Async::NotReady) => {
                            self.console_stream.replace(console_stream);
                            return Ok(Async::NotReady);
                        }
                        Err(e) => return Err(e.into()),
                    };

                    let ret = unsafe { libc::recvmsg(console_stream.as_raw_fd(), &mut msg, 0) };
                    return if ret < 0 {
                        Err(Error::IoError(io::Error::new(
                            ErrorKind::BrokenPipe,
                            "pty not received",
                        )))
                    } else {
                        Ok(Async::Ready(unsafe {
                            let cmsg = libc::CMSG_FIRSTHDR(&msg);
                            if cmsg.is_null() {
                                continue;
                            }
                            let cmsg_data = libc::CMSG_DATA(cmsg);
                            if cmsg_data.is_null() {
                                return Err(Error::IoError(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "expected message data",
                                )));
                            }
                            File::from_std(std::fs::File::from_raw_fd(ptr::read_unaligned(
                                cmsg_data as *const i32,
                            )))
                        }))
                    };
                }
                None => return Err(Error::Unknown),
            }
        }
    }
}

impl Drop for ReceivePtyMaster {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_file(&self.console_socket) {
            warn!("failed to clean up console socket: {}", e);
        }
    }
}
