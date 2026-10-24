//! Puts `memory.x` on the linker path and records the build time (lower bound for SNTP
//! results).

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());

    fs::copy(manifest.join("memory.x"), out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    println!("cargo:rustc-env=PICOCELL_BUILD_TIME={now}");
}
