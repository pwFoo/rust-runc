# rust-runc

A crate for consuming the runc binary in your Rust applications. Fully asynchronous using Rust Futures and Tokio.

Based on the reference go-runc implementation, rust-runc has a compatible high level API for working with containers.

## Usage

Please refer to the crate documentation

## Rust-runc Limitations

### No Checkpoint Support

Checkpoints rely on the external, checkpoint/restore in userspace project. The criu tool relies on a variety of kernel features and is not portable.

Due to the difficulties in shipping a criu binary for testing, and it's non portable nature, checkpoint support is currently out of scope for the project.

### No Support for Preserving File Descriptors

Runc includes the ability to inherit file descriptors from it's parent process. Rust's standard library does not include support for exec with additional file descriptors. 

The ability to preserve additional file descriptors is not currently supported by rust-runc.