//! USB console on the USB-C port (CDC-ACM): log output and commands on one serial port.
//!
//! Every log line goes into an 8 KB RAM history. When a terminal opens the port, the
//! history is replayed, so boot messages show up even though the terminal
//! connected later; after that new lines appear live. `log off|warn|info` filters the
//! live output (RAM only, `info` after boot); `log show` prints the history unfiltered.
//! A log line arriving while a command is typed is printed above the input line, which
//! is then redrawn. Every command ends with `ok` or `error: ...` on a line of its own.
//!
//! The config is edited in a RAM copy (`show`, `set`, `unset`, `ca`) and stored with
//! `commit`, which restarts the gateway with it. Secrets are never printed.
//!
//! With `console.password` stored, most commands need `login` first (`crate::login`);
//! a session is logged out when the port closes or after 10 minutes without input.

use core::cell::RefCell;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU8, Ordering};

use console::{Action, Editor, History, Read, Trunc};
use critical_section::Mutex;
use embassy_futures::join::join3;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_crypto::Sha256;
use embassy_net::Stack;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::Driver;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};
use heapless::{String, Vec};
use jit::duty::EU868;
use log::{Level, LevelFilter, Metadata, Record};
use settings::{CA_KEYS, KEYS, Pem, check_password};
use static_cell::StaticCell;
use wire::time::Utc;

use crate::config::Settings;
use crate::ids::Ids;
use crate::{boot, clock, config, config_key, downlink, login, mqtt, radio};

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
/// A logged-in session without input is logged out after this.
const IDLE: Duration = Duration::from_secs(600);

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
    /// `None` in provisioning mode.
    pub stack: Option<Stack<'static>>,
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
    // The config being edited; kept while terminals come and go.
    static PENDING: StaticCell<Settings> = StaticCell::new();
    let pending = PENDING.init_with(|| config::get().clone());
    join3(device.run(), reader(&mut rx), sessions(&mut tx, &control, &ctx, pending)).await;
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

async fn sessions(
    tx: &mut Sender<'static, Usb>,
    control: &ControlChanged<'static>,
    ctx: &Context,
    pending: &mut Settings,
) -> ! {
    loop {
        tx.wait_connection().await;
        opened(control).await;
        let mut o = Out { tx: &mut *tx, buf: Vec::new(), full: false };
        let _ = session(&mut o, control, ctx, pending).await;
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

/// Longest input line: `set net.hosts` with four long entries.
const LINE: usize = 384;

/// What the next input line is.
enum Input {
    Command,
    /// The value of this secret key, echoed as `*`.
    Secret(&'static str),
    /// A line of a PEM certificate for this CA slot.
    Pem(usize, Pem),
    /// "yes" to write the config key into OTP.
    ConfirmOtpKey,
    /// The console password, to log in.
    Login,
    /// A new console password, then the same again.
    NewPassword,
    RepeatPassword(String<64>),
}

fn prompt(input: &Input) -> &'static str {
    match input {
        Input::Command => PROMPT,
        Input::Secret(_) => "value: ",
        Input::Pem(..) => "",
        Input::ConfirmOtpKey => "write the key? ",
        Input::Login => "password: ",
        Input::NewPassword => "new password: ",
        Input::RepeatPassword(_) => "again: ",
    }
}

/// Echoed as `*`.
fn masked(input: &Input) -> bool {
    matches!(input, Input::Secret(_) | Input::Login | Input::NewPassword | Input::RepeatPassword(_))
}

/// Prompt and the line typed so far, secrets masked.
async fn redraw(o: &mut Out<'_>, input: &Input, editor: &Editor<LINE>) -> Result<(), Gone> {
    o.write(prompt(input).as_bytes()).await?;
    if masked(input) {
        for _ in 0..editor.line().len() {
            o.write(b"*").await?;
        }
        Ok(())
    } else {
        o.write(editor.line().as_bytes()).await
    }
}

async fn session(
    o: &mut Out<'_>,
    control: &ControlChanged<'static>,
    ctx: &Context,
    pending: &mut Settings,
) -> Result<(), Gone> {
    INPUT.clear();
    let mut editor: Editor<LINE> = Editor::new();
    let mut input = Input::Command;
    let mut logged_in = !login::required();
    let mut last_input = Instant::now();
    let mut cursor = critical_section::with(|cs| HISTORY.borrow_ref(cs).oldest());
    o.write(b"\r\n").await?;
    log_lines(o, &mut cursor, LEVEL.load(Ordering::Relaxed), b"").await?;
    out!(o, "picocell-gw {} console, \"help\" for commands\r\n", env!("CARGO_PKG_VERSION"))?;
    if !logged_in {
        o.write(b"console locked: \"login\" first, \"help\" lists the commands\r\n").await?;
    } else if !config::provisioned() {
        o.write(b"provisioning mode: \"show\" lists the keys, \"set\" them, then \"commit\"\r\n").await?;
    } else if let Some(why) = boot::refuse_online() {
        out!(o, "offline: production build on a board that is not locked ({why})\r\n")?;
    }
    o.write(PROMPT.as_bytes()).await?;
    o.flush().await?;
    loop {
        let mut rx = [0u8; 64];
        let idle = if logged_in && login::required() { last_input + IDLE } else { Instant::MAX };
        match select4(INPUT.read(&mut rx), LOGGED.wait(), control.control_changed(), Timer::at(idle)).await {
            Either4::First(n) => {
                last_input = Instant::now();
                for &b in &rx[..n] {
                    match editor.feed(b) {
                        Action::None => {}
                        Action::Echo(c) => {
                            let c = if masked(&input) { b'*' } else { c };
                            o.write(&[c]).await?
                        }
                        Action::Erase => o.write(b"\x08 \x08").await?,
                        Action::Bell => o.write(b"\x07").await?,
                        Action::Cancel => {
                            o.write(b"^C\r\n").await?;
                            if !matches!(input, Input::Command) {
                                input = Input::Command;
                                o.write(b"error: cancelled\r\n").await?;
                            }
                            o.write(PROMPT.as_bytes()).await?;
                        }
                        Action::Clear => {
                            o.write(b"\r\x1b[K").await?;
                            redraw(o, &input, &editor).await?;
                        }
                        Action::Enter => {
                            o.write(b"\r\n").await?;
                            let line = editor.take();
                            let line = core::str::from_utf8(&line).unwrap_or("");
                            input = match core::mem::replace(&mut input, Input::Command) {
                                Input::Command => command(o, line, &mut cursor, ctx, pending, &mut logged_in).await?,
                                Input::Login => {
                                    let r = login::check(line).await;
                                    logged_in |= r.is_ok();
                                    reply(o, r).await?;
                                    Input::Command
                                }
                                Input::NewPassword => match check_password(line) {
                                    Ok(()) => Input::RepeatPassword(String::try_from(line).unwrap()),
                                    Err(e) => {
                                        reply(o, Err(e)).await?;
                                        Input::Command
                                    }
                                },
                                Input::RepeatPassword(first) => {
                                    let r = if line == first {
                                        pending.set("console.password", &login::hash(line).await.format())
                                    } else {
                                        Err("the passwords differ")
                                    };
                                    reply(o, r).await?;
                                    Input::Command
                                }
                                Input::Secret(key) => {
                                    reply(o, pending.set(key, line)).await?;
                                    Input::Command
                                }
                                Input::ConfirmOtpKey if line.trim() == "yes" => match config_key::init_otp() {
                                    Ok(()) => restart(o, false).await,
                                    Err(e) => {
                                        reply(o, Err(e)).await?;
                                        Input::Command
                                    }
                                },
                                Input::ConfirmOtpKey => {
                                    reply(o, Err("cancelled")).await?;
                                    Input::Command
                                }
                                Input::Pem(slot, mut pem) => match pem.line(line) {
                                    Ok(false) => Input::Pem(slot, pem),
                                    Ok(true) => {
                                        let r = pending.set_ca(slot, pem.der());
                                        if r.is_ok() {
                                            out!(o, "{:<18} ", CA_KEYS[slot])?;
                                            ca_info(o, pending.ca(slot)).await?;
                                        }
                                        reply(o, r).await?;
                                        Input::Command
                                    }
                                    Err(e) => {
                                        reply(o, Err(e)).await?;
                                        Input::Command
                                    }
                                },
                            };
                            o.write(prompt(&input).as_bytes()).await?;
                        }
                    }
                }
            }
            Either4::Second(()) => {}
            // Closed, or opened again (see `opened`).
            Either4::Third(()) => return Ok(()),
            Either4::Fourth(()) => {
                logged_in = false;
                input = Input::Command;
                editor.take();
                o.write(b"\r\x1b[Klogged out after 10 minutes without input\r\n").await?;
                o.write(PROMPT.as_bytes()).await?;
            }
        }
        // New log lines go above the input line, which is then redrawn.
        if log_lines(o, &mut cursor, LEVEL.load(Ordering::Relaxed), b"\r\x1b[K").await? {
            redraw(o, &input, &editor).await?;
        }
        o.flush().await?;
    }
}

const HELP: &str = "\
status               firmware, boot slot, network, time, MQTT and duty-cycle state\r\n\
log                  live log level\r\n\
log off|warn|info    set the live log level (until reboot)\r\n\
log show [lines]     print the log history, or its last lines\r\n\
show                 the config being edited, secrets as ***\r\n\
set <key> <value>    change a key; \"set mqtt.password\" asks without echo\r\n\
set console.password asks for a new console password twice, without echo\r\n\
unset <key>          back to the default (or none); also tls.ca0 and tls.ca1\r\n\
ca 0|1               paste a CA certificate (PEM) into tls.ca0 or tls.ca1\r\n\
clear                start from an empty config\r\n\
discard              drop the changes since the last commit\r\n\
commit               save the config and restart with it\r\n\
erase                delete the stored config and restart (provisioning mode)\r\n\
radio                state of the WM1302 concentrator, EUI and received frames\r\n\
radio restart        power-cycle the WM1302 and start the concentrator again\r\n\
otp-key              state of the config key and its OTP page\r\n\
otp-key init         production only: write a random config key into OTP and restart\r\n\
reboot               restart\r\n\
bootsel              restart into the USB bootloader (RP2350 drive) for flashing\r\n\
login                with console.password set: needed for all but help, status, log and erase\r\n\
logout               lock the console again (also on closing the port, or 10 minutes idle)\r\n";

async fn reply(o: &mut Out<'_>, r: Result<(), &str>) -> Result<(), Gone> {
    match r {
        Ok(()) => o.write(b"ok\r\n").await,
        Err(e) => out!(o, "error: {e}\r\n"),
    }
}

/// Replies `ok` and restarts once the reply had time to reach the host, also when
/// the terminal is gone (a saved config takes effect only after a restart).
async fn restart(o: &mut Out<'_>, bootsel: bool) -> ! {
    let _ = o.write(b"ok\r\n").await;
    let _ = o.flush().await;
    Timer::after_millis(100).await;
    if bootsel {
        embassy_rp::rom_data::reset_to_usb_boot(0, 0);
    }
    cortex_m::peripheral::SCB::sys_reset()
}

/// Runs a command line; returns what the next input line is.
async fn command(
    o: &mut Out<'_>,
    line: &str,
    cursor: &mut u64,
    ctx: &Context,
    pending: &mut Settings,
    logged_in: &mut bool,
) -> Result<Input, Gone> {
    let line = line.trim();
    let (cmd, rest) = line.split_once(' ').map_or((line, ""), |(c, r)| (c, r.trim_start()));
    if cmd.is_empty() {
        return Ok(Input::Command);
    }
    if !*logged_in && !matches!(cmd, "help" | "status" | "log" | "login" | "logout" | "erase") {
        reply(o, Err("console locked, \"login\" first")).await?;
        return Ok(Input::Command);
    }
    let mut args = rest.split_whitespace();
    let (a1, a2) = (args.next(), args.next());
    let r = match (cmd, a1, a2) {
        ("help", None, _) => {
            o.write(HELP.as_bytes()).await?;
            Ok(())
        }
        ("status", None, _) => {
            status(o, ctx, pending).await?;
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
        ("show", None, _) => {
            show(o, pending).await?;
            Ok(())
        }
        ("set", Some(key), _) => {
            // The value is the rest of the line, blanks included.
            let value = rest[key.len()..].trim();
            match KEYS.iter().find(|k| k.name == key) {
                Some(k) if value.is_empty() && k.hashed() => {
                    o.write(b"enter the new password twice (not shown), Ctrl-C cancels\r\n").await?;
                    return Ok(Input::NewPassword);
                }
                Some(k) if value.is_empty() && k.secret() => {
                    o.write(b"enter the value (not shown), Ctrl-C cancels\r\n").await?;
                    return Ok(Input::Secret(k.name));
                }
                Some(_) if value.is_empty() => Err("value expected; \"unset\" restores the default"),
                _ if CA_KEYS.contains(&key) => Err("certificates are set with \"ca 0\" or \"ca 1\""),
                _ => pending.set(key, value),
            }
        }
        ("set" | "unset", None, _) => Err("key expected, see \"show\""),
        ("unset", Some(key), None) => pending.unset(key),
        ("ca", Some(slot @ ("0" | "1")), None) => {
            o.write(b"paste the PEM certificate, Ctrl-C cancels\r\n").await?;
            return Ok(Input::Pem(usize::from(slot.as_bytes()[0] - b'0'), Pem::new()));
        }
        ("ca", _, _) => Err("slot 0 or 1 expected"),
        ("clear", None, _) => {
            *pending = Settings::new();
            Ok(())
        }
        ("discard", None, _) => {
            pending.clone_from(config::get());
            Ok(())
        }
        ("commit", None, _) => match pending.missing() {
            Some(k) => {
                out!(o, "error: {k} not set\r\n")?;
                return Ok(Input::Command);
            }
            None => match config::save(pending).await {
                Ok(()) => restart(o, false).await,
                Err(e) => Err(e),
            },
        },
        ("erase", None, _) => match config::erase().await {
            Ok(()) => restart(o, false).await,
            Err(e) => Err(e),
        },
        ("radio", None, _) => {
            radio_status(o).await?;
            Ok(())
        }
        ("radio", Some("restart"), None) => {
            let before = radio::status().starts;
            radio::restart();
            o.write(b"restarting, this takes a few seconds\r\n").await?;
            let t = Instant::now();
            // Power cycle, probe and lgw_start on core 1.
            while t.elapsed() < Duration::from_secs(30) {
                Timer::after_millis(100).await;
                let s = radio::status();
                if s.starts != before && s.state != radio::State::Starting {
                    break;
                }
            }
            radio_status(o).await?;
            match radio::status().state {
                radio::State::Running => Ok(()),
                _ => Err("radio not running"),
            }
        }
        ("radio", Some(_), None) => Err("\"restart\" or nothing expected"),
        ("otp-key", None, _) => {
            out!(o, "{}\r\n", config_key::Status)?;
            Ok(())
        }
        ("otp-key", Some("init"), None) => match config_key::can_init() {
            Ok(()) => {
                out!(o, "writes a random key into OTP page {}, then locks the page for good; cannot be undone\r\n", config_key::PAGE)?;
                o.write(b"type \"yes\" to write it, anything else cancels\r\n").await?;
                return Ok(Input::ConfirmOtpKey);
            }
            Err(e) => Err(e),
        },
        ("otp-key", Some(_), None) => Err("\"init\" or nothing expected"),
        ("login", None, _) if login::required() => {
            return Ok(Input::Login);
        }
        ("logout", None, _) if login::required() => {
            *logged_in = false;
            Ok(())
        }
        ("login" | "logout", None, _) => Err("no console password set"),
        ("reboot", None, _) => restart(o, false).await,
        ("bootsel", None, _) => restart(o, true).await,
        (
            "help" | "status" | "log" | "show" | "unset" | "clear" | "discard" | "commit" | "erase" | "otp-key"
            | "radio" | "reboot" | "bootsel" | "login" | "logout",
            _,
            _,
        ) => Err("too many arguments"),
        _ => Err("unknown command, see \"help\""),
    };
    reply(o, r).await?;
    Ok(Input::Command)
}

/// Size and SHA-256 of a certificate (compare: `openssl x509 -outform der | sha256sum`).
async fn ca_info(o: &mut Out<'_>, der: &[u8]) -> Result<(), Gone> {
    if der.is_empty() {
        return o.write(b"-\r\n").await;
    }
    out!(o, "{} bytes, sha256 ", der.len())?;
    for b in Sha256::digest(der) {
        out!(o, "{b:02x}")?;
    }
    o.write(b"\r\n").await
}

async fn show(o: &mut Out<'_>, s: &Settings) -> Result<(), Gone> {
    for k in &KEYS {
        out!(o, "{:<18} ", k.name)?;
        match s.get(k.name) {
            Some(_) if k.secret() => o.write(b"***").await?,
            Some(v) => o.write(v.as_bytes()).await?,
            None if !k.default.is_empty() => out!(o, "{} (default)", k.default)?,
            None => o.write(b"-").await?,
        }
        o.write(b"\r\n").await?;
    }
    for (slot, name) in CA_KEYS.iter().enumerate() {
        out!(o, "{name:<18} ")?;
        ca_info(o, s.ca(slot)).await?;
    }
    if let Some(k) = s.missing() {
        out!(o, "incomplete: {k} not set\r\n")?;
    }
    if s != config::get() {
        o.write(b"changed since the last commit; \"commit\" saves and restarts\r\n").await?;
    }
    Ok(())
}

async fn radio_status(o: &mut Out<'_>) -> Result<(), Gone> {
    let s = radio::status();
    out!(o, "{}\r\n", s.state)?;
    if let Some(p) = s.probe {
        out!(o, "SX1302 {p}\r\n")?;
    }
    if let Some(eui) = s.eui {
        out!(o, "eui {eui:016x}\r\n")?;
    }
    if s.state == radio::State::Running {
        out!(o, "rx {} frames ok, {} with crc errors\r\n", s.rx_ok, s.rx_bad)?;
        out!(o, "tx {} frames sent, {} not sent", s.tx_ok, s.tx_failed)?;
        if s.tx_ok + s.tx_failed > 0 {
            out!(o, ", lgw_send up to {} µs", s.tx_load_max_us)?;
        }
        o.write(b"\r\n").await?;
    }
    Ok(())
}

async fn status(o: &mut Out<'_>, ctx: &Context, pending: &Settings) -> Result<(), Gone> {
    let m = ctx.ids.mac;
    let up = Instant::now().as_secs();
    let cfg = config::get();
    out!(o, "firmware   {}, built {}\r\n", env!("CARGO_PKG_VERSION"), Utc(config::build_time()))?;
    match boot::info() {
        Some(b) => out!(o, "boot       {b}\r\n")?,
        None => o.write(b"boot       unknown\r\n").await?,
    }
    out!(o, "security   {}", boot::security())?;
    if let Some(why) = boot::refuse_online() {
        out!(o, "; not locked ({why}), offline")?;
    }
    o.write(b"\r\n").await?;
    out!(o, "config key {}\r\n", config_key::Status)?;
    if boot::trial() {
        let left = boot::BUY_DEADLINE_S.saturating_sub(up);
        out!(o, "trial      OTA image not bought yet: needs the broker within {left} s, else the old image boots\r\n")?;
    }
    out!(o, "gateway id {}\r\n", ctx.ids.gateway_id)?;
    out!(o, "radio      {}\r\n", radio::status().state)?;
    match cfg.missing() {
        None => o.write(b"config     stored").await?,
        Some(k) => out!(o, "config     provisioning mode ({k} not set)")?,
    }
    if pending != cfg {
        o.write(b", uncommitted changes").await?;
    }
    if config::staged_password().is_some() {
        o.write(b", new mqtt.password staged").await?;
    }
    o.write(b"\r\n").await?;
    match config::ca_seq() {
        0 => o.write(b"ca update  none\r\n").await?,
        u64::MAX => o.write(b"ca update  sequence number unusable, updates refused until \"erase\"\r\n").await?,
        seq => out!(o, "ca update  {seq} ({})\r\n", Utc(seq))?,
    }
    out!(o, "mac        {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\r\n", m[0], m[1], m[2], m[3], m[4], m[5])?;
    match ctx.stack.and_then(|s| s.config_v4()) {
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
    out!(o, "mqtt       {}", mqtt::state())?;
    if ctx.stack.is_some() {
        out!(o, " ({}:{})", cfg.mqtt_host(), cfg.mqtt_port())?;
    }
    o.write(b"\r\n").await?;
    let used = downlink::duty_cycle_used();
    for (b, used) in EU868.iter().zip(used) {
        out!(o, "duty cycle {:>12} MHz {:>6} of {:>6} ms/h\r\n", b.name, used / 1000, b.limit_us() / 1000)?;
    }
    Ok(())
}
