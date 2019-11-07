# rust-runc

A crate for consuming the runc binary in your Rust applications. Fully asynchronous using Futures, and Tokio.

Based on the reference [go-runc](https://github.com/containerd/go-runc) implementation.

## Usage

Please refer to the crate [documentation](https://docs.rs/rust-runc).

## Limitations

### Checkpoint Support

Checkpoints rely on the external, checkpoint/restore in userspace project. The criu tool relies on a variety of kernel features and is not portable.

Due to the difficulties in shipping a criu binary for testing, and it's non portable nature, checkpoint support is currently out of scope for rust-runc.

### Preserving File Descriptors

Runc includes the ability to inherit file descriptors from a parent process. Rust's standard library does not include support for exec with additional file descriptors.

Therefore there is currently no support for preserving file descriptors.