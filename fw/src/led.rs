//! Status LED on GP28 (sketch.md section 2.4), first problem wins:
//!
//! | Pattern | State |
//! |---|---|
//! | on | booting (from reset until the task starts, a few seconds) |
//! | 2 Hz blink | offline: provisioning mode, or a production build refusing to go online |
//! | 1 blink every 2 s | no Ethernet link |
//! | 2 blinks every 2 s | no broker: no address, no time, or MQTT not connected |
//! | 3 blinks every 2 s | online, but the concentrator is not running |
//! | on, short off on each frame | online, the off flash marks a received or sent frame |
//!
//! The state is taken at the start of each 2 s frame, so a blink count is never cut.

use embassy_net::Stack;
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Ticker};

use crate::{mqtt, radio};

const TICK: Duration = Duration::from_millis(50);
/// Ticks per frame (2 s).
const FRAME: u32 = 40;
/// A blink is 200 ms on, 200 ms off.
const BLINK: u32 = 4;
/// The off flash for a frame, 100 ms.
const FLASH: u32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pattern {
    Offline,
    Blinks(u32),
    Online,
}

fn pattern(stack: Option<Stack<'static>>) -> Pattern {
    let Some(stack) = stack else { return Pattern::Offline };
    if !stack.is_link_up() {
        Pattern::Blinks(1)
    } else if !mqtt::connected() {
        Pattern::Blinks(2)
    } else if matches!(radio::status().state, radio::State::NoAnswer | radio::State::Failed(_)) {
        Pattern::Blinks(3)
    } else {
        Pattern::Online
    }
}

/// Frames received and sent since the radio's last start.
fn frames() -> u32 {
    let s = radio::status();
    s.rx_ok.wrapping_add(s.rx_bad).wrapping_add(s.tx_ok)
}

/// `stack` is `None` when the gateway stays offline.
#[embassy_executor::task]
pub async fn led_task(mut led: Output<'static>, stack: Option<Stack<'static>>) -> ! {
    let mut ticker = Ticker::every(TICK);
    let mut p = pattern(stack);
    let mut seen = frames();
    let mut flash = 0;
    let mut t = 0;
    loop {
        let on = match p {
            Pattern::Offline => t % 10 < 5,
            Pattern::Blinks(n) => t < 2 * BLINK * n && t % (2 * BLINK) < BLINK,
            Pattern::Online => {
                let n = frames();
                if n != seen {
                    seen = n;
                    flash = FLASH;
                }
                if flash > 0 {
                    flash -= 1;
                    false
                } else {
                    true
                }
            }
        };
        led.set_level(on.into());
        ticker.next().await;
        t += 1;
        if t == FRAME {
            t = 0;
            p = pattern(stack);
        }
    }
}
