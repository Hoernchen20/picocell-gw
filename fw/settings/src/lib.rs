//! Provisioned configuration (sketch.md section 7): the keys, their validation, the
//! stored form and the PEM input for CA certificates. Free of I/O and crypto, so it can
//! be tested on the host; the firmware encrypts the stored form and keeps it in flash.
//!
//! Values are kept as text, `key=value` lines in key table order. Every value is
//! checked when it is set or loaded, so the typed accessors can rely on it.

#![no_std]

use core::net::Ipv4Addr;
use core::str::FromStr;

use heapless::{String, Vec};

/// Room for all values as `key=value` lines.
pub const TEXT_MAX: usize = 1024;
/// Largest CA certificate (DER); a P-256 root is about 500 bytes.
pub const CA_MAX: usize = 1024;
pub const CA_SLOTS: usize = 2;
/// Longest stored form (`encode`).
pub const ENCODED_MAX: usize = 1 + 2 + TEXT_MAX + CA_SLOTS * (2 + CA_MAX);
const FORMAT: u8 = 1;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Mode,
    Cidr,
    Addr,
    Addrs,
    Hosts,
    Names,
    Host,
    Port,
    Prefix,
    Secret,
}

pub struct Key {
    pub name: &'static str,
    kind: Kind,
    /// Used while the key is not set; empty = none.
    pub default: &'static str,
}

impl Key {
    /// Never printed; shown as `***`.
    pub fn secret(&self) -> bool {
        self.kind == Kind::Secret
    }
}

const fn key(name: &'static str, kind: Kind, default: &'static str) -> Key {
    Key { name, kind, default }
}

/// The text keys; the CA certificates are `tls.ca0` and `tls.ca1`.
pub const KEYS: [Key; 10] = [
    key("net.mode", Kind::Mode, "static"),
    key("net.ipv4", Kind::Cidr, ""),
    key("net.gateway", Kind::Addr, ""),
    key("net.dns", Kind::Addrs, ""),
    key("net.hosts", Kind::Hosts, ""),
    key("ntp.servers", Kind::Names, "0.pool.ntp.org,1.pool.ntp.org,2.pool.ntp.org"),
    key("mqtt.host", Kind::Host, ""),
    key("mqtt.port", Kind::Port, "8883"),
    key("mqtt.topic_prefix", Kind::Prefix, "eu868"),
    key("mqtt.password", Kind::Secret, ""),
];

pub const CA_KEYS: [&str; CA_SLOTS] = ["tls.ca0", "tls.ca1"];

fn find(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.name == name)
}

/// Comma-separated list, blanks around entries ignored.
fn list(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|e| !e.is_empty())
}

/// All entries pass `f` and there are `min..=max` of them.
fn entries(s: &str, min: usize, max: usize, f: impl Fn(&str) -> bool) -> bool {
    let mut n = 0;
    for e in list(s) {
        if !f(e) {
            return false;
        }
        n += 1;
    }
    (min..=max).contains(&n)
}

fn hostname(s: &str) -> bool {
    (1..=63).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

fn addr(s: &str) -> Option<Ipv4Addr> {
    Ipv4Addr::from_str(s).ok()
}

fn cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (a, p) = s.split_once('/')?;
    Some((addr(a)?, u8::from_str(p).ok().filter(|p| *p <= 32)?))
}

fn host_entry(e: &str) -> Option<(&str, Ipv4Addr)> {
    let (name, a) = e.split_once('=')?;
    let name = name.trim();
    hostname(name).then_some(())?;
    Some((name, addr(a.trim())?))
}

fn check(kind: Kind, v: &str) -> Result<(), &'static str> {
    let (ok, expected) = match kind {
        Kind::Mode => (matches!(v, "static" | "dhcp"), "static or dhcp expected"),
        Kind::Cidr => (cidr(v).is_some(), "address/prefix expected, e.g. 10.0.0.20/24"),
        Kind::Addr => (addr(v).is_some(), "IPv4 address expected"),
        Kind::Addrs => (
            entries(v, 1, 3, |e| addr(e).is_some()),
            "1 to 3 IPv4 addresses expected, comma-separated",
        ),
        Kind::Hosts => (
            entries(v, 1, 4, |e| host_entry(e).is_some()),
            "1 to 4 name=address entries expected, comma-separated",
        ),
        Kind::Names => (
            entries(v, 1, 3, hostname),
            "1 to 3 host names or IPv4 addresses expected, comma-separated",
        ),
        Kind::Host => (hostname(v), "host name expected (letters, digits, '-' and '.', up to 63)"),
        Kind::Port => (u16::from_str(v).is_ok_and(|p| p > 0), "port 1-65535 expected"),
        Kind::Prefix => (
            (1..=32).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic() && b != b'+' && b != b'#'),
            "1 to 32 characters expected, no '+', '#' or blanks",
        ),
        // No blanks: trimming (console, config files) or a stray newline (remote
        // change) cannot then change a password unnoticed.
        Kind::Secret => (
            (1..=64).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic()),
            "1 to 64 printable characters expected, no blanks",
        ),
    };
    if ok { Ok(()) } else { Err(expected) }
}

/// Checks a new `mqtt.password` as it is, without the trimming of `Settings::set`.
pub fn check_password(v: &str) -> Result<(), &'static str> {
    check(Kind::Secret, v)
}

/// An X.509 certificate is one DER SEQUENCE: checks its header against the length.
pub fn is_der(d: &[u8]) -> bool {
    let (len, header) = match *d {
        [0x30, l, ..] if l < 0x80 => (usize::from(l), 2),
        [0x30, 0x81, l, ..] => (usize::from(l), 3),
        [0x30, 0x82, h, l, ..] => (usize::from(h) << 8 | usize::from(l), 4),
        _ => return false,
    };
    header + len == d.len()
}

pub enum Net {
    Dhcp,
    Static { address: Ipv4Addr, prefix: u8, gateway: Option<Ipv4Addr> },
}

#[derive(Clone, PartialEq)]
pub struct Settings {
    text: String<TEXT_MAX>,
    ca: [Vec<u8, CA_MAX>; CA_SLOTS],
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

impl Settings {
    /// Nothing set.
    pub const fn new() -> Self {
        Self { text: String::new(), ca: [const { Vec::new() }; CA_SLOTS] }
    }

    /// The value of `name` if it is set.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.text.lines().find_map(|l| l.split_once('=').filter(|(k, _)| *k == name).map(|(_, v)| v))
    }

    /// The value, else the default.
    fn value(&self, name: &str) -> &str {
        self.get(name).or_else(|| find(name).map(|k| k.default)).unwrap_or("")
    }

    pub fn set(&mut self, name: &str, value: &str) -> Result<(), &'static str> {
        let k = find(name).ok_or("unknown key")?;
        let value = value.trim();
        check(k.kind, value)?;
        self.replace(k.name, Some(value))
    }

    /// Back to the default (or none); also takes `tls.ca0` / `tls.ca1`.
    pub fn unset(&mut self, name: &str) -> Result<(), &'static str> {
        if let Some(slot) = CA_KEYS.iter().position(|k| *k == name) {
            self.ca[slot].clear();
            return Ok(());
        }
        let k = find(name).ok_or("unknown key")?;
        self.replace(k.name, None)
    }

    /// Rebuilds the text with `name` replaced.
    fn replace(&mut self, name: &str, value: Option<&str>) -> Result<(), &'static str> {
        let mut text = String::<TEXT_MAX>::new();
        for k in &KEYS {
            let v = if k.name == name { value } else { self.get(k.name) };
            if let Some(v) = v {
                text.push_str(k.name)
                    .and(text.push('='))
                    .and(text.push_str(v))
                    .and(text.push('\n'))
                    .map_err(|_| "config too long")?;
            }
        }
        self.text = text;
        Ok(())
    }

    pub fn set_ca(&mut self, slot: usize, der: &[u8]) -> Result<(), &'static str> {
        if !is_der(der) {
            return Err("not a DER certificate");
        }
        self.ca[slot] = Vec::from_slice(der).map_err(|_| "certificate too long")?;
        Ok(())
    }

    /// The certificate in `slot`; empty if none.
    pub fn ca(&self, slot: usize) -> &[u8] {
        &self.ca[slot]
    }

    /// The first required key that is not set, if any.
    pub fn missing(&self) -> Option<&'static str> {
        if self.value("net.mode") == "static" && self.get("net.ipv4").is_none() {
            return Some("net.ipv4");
        }
        for name in ["mqtt.host", "mqtt.password"] {
            if self.get(name).is_none() {
                return Some(name);
            }
        }
        if self.ca[0].is_empty() {
            return Some(CA_KEYS[0]);
        }
        None
    }

    /// `None` in static mode without an address.
    pub fn net(&self) -> Option<Net> {
        if self.value("net.mode") == "dhcp" {
            return Some(Net::Dhcp);
        }
        let (address, prefix) = cidr(self.get("net.ipv4")?)?;
        let gateway = self.get("net.gateway").and_then(addr);
        Some(Net::Static { address, prefix, gateway })
    }

    /// Checked before DNS.
    pub fn hosts(&self) -> Vec<(&str, Ipv4Addr), 4> {
        list(self.value("net.hosts")).filter_map(host_entry).take(4).collect()
    }

    /// Tried before any from DHCP.
    pub fn dns_servers(&self) -> Vec<Ipv4Addr, 3> {
        list(self.value("net.dns")).filter_map(addr).take(3).collect()
    }

    /// IP addresses or host names.
    pub fn ntp_servers(&self) -> Vec<&str, 3> {
        list(self.value("ntp.servers")).take(3).collect()
    }

    /// Host name for SNI and the certificate check; empty if not set.
    pub fn mqtt_host(&self) -> &str {
        self.value("mqtt.host")
    }

    pub fn mqtt_port(&self) -> u16 {
        u16::from_str(self.value("mqtt.port")).unwrap_or(8883)
    }

    pub fn topic_prefix(&self) -> &str {
        self.value("mqtt.topic_prefix")
    }

    pub fn mqtt_password(&self) -> &str {
        self.value("mqtt.password")
    }

    /// The stored form: format byte, then the text and each CA slot with a 16-bit
    /// little-endian length.
    pub fn encode<'a>(&self, out: &'a mut [u8; ENCODED_MAX]) -> &'a [u8] {
        fn put(out: &mut [u8], n: &mut usize, b: &[u8]) {
            out[*n..*n + b.len()].copy_from_slice(b);
            *n += b.len();
        }
        let mut n = 0;
        put(out, &mut n, &[FORMAT]);
        for block in core::iter::once(self.text.as_bytes()).chain(self.ca.iter().map(|c| &c[..])) {
            put(out, &mut n, &(block.len() as u16).to_le_bytes());
            put(out, &mut n, block);
        }
        &out[..n]
    }

    /// Reads the stored form, checking every value.
    pub fn decode(data: &[u8]) -> Result<Self, &'static str> {
        fn block<'a>(r: &mut &'a [u8]) -> Result<&'a [u8], &'static str> {
            let &[l0, l1, ref rest @ ..] = *r else {
                return Err("truncated");
            };
            let len = usize::from(u16::from_le_bytes([l0, l1]));
            if rest.len() < len {
                return Err("truncated");
            }
            let (b, rest) = rest.split_at(len);
            *r = rest;
            Ok(b)
        }
        let [FORMAT, ref rest @ ..] = *data else {
            return Err("unknown format");
        };
        let mut r = rest;
        let text = core::str::from_utf8(block(&mut r)?).map_err(|_| "text not UTF-8")?;
        let mut s = Self::new();
        for line in text.lines() {
            let (k, v) = line.split_once('=').ok_or("malformed line")?;
            s.set(k, v)?;
        }
        for slot in 0..CA_SLOTS {
            let der = block(&mut r)?;
            if !der.is_empty() {
                s.set_ca(slot, der)?;
            }
        }
        if !r.is_empty() {
            return Err("trailing data");
        }
        Ok(s)
    }
}

/// Collects a PEM certificate pasted line by line.
pub struct Pem {
    der: Vec<u8, CA_MAX>,
    started: bool,
    padded: bool,
    acc: u32,
    bits: u32,
}

impl Default for Pem {
    fn default() -> Self {
        Self::new()
    }
}

impl Pem {
    pub const fn new() -> Self {
        Self { der: Vec::new(), started: false, padded: false, acc: 0, bits: 0 }
    }

    /// Takes one line; `Ok(true)` after the END line, with the certificate in `der()`.
    pub fn line(&mut self, line: &str) -> Result<bool, &'static str> {
        let line = line.trim();
        if !self.started {
            return match line {
                "" => Ok(false),
                "-----BEGIN CERTIFICATE-----" => {
                    self.started = true;
                    Ok(false)
                }
                _ => Err("-----BEGIN CERTIFICATE----- expected"),
            };
        }
        if line == "-----END CERTIFICATE-----" {
            return if is_der(&self.der) { Ok(true) } else { Err("not a DER certificate") };
        }
        for b in line.bytes() {
            let v = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => {
                    self.padded = true;
                    continue;
                }
                _ => return Err("invalid base64"),
            };
            if self.padded {
                return Err("invalid base64");
            }
            self.acc = self.acc << 6 | u32::from(v);
            self.bits += 6;
            if self.bits >= 8 {
                self.bits -= 8;
                self.der.push((self.acc >> self.bits) as u8).map_err(|_| "certificate too long")?;
            }
        }
        Ok(false)
    }

    pub fn der(&self) -> &[u8] {
        &self.der
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEM: &str = include_str!("../test-ca.pem");
    const DER: &[u8] = include_bytes!("../test-ca.der");

    fn provisioned() -> Settings {
        let mut s = Settings::new();
        s.set("net.ipv4", " 10.0.0.20/24 ").unwrap();
        s.set("mqtt.host", "mqtt.example.org").unwrap();
        s.set("mqtt.password", r#"p@ss"%{}=\w'rd"#).unwrap();
        s.set_ca(0, DER).unwrap();
        s
    }

    #[test]
    fn defaults_and_order() {
        let mut s = Settings::new();
        assert_eq!(s.get("mqtt.port"), None);
        assert_eq!(s.mqtt_port(), 8883);
        assert_eq!(s.topic_prefix(), "eu868");
        assert_eq!(&s.ntp_servers()[..], ["0.pool.ntp.org", "1.pool.ntp.org", "2.pool.ntp.org"]);
        s.set("mqtt.port", "1883").unwrap();
        s.set("net.mode", "dhcp").unwrap();
        assert_eq!(s.text.as_str(), "net.mode=dhcp\nmqtt.port=1883\n");
        s.set("mqtt.port", "8884").unwrap();
        s.unset("net.mode").unwrap();
        assert_eq!(s.text.as_str(), "mqtt.port=8884\n");
        assert_eq!(s.mqtt_port(), 8884);
    }

    #[test]
    fn validation() {
        let mut s = Settings::new();
        assert_eq!(s.set("nope", "1"), Err("unknown key"));
        assert!(s.set("net.mode", "auto").is_err());
        assert!(s.set("net.ipv4", "10.0.0.20").is_err());
        assert!(s.set("net.ipv4", "10.0.0.20/33").is_err());
        assert!(s.set("net.dns", "1.1.1.1,8.8.8.8,9.9.9.9,1.0.0.1").is_err());
        assert!(s.set("net.hosts", "mqtt=10.0.0.5,bad name=10.0.0.6").is_err());
        assert!(s.set("mqtt.port", "0").is_err());
        assert!(s.set("mqtt.topic_prefix", "eu/#").is_err());
        assert!(s.set("mqtt.password", "").is_err());
        assert!(s.set("mqtt.password", "pass word").is_err());
        assert!(s.set("mqtt.password", "pässword").is_err());
        assert!(check_password(r#"p@ss"%{}=\w'rd"#).is_ok());
        assert!(check_password("password\n").is_err());
        assert!(check_password(" password").is_err());
        assert!(check_password(&"x".repeat_into::<65>()).is_err());
        assert!(s.set("mqtt.host", "a_b").is_err());
        assert_eq!(s.text.as_str(), "");
        let long = "x".repeat_into::<64>();
        assert!(s.set("mqtt.host", &long).is_err());
    }

    trait RepeatInto {
        fn repeat_into<const N: usize>(&self) -> String<N>;
    }

    impl RepeatInto for &str {
        fn repeat_into<const N: usize>(&self) -> String<N> {
            let mut s = String::new();
            while s.push_str(self).is_ok() {}
            s
        }
    }

    #[test]
    fn typed_values() {
        let mut s = provisioned();
        s.set("net.gateway", "10.0.0.1").unwrap();
        s.set("net.hosts", "mqtt.example.org = 10.0.0.5, ntp=10.0.0.1").unwrap();
        s.set("net.dns", "10.0.0.1").unwrap();
        let Some(Net::Static { address, prefix, gateway }) = s.net() else { panic!() };
        assert_eq!((address, prefix, gateway), (Ipv4Addr::new(10, 0, 0, 20), 24, Some(Ipv4Addr::new(10, 0, 0, 1))));
        assert_eq!(&s.hosts()[..], [("mqtt.example.org", Ipv4Addr::new(10, 0, 0, 5)), ("ntp", Ipv4Addr::new(10, 0, 0, 1))]);
        assert_eq!(&s.dns_servers()[..], [Ipv4Addr::new(10, 0, 0, 1)]);
        assert_eq!(s.mqtt_password(), r#"p@ss"%{}=\w'rd"#);
        s.set("net.mode", "dhcp").unwrap();
        assert!(matches!(s.net(), Some(Net::Dhcp)));
    }

    #[test]
    fn missing() {
        let mut s = provisioned();
        assert_eq!(s.missing(), None);
        s.unset("tls.ca0").unwrap();
        assert_eq!(s.missing(), Some("tls.ca0"));
        s.unset("net.ipv4").unwrap();
        assert_eq!(s.missing(), Some("net.ipv4"));
        s.set("net.mode", "dhcp").unwrap();
        assert_eq!(s.missing(), Some("tls.ca0"));
        assert_eq!(Settings::new().missing(), Some("net.ipv4"));
    }

    #[test]
    fn text_limit() {
        let mut s = Settings::new();
        let host = "h".repeat_into::<63>();
        let mut v = String::<512>::new();
        for i in 0..4 {
            if i > 0 {
                v.push(',').unwrap();
            }
            v.push_str(&host).unwrap();
            v.push_str("=10.0.0.1").unwrap();
        }
        s.set("net.hosts", &v).unwrap();
        let mut ntp = String::<256>::new();
        for i in 0..3 {
            if i > 0 {
                ntp.push(',').unwrap();
            }
            ntp.push_str(&host).unwrap();
        }
        s.set("ntp.servers", &ntp).unwrap();
        s.set("mqtt.host", &host).unwrap();
        s.set("mqtt.password", &"p".repeat_into::<64>()).unwrap();
        s.set("net.ipv4", "255.255.255.255/32").unwrap();
        s.set("net.gateway", "255.255.255.255").unwrap();
        s.set("net.dns", "255.255.255.255,255.255.255.255,255.255.255.255").unwrap();
        s.set("mqtt.topic_prefix", &"t".repeat_into::<32>()).unwrap();
        // Every key at its longest still fits.
        assert!(s.text.len() < TEXT_MAX);
    }

    #[test]
    fn encode_decode() {
        let mut s = provisioned();
        s.set_ca(1, DER).unwrap();
        let mut buf = [0; ENCODED_MAX];
        let enc = s.encode(&mut buf);
        assert_eq!(enc.len(), 1 + 2 + s.text.len() + 2 * (2 + DER.len()));
        let d = Settings::decode(enc).unwrap();
        assert!(d == s);
        assert_eq!(d.ca(1), DER);
        let n = enc.len();
        assert_eq!(Settings::decode(&buf[..n - 1]).err(), Some("truncated"));
        buf[0] = 9;
        assert_eq!(Settings::decode(&buf[..n]).err(), Some("unknown format"));
        let empty = Settings::new();
        let enc = empty.encode(&mut buf);
        assert_eq!(enc, [FORMAT, 0, 0, 0, 0, 0, 0]);
        assert!(Settings::decode(enc).unwrap() == empty);
        // A value that does not pass the checks.
        let mut bad = Vec::<u8, 32>::new();
        let text = b"mqtt.port=0\n";
        bad.extend_from_slice(&[FORMAT, text.len() as u8, 0]).unwrap();
        bad.extend_from_slice(text).unwrap();
        bad.extend_from_slice(&[0, 0, 0, 0]).unwrap();
        assert_eq!(Settings::decode(&bad).err(), Some("port 1-65535 expected"));
    }

    #[test]
    fn pem() {
        let mut p = Pem::new();
        let mut done = false;
        for line in PEM.lines() {
            done = p.line(line).unwrap();
        }
        assert!(done);
        assert_eq!(p.der(), DER);
        assert!(is_der(DER));
        assert!(!is_der(&DER[..DER.len() - 1]));

        let mut p = Pem::new();
        assert!(p.line("hello").is_err());
        let mut p = Pem::new();
        p.line("-----BEGIN CERTIFICATE-----").unwrap();
        p.line("MIIB").unwrap();
        assert_eq!(p.line("-----END CERTIFICATE-----"), Err("not a DER certificate"));
        assert_eq!(p.line("a=b"), Err("invalid base64"));
        assert_eq!(p.line("a$"), Err("invalid base64"));
    }
}
