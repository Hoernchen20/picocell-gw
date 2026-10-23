//! TLS support: RNG driver for `embassy-crypto`, clock and the fail-closed verifier.

use core::cell::RefCell;
use core::sync::atomic::Ordering;

use critical_section::Mutex;
// Software SHA-256, HMAC, AES-GCM and P-256 drivers, registered at link time.
use embassy_crypto_rustcrypto as _;
use embassy_rp::peripherals::TRNG;
use embassy_rp::trng::Trng;
use embedded_tls::pki::CertVerifier;
use embedded_tls::{
    Aes128GcmSha256, Certificate, CertificateRef, CertificateVerifyRef, TlsCipherSuite, TlsClock,
    TlsError, TlsVerifier,
};
use portable_atomic::AtomicU64;

pub type Suite = Aes128GcmSha256;

static TRNG_CELL: Mutex<RefCell<Option<Trng<'static, TRNG>>>> = Mutex::new(RefCell::new(None));

/// Hands the TRNG to the `embassy-crypto` RNG driver. Released `embassy-rp` 0.10 has
/// no `embassy-crypto-rng` feature yet.
pub fn init_rng(trng: Trng<'static, TRNG>) {
    critical_section::with(|cs| TRNG_CELL.borrow_ref_mut(cs).replace(trng));
}

struct TrngDriver;

impl embassy_crypto::driver::Rng for TrngDriver {
    fn fill_bytes(buf: &mut [u8]) {
        critical_section::with(|cs| {
            let mut trng = TRNG_CELL.borrow_ref_mut(cs);
            trng.as_mut().expect("TRNG not initialised").blocking_fill_bytes(buf)
        })
    }
}

embassy_crypto::rng_impl!(TrngDriver);

/// Unix time in seconds, 0 = unknown.
static UNIX_TIME: AtomicU64 = AtomicU64::new(0);

pub fn set_time(unix: u64) {
    UNIX_TIME.store(unix, Ordering::Relaxed);
}

pub struct Clock;

impl TlsClock for Clock {
    fn now() -> Option<u64> {
        match UNIX_TIME.load(Ordering::Relaxed) {
            0 => None,
            t => Some(t),
        }
    }
}

/// `rustpki` skips the validity dates when the time is unknown; refuse instead
/// (sketch.md section 6).
pub struct FailClosed<'a> {
    inner: CertVerifier<'a, Suite, Clock, 2048>,
}

impl<'a> FailClosed<'a> {
    pub fn new(ca: &'a [u8]) -> Self {
        Self { inner: CertVerifier::new(Certificate::X509(ca)) }
    }
}

impl TlsVerifier<Suite> for FailClosed<'_> {
    fn set_hostname_verification(&mut self, hostname: &str) -> Result<(), TlsError> {
        self.inner.set_hostname_verification(hostname)
    }

    fn verify_certificate(
        &mut self,
        transcript: &<Suite as TlsCipherSuite>::Hash,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        if Clock::now().is_none() {
            log::warn!("time unknown, refusing the broker certificate");
            return Err(TlsError::InvalidCertificate);
        }
        self.inner.verify_certificate(transcript, cert)
    }

    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        self.inner.verify_signature(verify)
    }
}
