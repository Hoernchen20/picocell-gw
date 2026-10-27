//! Signed CA updates (sketch.md sections 5.3 and 8.1, milestone 8e).
//!
//! A new broker CA comes on `command/ca`: a sequence number (u64 LE, by convention the
//! Unix time of signing), the certificate (DER; empty clears `tls.ca1`) and an ECDSA
//! P-256 signature (`r || s`, 64 bytes) over the SHA-256 of `DOMAIN`, sequence number
//! and certificate. The public key of the config-signing key, separate from the boot
//! key, is compiled in (`build.rs`, `PICOCELL_CONFIG_PUBKEY`). The message is not bound
//! to a gateway, so one signature serves all of them, and it may be retained: a
//! sequence number not above the stored one is refused (equal: `current`, no change),
//! so a recorded update cannot be replayed once a newer one is in. Accepted, it goes
//! into `tls.ca1` and the gateway restarts; answers on `state/ca`.
//!
//! The MQTT task tries `tls.ca1` after a handshake with `tls.ca0` failed. When that
//! works and the session is up, `tls.ca1` becomes `tls.ca0` and the old CA is dropped
//! (`config::promote_ca`).
//!
//! An image can bring a CA as well (`PICOCELL_CA` at build time, `IMAGE_CA`), with
//! the build time as its sequence number; `config` installs it at boot.

use core::fmt::Write;

use embassy_crypto::Sha256;
use embassy_crypto::p256::{Signature, VerifyingKey};
use log::{info, warn};

use crate::{clock, config};

/// Separates these signatures from any other use of the key.
const DOMAIN: &[u8] = b"picocell-gw ca update\0";
const SEQ: usize = 8;
const SIG: usize = 64;
/// Uncompressed SEC1 point.
const PUBLIC_KEY: &[u8; 65] = include_bytes!(concat!(env!("OUT_DIR"), "/config-signing.pub"));
/// A sequence number this far ahead of the clock is a mistake (milliseconds?) that
/// would block every later update.
const MAX_AHEAD_S: u64 = 86400;

/// The CA built into the image, empty if none.
pub const IMAGE_CA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/image-ca.der"));

pub type Reply = heapless::String<80>;

/// Sequence number and certificate of a correctly signed message.
fn verify(payload: &[u8]) -> Result<(u64, &[u8]), &'static str> {
    let n = payload.len().checked_sub(SEQ + SIG).ok_or("too short")?;
    let (signed, sig) = payload.split_at(SEQ + n);
    let key = VerifyingKey::from_sec1(PUBLIC_KEY).map_err(|_| "bad public key")?;
    let sig = Signature::from_bytes(sig.try_into().unwrap()).map_err(|_| "bad signature")?;
    let mut h = Sha256::new();
    h.update(DOMAIN);
    h.update(signed);
    key.verify_prehash(&h.finalize(), &sig).map_err(|_| "bad signature")?;
    let (seq, der) = signed.split_at(SEQ);
    Ok((u64::from_le_bytes(seq.try_into().unwrap()), der))
}

/// `Some(seq)` once stored, `None` if it is the current one.
async fn accept(payload: &[u8]) -> Result<Option<u64>, &'static str> {
    let (seq, der) = verify(payload)?;
    let stored = config::ca_seq();
    if seq == stored {
        return Ok(None);
    }
    if seq < stored {
        return Err("older than the stored sequence number");
    }
    if clock::now_unix_s().is_none_or(|now| seq > now + MAX_AHEAD_S) {
        return Err("sequence number in the future");
    }
    config::store_ca(seq, der).await?;
    Ok(Some(seq))
}

/// Handles a message on `command/ca`: the answer for `state/ca`, and whether to
/// restart (the new `tls.ca1` is used from the next boot on).
pub async fn handle(payload: &[u8]) -> (Reply, bool) {
    let mut r = Reply::new();
    match accept(payload).await {
        Ok(Some(seq)) => {
            info!("ca: tls.ca1 updated (sequence number {seq}), restarting");
            let _ = write!(r, "stored {seq}, restarting");
            (r, true)
        }
        Ok(None) => {
            let _ = write!(r, "current {}", config::ca_seq());
            (r, false)
        }
        Err(e) => {
            warn!("ca: update rejected: {e}");
            let _ = write!(r, "rejected: {e}");
            (r, false)
        }
    }
}
