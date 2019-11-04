# rust-runc

This a crate for consuming the runc binary in your Rust applications. The crate uses futures and tokio for non-blocking control of the runc binary.

The high level API of rust-runc is heavily inspired by the go-runc package.

# Tests

```
echo 1 > /proc/sys/kernel/unprivileged_userns_clone
mkdir /run/rust-runc
chown developer:developer /run/rust-runc
```