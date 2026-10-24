//! USB console on the USB-C port (CDC-ACM): log output and commands on one serial port.
//!
//! Every log line goes into an 8 KB RAM history. When a terminal opens the port, the
//! history is replayed, so boot messages show up even though the terminal
//! connected later; after that new lines appear live. `log off|warn|info` filters the
//! live output (RAM only, `info` after boot); `log show` prints the history unfiltered.
//! A log line arriving while a command is typed is printed above the input line, which
//! is then redrawn. Every command ends with `ok` or `error: ...` on a line of its own.

use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU8, Ordering};

use console::{Action, Editor, History, Read, Trunc};
use critical_section::Mutex;
use embassy_futures::join::join3;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::Stack;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};
use heapless::Vec;
use jit::duty::EU868;
use log::{Level, LevelFilter, Metadata, Record};
use static_cell::StaticCell;
use wire::time::Utc;

use crate::ids::Ids;
use crate::{clock, config, downlink, mqtt};

type Usb = Driver<'static, USB>;

static HISTORY: Mutex<RefCell<History<8192>>> = Mutex::new(RefCell::new(History::new()));
/// A line was added to the history.
static LOGGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
/// Received bytes, from the USB reader to the session.
static INPUT: Pipe<CriticalSectionRawMutex, 128> = Pipe::new();

/// Live log output: 0 off, 1 warnings and errors, 2 everything.
static LEVEL: AtomicU8 = AtomicU8::new(2);
const LEVELS: [&str; 3] = ["off", "warn", "info"];

const PROMPT: &str = "picocell> ";
const PACKET: usize = 64;

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let (tag, prefix) = match record.level() {
            Level::Error => (b'E', "error: "),
            Level::Warn => (b'W', "warning: "),
            _ => (b'I', ""),
        };
        let ms = Instant::now().as_millis();
        let mut buf = [0u8; 200];
        let mut t = Trunc::new(&mut buf);
        let _ = write!(t, "[{:>6}.{:03}] {prefix}{}", ms / 1000, ms % 1000, record.args());
        critical_section::with(|cs| HISTORY.borrow_ref_mut(cs).push(tag, t.as_bytes()));
        LOGGED.signal(());
    }

    fn flush(&self) {}
}

/// Installs the logger; lines are kept from here on, before USB is up.
pub fn init() {
    static LOGGER: Logger = Logger;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

/// The terminal went away (USB unplugged or port closed).
struct Gone;

/// Collects output into USB packets.
struct Out<'a> {
    tx: &'a mut Sender<'static, Usb>,
    buf: Vec<u8, PACKET>,
    /// The last packet sent was full, so the host waits for a short one.
    full: bool,
}

impl Out<'_> {
    async fn send(&mut self) -> Result<(), Gone> {
        match self.tx.write_packet(&self.buf).await {
            Err(EndpointError::Disabled) => return Err(Gone),
            _ => {}
        }
        self.full = self.buf.len() == PACKET;
        self.buf.clear();
        Ok(())
    }

    async fn write(&mut self, mut data: &[u8]) -> Result<(), Gone> {
        while !data.is_empty() {
            let n = (PACKET - self.buf.len()).min(data.len());
            let _ = self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            if self.buf.is_full() {
                self.send().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Gone> {
        if !self.buf.is_empty() || self.full {
            self.send().await?;
        }
        Ok(())
    }
}

/// Formats into a line buffer, then writes it: `out!(o, "...", args)?`.
macro_rules! out {
    ($o:expr, $($arg:tt)*) => {{
        let mut b = [0u8; 256];
        let n = {
            let mut t = Trunc::new(&mut b);
            let _ = write!(t, $($arg)*);
            t.as_bytes().len()
        };
        $o.write(&b[..n]).await
    }};
}

/// Writes history lines from `cursor` on that pass the live level (or all with
/// `level` 2); `before` goes ahead of the first line. Returns whether anything was
/// written.
async fn log_lines(o: &mut Out<'_>, cursor: &mut u64, level: u8, before: &[u8]) -> Result<bool, Gone> {
    let mut written = false;
    loop {
        let mut line = [0u8; 256];
        let r = critical_section::with(|cs| HISTORY.borrow_ref(cs).read(*cursor, &mut line));
        let text: &[u8] = match r {
            Read::End => return Ok(written),
            Read::Lost { next } => {
                *cursor = next;
                b"[earlier lines dropped]"
            }
            Read::Line { tag, len, next } => {
                *cursor = next;
                let shown = match level {
                    0 => false,
                    1 => tag != b'I',
                    _ => true,
                };
                if !shown {
                    continue;
                }
                &line[..len]
            }
        };
        if level == 0 {
            continue;
        }
        if !written {
            o.write(before).await?;
            written = true;
        }
        o.write(text).await?;
        o.write(b"\r\n").await?;
    }
}

pub struct Context {
    pub stack: Stack<'static>,
    pub ids: &'static Ids,
}

#[embassy_executor::task]
pub async fn console_task(driver: Usb, ctx: Context) -> ! {
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("picocell");
    config.product = Some("picocell-gw console");
    // /dev/serial/by-id/usb-picocell_picocell-gw_console_<gateway id>-if00
    config.serial_number = Some(ctx.ids.gateway_id.as_str());
    config.max_power = 100;
    config.max_packet_size_0 = PACKET as u8;

    struct Bufs {
        config_descriptor: [u8; 128],
        bos_descriptor: [u8; 16],
        msos_descriptor: [u8; 256],
        control: [u8; 64],
        state: State<'static>,
    }
    static BUFS: StaticCell<Bufs> = StaticCell::new();
    let b = BUFS.init(Bufs {
        config_descriptor: [0; 128],
        bos_descriptor: [0; 16],
        msos_descriptor: [0; 256],
        control: [0; 64],
        state: State::new(),
    });
    let mut builder = Builder::new(
        driver,
        config,
        &mut b.config_descriptor,
        &mut b.bos_descriptor,
        &mut b.msos_descriptor,
        &mut b.control,
    );
    let class = CdcAcmClass::new(&mut builder, &mut b.state, PACKET as u16);
    let (mut tx, mut rx, control) = class.split_with_control();
    let mut device = builder.build();
    join3(device.run(), reader(&mut rx), sessions(&mut tx, &control, &ctx)).await;
    unreachable!()
}

async fn reader(rx: &mut Receiver<'static, Usb>) -> ! {
    let mut buf = [0u8; PACKET];
    loop {
        rx.wait_connection().await;
        loop {
            match rx.read_packet(&mut buf).await {
                Ok(n) => INPUT.write_all(&buf[..n]).await,
                Err(EndpointError::Disabled) => break,
                Err(_) => {}
            }
        }
    }
}

async fn sessions(tx: &mut Sender<'static, Usb>, control: &ControlChanged<'static>, ctx: &Context) -> ! {
    loop {
        tx.wait_connection().await;
        opened(control).await;
        let mut o = Out { tx: &mut *tx, buf: Vec::new(), full: false };
        let _ = session(&mut o, control, ctx).await;
    }
}

/// Waits until a terminal has opened the port. Every open sends the control line
/// state (and usually the line coding), but DTR only drops on close with HUPCL, which
/// minicom clears for good. So any change counts as an open, once the port has been
/// quiet for 300 ms: terminal programs flush what arrives while they set it up.
async fn opened(control: &ControlChanged<'static>) {
    loop {
        while let Either::First(()) = select(control.control_changed(), Timer::after_millis(300)).await {}
        if control.dtr() {
            return;
        }
        control.control_changed().await;
    }
}

async fn session(o: &mut Out<'_>, control: &ControlChanged<'static>, ctx: &Context) -> Result<(), Gone> {
    INPUT.clear();
    let mut editor: Editor<128> = Editor::new();
    let mut cursor = critical_section::with(|cs| HISTORY.borrow_ref(cs).oldest());
    o.write(b"\r\n").await?;
    log_lines(o, &mut cursor, LEVEL.load(Ordering::Relaxed), b"").await?;
    out!(o, "picocell-gw {} console, \"help\" for commands\r\n{PROMPT}", env!("CARGO_PKG_VERSION"))?;
    o.flush().await?;
    loop {
        let mut input = [0u8; 64];
        match select3(INPUT.read(&mut input), LOGGED.wait(), control.control_changed()).await {
            Either3::First(n) => {
                for &b in &input[..n] {
                    match editor.feed(b) {
                        Action::None => {}
                        Action::Echo(c) => o.write(&[c]).await?,
                        Action::Erase => o.write(b"\x08 \x08").await?,
                        Action::Bell => o.write(b"\x07").await?,
                        Action::Cancel => out!(o, "^C\r\n{PROMPT}")?,
                        Action::Clear => out!(o, "\r\x1b[K{PROMPT}")?,
                        Action::Enter => {
                            o.write(b"\r\n").await?;
                            let line = editor.take();
                            command(o, core::str::from_utf8(&line).unwrap_or(""), &mut cursor, ctx).await?;
                            o.write(PROMPT.as_bytes()).await?;
                        }
                    }
                }
            }
            Either3::Second(()) => {}
            // Closed, or opened again (see `opened`).
            Either3::Third(()) => return Ok(()),
        }
        // New log lines go above the input line, which is then redrawn.
        if log_lines(o, &mut cursor, LEVEL.load(Ordering::Relaxed), b"\r\x1b[K").await? {
            out!(o, "{PROMPT}{}", editor.line())?;
        }
        o.flush().await?;
    }
}

const HELP: &str = "\
status               firmware, network, time, MQTT and duty-cycle state\r\n\
log                  live log level\r\n\
log off|warn|info    set the live log level (until reboot)\r\n\
log show [lines]     print the log history, or its last lines\r\n\
reboot               restart\r\n\
bootsel              restart into the USB bootloader (RP2350 drive) for flashing\r\n";

async fn command(o: &mut Out<'_>, line: &str, cursor: &mut u64, ctx: &Context) -> Result<(), Gone> {
    let mut args = line.split_whitespace();
    let Some(cmd) = args.next() else {
        return Ok(());
    };
    let (a1, a2) = (args.next(), args.next());
    let r = match (cmd, a1, a2) {
        ("help", None, _) => {
            o.write(HELP.as_bytes()).await?;
            Ok(())
        }
        ("status", None, _) => {
            status(o, ctx).await?;
            Ok(())
        }
        ("log", None, _) => {
            out!(o, "{}\r\n", LEVELS[LEVEL.load(Ordering::Relaxed) as usize])?;
            Ok(())
        }
        ("log", Some("show"), n) => match n.map(str::parse::<usize>) {
            None | Some(Ok(_)) => {
                let mut from = critical_section::with(|cs| {
                    let h = HISTORY.borrow_ref(cs);
                    n.and_then(|n| n.parse().ok()).map_or(h.oldest(), |n| h.last_lines(n))
                });
                log_lines(o, &mut from, 2, b"").await?;
                // Not again as live output.
                *cursor = (*cursor).max(from);
                Ok(())
            }
            Some(Err(_)) => Err("line count expected"),
        },
        ("log", Some(l), None) => match LEVELS.iter().position(|x| *x == l) {
            Some(i) => {
                LEVEL.store(i as u8, Ordering::Relaxed);
                Ok(())
            }
            None => Err("off, warn, info or show expected"),
        },
        ("reboot", None, _) | ("bootsel", None, _) => {
            o.write(b"ok\r\n").await?;
            o.flush().await?;
            // Let the reply reach the host.
            Timer::after_millis(100).await;
            if cmd == "bootsel" {
                embassy_rp::rom_data::reset_to_usb_boot(0, 0);
            }
            cortex_m::peripheral::SCB::sys_reset();
        }
        ("help" | "status" | "log" | "reboot" | "bootsel", _, _) => Err("too many arguments"),
        _ => Err("unknown command, see \"help\""),
    };
    match r {
        Ok(()) => o.write(b"ok\r\n").await,
        Err(e) => out!(o, "error: {e}\r\n"),
    }
}

async fn status(o: &mut Out<'_>, ctx: &Context) -> Result<(), Gone> {
    let m = ctx.ids.mac;
    let up = Instant::now().as_secs();
    out!(o, "firmware   {}, built {}\r\n", env!("CARGO_PKG_VERSION"), Utc(config::build_time()))?;
    out!(o, "gateway id {}\r\n", ctx.ids.gateway_id)?;
    out!(o, "mac        {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\r\n", m[0], m[1], m[2], m[3], m[4], m[5])?;
    match ctx.stack.config_v4() {
        Some(c) => {
            out!(o, "ip         {}", c.address)?;
            if let Some(g) = c.gateway {
                out!(o, ", gateway {g}")?;
            }
            o.write(b"\r\n").await?;
        }
        None => o.write(b"ip         none\r\n").await?,
    }
    out!(o, "time       {}\r\n", clock::Now)?;
    out!(o, "uptime     {}d {:02}:{:02}:{:02}\r\n", up / 86400, up / 3600 % 24, up / 60 % 60, up % 60)?;
    out!(o, "mqtt       {} ({}:{})\r\n", mqtt::state(), config::MQTT_HOST, config::mqtt_port().unwrap_or(0))?;
    let used = downlink::duty_cycle_used();
    for (b, used) in EU868.iter().zip(used) {
        out!(o, "duty cycle {:>12} MHz {:>6} of {:>6} ms/h\r\n", b.name, used / 1000, b.limit_us() / 1000)?;
    }
    Ok(())
}
