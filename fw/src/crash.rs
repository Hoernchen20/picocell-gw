//! Panics and hangs of core 0: the reason goes into RAM the reset leaves alone
//! (`.uninit`) and the chip resets through the watchdog at once; the next boot logs it
//! (`take`). Without it both left only "reset by watchdog timeout" and the log was gone.
//!
//! A hang is caught by TIMER0 alarm 3 (the time driver uses alarm 0), routed to core
//! 0's NMI, so it fires with interrupts off too: `feed` moves it on every second from
//! `boot::watchdog_task`, and when it fires the record holds the interrupted PC and LR
//! (look them up with `addr2line -e <elf>`).

use core::arch::global_asm;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

use embassy_rp::pac;
use embassy_rp::peripherals::WATCHDOG;
use embassy_rp::watchdog::Watchdog;

const MAGIC: u32 = 0x5041_4e43; // "PANC"
/// Shorter than the watchdog's 8 s, so the NMI comes first.
const HANG_US: u32 = 5_000_000;
const ALARM: usize = 3;

struct Record {
    magic: u32,
    len: usize,
    text: [u8; 240],
}

#[unsafe(link_section = ".uninit.crash")]
static mut RECORD: Record = Record { magic: 0, len: 0, text: [0; 240] };

struct Writer<'a>(&'a mut Record);

impl Write for Writer<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let r = &mut *self.0;
        let n = s.len().min(r.text.len() - r.len);
        r.text[r.len..r.len + n].copy_from_slice(&s.as_bytes()[..n]);
        r.len += n;
        Ok(())
    }
}

/// Stores the reason and resets. Only this and `take` touch the record, and `take`
/// runs once at boot.
fn record_and_reset(reason: fmt::Arguments) -> ! {
    cortex_m::interrupt::disable();
    let r = unsafe { &mut *addr_of_mut!(RECORD) };
    r.len = 0;
    let _ = Writer(r).write_fmt(reason);
    r.magic = MAGIC;
    // The watchdog task holds the watchdog, but nothing else runs here any more.
    Watchdog::new(unsafe { WATCHDOG::steal() }).trigger_reset();
    loop {
        cortex_m::asm::nop();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    record_and_reset(format_args!("{info}"))
}

// The NMI with the stack frame of the code it interrupted.
global_asm!(
    ".section .text.NonMaskableInt",
    ".global NonMaskableInt",
    ".type NonMaskableInt, %function",
    ".thumb_func",
    "NonMaskableInt:",
    "tst lr, #4",
    "ite eq",
    "mrseq r0, msp",
    "mrsne r0, psp",
    "b {hung}",
    hung = sym hung,
);

extern "C" fn hung(frame: *const u32) -> ! {
    let (lr, pc) = unsafe { (frame.add(5).read(), frame.add(6).read()) };
    record_and_reset(format_args!("hung: core 0 at pc {pc:#010x}, lr {lr:#010x}"))
}

/// Routes the hang alarm to core 0's NMI and starts it. Core 0 only.
pub fn start_hang_detector() {
    feed();
    pac::TIMER0.inte().modify(|w| w.set_alarm(ALARM, true));
    pac::EPPB.nmi_mask0().modify(|m| *m |= 1 << pac::Interrupt::TIMER0_IRQ_3 as u32);
}

/// Moves the hang alarm `HANG_US` on.
pub fn feed() {
    let now = pac::TIMER0.timerawl().read();
    pac::TIMER0.alarm(ALARM).write_value(now.wrapping_add(HANG_US));
}

/// The reason of a crash before this boot, once.
pub fn take() -> Option<&'static str> {
    let r = unsafe { &mut *addr_of_mut!(RECORD) };
    if r.magic != MAGIC || r.len > r.text.len() {
        return None;
    }
    r.magic = 0;
    // Truncation may have cut a character in two.
    Some(match core::str::from_utf8(&r.text[..r.len]) {
        Ok(s) => s,
        Err(e) => unsafe { core::str::from_utf8_unchecked(&r.text[..e.valid_up_to()]) },
    })
}
