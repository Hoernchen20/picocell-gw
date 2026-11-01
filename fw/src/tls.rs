//! TLS support: RNG driver for `embassy-crypto`, clock and the fail-closed verifier.

use core::sync::atomic::{AtomicBool, Ordering};

// Software SHA-256, HMAC, AES-GCM and P-256 drivers, registered at link time.
use embassy_crypto_rustcrypto as _;
use embassy_crypto::driver::Rng;
use embassy_rp::Peri;
use embassy_rp::pac;
use embassy_rp::peripherals::TRNG;
use embassy_time::{Duration, Instant};
use embedded_tls::pki::CertVerifier;
use embedded_tls::{
    Aes128GcmSha256, Certificate, CertificateRef, CertificateVerifyRef, TlsCipherSuite, TlsClock,
    TlsError, TlsVerifier,
};

use crate::clock;

pub type Suite = Aes128GcmSha256;

/// The TRNG is read here, not with `embassy_rp::trng`: its blocking read waits for
/// TRNG_BUSY to clear before looking at the errors, but after the autocorrelation test
/// failed four times in a row the TRNG stops until reset and stays busy, so core 0
/// hung there with interrupts off (on the dev board, in about one boot in three on some days).
/// Here a stall resets it; the wait holds only this lock between the two cores, not a
/// critical section.
static TRNG_LOCK: AtomicBool = AtomicBool::new(false);
/// System clocks between ROSC samples; 25 in `embassy_rp::trng`, where the
/// autocorrelation test failed too often.
const SAMPLE_COUNT: u32 = 100;
/// One 192-bit block takes well under 1 ms.
const BLOCK_TIMEOUT: Duration = Duration::from_millis(10);
const BLOCK: usize = 24;
const TRIES: u32 = 8;

/// Takes the TRNG for the `embassy-crypto` RNG driver. Released `embassy-rp` 0.10 has
/// no `embassy-crypto-rng` feature yet.
pub fn init_rng(_trng: Peri<'static, TRNG>) {
    trng_reset();
}

/// Software reset and configuration: no interrupts, all health tests on.
fn trng_reset() {
    let t = pac::TRNG;
    t.trng_sw_reset().write(|w| w.set_trng_sw_reset(true));
    // The datasheet asks for a delay after the reset; a read is enough.
    t.trng_sw_reset().read();
    t.rng_imr().write(|w| {
        w.set_ehr_valid_int_mask(true);
        w.set_autocorr_err_int_mask(true);
        w.set_crngt_err_int_mask(true);
        w.set_vn_err_int_mask(true);
    });
    t.trng_config().write(|w| w.set_rnd_src_sel(0));
    t.sample_cnt1().write_value(SAMPLE_COUNT);
    t.trng_debug_control().write(|_| {});
    t.rng_icr().write(|w| {
        w.set_ehr_valid(true);
        w.set_autocorr_err(true);
        w.set_crngt_err(true);
        w.set_vn_err(true);
    });
}

/// One block, or `None` after an autocorrelation error or a stall (the TRNG is reset
/// then). CRNGT and Von Neumann errors only drop bits; the TRNG goes on by itself.
fn trng_block(out: &mut [u8; BLOCK]) -> Option<()> {
    let t = pac::TRNG;
    t.rnd_source_enable().write(|w| w.set_rnd_src_en(true));
    let start = Instant::now();
    let r = loop {
        let isr = t.rng_isr().read();
        if isr.autocorr_err() {
            log::warn!("trng: autocorrelation test failed, resetting");
            break None;
        }
        if t.trng_valid().read().ehr_valid() {
            let regs = [t.ehr_data0(), t.ehr_data1(), t.ehr_data2(), t.ehr_data3(), t.ehr_data4(), t.ehr_data5()];
            // Reading EHR_DATA5 last clears the block.
            for (chunk, reg) in out.chunks_mut(4).zip(regs) {
                chunk.copy_from_slice(&reg.read().to_le_bytes());
            }
            break Some(());
        }
        if start.elapsed() > BLOCK_TIMEOUT {
            log::warn!("trng: no data after {} ms (isr {:#x}), resetting", BLOCK_TIMEOUT.as_millis(), isr.0);
            break None;
        }
    };
    t.rnd_source_enable().write(|w| w.set_rnd_src_en(false));
    t.rst_bits_counter().write(|w| w.set_rst_bits_counter(true));
    if r.is_none() {
        trng_reset();
    }
    r
}

struct TrngDriver;

impl Rng for TrngDriver {
    fn fill_bytes(buf: &mut [u8]) {
        // Only tasks of both cores call this, no interrupt handler.
        while TRNG_LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        let mut block = [0u8; BLOCK];
        for chunk in buf.chunks_mut(BLOCK) {
            // A TRNG that keeps failing is broken hardware: the panic is recorded.
            (0..TRIES).find_map(|_| trng_block(&mut block)).expect("TRNG keeps failing");
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        TRNG_LOCK.store(false, Ordering::Release);
    }
}

embassy_crypto::rng_impl!(TrngDriver);

/// Random bytes from the TRNG, e.g. DNS IDs and SNTP cookies.
pub fn random_bytes(buf: &mut [u8]) {
    TrngDriver::fill_bytes(buf)
}

pub struct Clock;

impl TlsClock for Clock {
    fn now() -> Option<u64> {
        clock::now_unix_s()
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
