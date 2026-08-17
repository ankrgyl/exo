use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

const BUILD_INPUTS: &[&str] = &[
    "Cargo.toml",
    "src/main.rs",
    "src/linux.rs",
    "../firecracker-protocol/Cargo.toml",
    "../firecracker-protocol/src/lib.rs",
];

fn main() {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR").expect("missing manifest directory");
    let manifest_dir = Path::new(&manifest_dir);
    let mut hasher = Sha256::new();
    for relative in BUILD_INPUTS {
        println!("cargo::rerun-if-changed={relative}");
        hasher.update(relative.as_bytes());
        hasher.update(
            fs::read(manifest_dir.join(relative))
                .unwrap_or_else(|error| panic!("reading guest build input {relative}: {error}")),
        );
    }
    println!(
        "cargo::rustc-env=EXO_FIRECRACKER_GUEST_BUILD_ID={:x}",
        hasher.finalize()
    );
}
