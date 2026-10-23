//! Puts `memory.x` and the broker CA on the linker / include path and records the build
//! time (stand-in clock until SNTP exists).

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

    // Broker CA (DER). Relative paths are relative to fw/.
    let ca = manifest.join(env::var("PICOCELL_CA_DER").expect("PICOCELL_CA_DER not set"));
    if !ca.exists() {
        panic!(
            "broker CA {} not found; run tools/test-broker/setup.sh or set PICOCELL_CA_DER",
            ca.display()
        );
    }
    fs::copy(&ca, out.join("ca.der")).unwrap();
    println!("cargo:rerun-if-changed={}", ca.display());

    for key in [
        "PICOCELL_CA_DER",
        "PICOCELL_NET",
        "PICOCELL_NET_GATEWAY",
        "PICOCELL_MQTT_HOST",
        "PICOCELL_MQTT_ADDR",
        "PICOCELL_MQTT_PORT",
        "PICOCELL_MQTT_PASSWORD",
        "PICOCELL_TOPIC_PREFIX",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    println!("cargo:rustc-env=PICOCELL_BUILD_TIME={now}");
}
