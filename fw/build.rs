//! Puts `memory.x` on the linker path, records the build time (lower bound for SNTP
//! results) and embeds the config-signing public key and the image's CA certificate
//! (sketch.md section 8.1, 8e):
//!
//! - `PICOCELL_CONFIG_PUBKEY`: P-256 public key (PEM) that signed CA updates must
//!   verify against; default the test key in `keys/`, required for production builds.
//! - `PICOCELL_CA`: optional CA certificate (PEM or DER) that the image installs into
//!   `tls.ca1` once, so new CAs can also come with a signed image (`ca_update`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// SubjectPublicKeyInfo of a P-256 key up to the uncompressed point.
const P256_SPKI: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48,
    0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

fn base64(s: &str) -> Vec<u8> {
    let (mut acc, mut bits, mut out) = (0u32, 0, Vec::new());
    for b in s.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("invalid base64"),
        };
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// The body of the PEM block `label`, or the file as it is if it is no PEM.
fn pem_or_der(path: &Path, label: &str) -> Vec<u8> {
    let data = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let Ok(text) = std::str::from_utf8(&data) else {
        return data;
    };
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let Some(start) = text.find(&begin) else {
        return data;
    };
    let body = &text[start + begin.len()..];
    base64(&body[..body.find(&end).unwrap_or_else(|| panic!("{}: no {end}", path.display()))])
}

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

    println!("cargo:rerun-if-env-changed=PICOCELL_CONFIG_PUBKEY");
    let pubkey = match env::var_os("PICOCELL_CONFIG_PUBKEY") {
        Some(p) => PathBuf::from(p),
        None if env::var_os("CARGO_FEATURE_PRODUCTION").is_some() => {
            panic!("production build: set PICOCELL_CONFIG_PUBKEY to the config-signing public key (PEM)")
        }
        None => manifest.join("keys/test-config-signing.pub.pem"),
    };
    println!("cargo:rerun-if-changed={}", pubkey.display());
    let spki = pem_or_der(&pubkey, "PUBLIC KEY");
    if spki.len() != P256_SPKI.len() + 65 || spki[..P256_SPKI.len()] != P256_SPKI {
        panic!("{}: not an uncompressed P-256 public key", pubkey.display());
    }
    fs::write(out.join("config-signing.pub"), &spki[P256_SPKI.len()..]).unwrap();

    println!("cargo:rerun-if-env-changed=PICOCELL_CA");
    let ca = match env::var_os("PICOCELL_CA") {
        Some(p) => {
            let p = PathBuf::from(p);
            println!("cargo:rerun-if-changed={}", p.display());
            pem_or_der(&p, "CERTIFICATE")
        }
        None => Vec::new(),
    };
    // Its sequence number is PICOCELL_BUILD_TIME, the time this script ran, which a
    // change of PICOCELL_CA or of the file brings forward.
    fs::write(out.join("image-ca.der"), ca).unwrap();
}
