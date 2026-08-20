# Fuzzing

Install `cargo-fuzz`, then run the Firecracker protocol target on a nightly
toolchain:

```bash
cargo +nightly fuzz run firecracker_protocol --fuzz-dir fuzz
```

The target exercises the bounded length-prefix decoder and the typed JSON
request and response envelopes used by both sides of the Firecracker vsock
connection.
