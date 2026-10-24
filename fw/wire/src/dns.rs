//! Minimal DNS client messages (RFC 1035): one A query, recursion desired.

pub const PORT: u16 = 53;

const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a valid host name (empty label, label > 63 or name > 253 characters).
    InvalidName,
    /// The buffer is too small for the query.
    BufferTooSmall,
    /// Not a well-formed DNS message.
    Malformed,
    /// Not the answer to our query (ID, question or opcode differ); ignore it.
    Mismatch,
    /// Truncated answer (TC); not retried over TCP.
    Truncated,
    /// The name does not exist (NXDOMAIN).
    NotFound,
    /// Another error code from the server.
    Rcode(u8),
    /// No A record in the answer.
    NoAddress,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::InvalidName => f.write_str("invalid host name"),
            Error::BufferTooSmall => f.write_str("buffer too small"),
            Error::Malformed => f.write_str("malformed answer"),
            Error::Mismatch => f.write_str("answer does not match the query"),
            Error::Truncated => f.write_str("truncated answer"),
            Error::NotFound => f.write_str("name not found"),
            Error::Rcode(c) => write!(f, "server error {c}"),
            Error::NoAddress => f.write_str("no address in answer"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    pub addr: [u8; 4],
    /// Seconds the answer may be cached.
    pub ttl: u32,
}

/// Labels of a host name; a trailing dot is ignored.
fn labels(name: &str) -> Result<core::str::Split<'_, char>, Error> {
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > 253 || name.split('.').any(|l| l.is_empty() || l.len() > 63) {
        return Err(Error::InvalidName);
    }
    Ok(name.split('.'))
}

/// Writes an A query for `name` into `buf` and returns its length.
pub fn build_query(id: u16, name: &str, buf: &mut [u8]) -> Result<usize, Error> {
    let labels = labels(name)?;
    let mut w = Writer { buf, pos: 0 };
    w.u16(id)?;
    w.u16(FLAG_RD)?;
    w.u16(1)?; // QDCOUNT
    w.u16(0)?;
    w.u16(0)?;
    w.u16(0)?;
    for label in labels {
        w.u8(label.len() as u8)?;
        w.bytes(label.as_bytes())?;
    }
    w.u8(0)?;
    w.u16(TYPE_A)?;
    w.u16(CLASS_IN)?;
    Ok(w.pos)
}

/// Parses the answer to the query `build_query(id, name)` and returns the first A
/// record. CNAME records before it are skipped (the recursive server resolves them).
pub fn parse_response(id: u16, name: &str, msg: &[u8]) -> Result<Answer, Error> {
    let mut r = Reader { msg, pos: 0 };
    if r.u16()? != id {
        return Err(Error::Mismatch);
    }
    let flags = r.u16()?;
    // Response, opcode QUERY.
    if flags & FLAG_QR == 0 || (flags >> 11) & 0xf != 0 {
        return Err(Error::Mismatch);
    }
    let qdcount = r.u16()?;
    let ancount = r.u16()?;
    r.skip(4)?; // NSCOUNT, ARCOUNT
    if qdcount != 1 {
        return Err(Error::Mismatch);
    }
    // The question must be ours. It is the first name in the message, so a compression
    // pointer here could only point forward; treat it as a mismatch.
    for label in labels(name)? {
        let len = usize::from(r.u8()?);
        if len != label.len() || !r.take(len)?.eq_ignore_ascii_case(label.as_bytes()) {
            return Err(Error::Mismatch);
        }
    }
    if r.u8()? != 0 || r.u16()? != TYPE_A || r.u16()? != CLASS_IN {
        return Err(Error::Mismatch);
    }
    if flags & FLAG_TC != 0 {
        return Err(Error::Truncated);
    }
    match (flags & 0xf) as u8 {
        0 => {}
        3 => return Err(Error::NotFound),
        c => return Err(Error::Rcode(c)),
    }

    for _ in 0..ancount {
        r.skip_name()?;
        let rtype = r.u16()?;
        let class = r.u16()?;
        let ttl = r.u32()?;
        let rdlength = usize::from(r.u16()?);
        let rdata = r.take(rdlength)?;
        if (rtype, class) == (TYPE_A, CLASS_IN) {
            let addr = rdata.try_into().map_err(|_| Error::Malformed)?;
            // RFC 2181: a TTL with the top bit set is treated as zero.
            let ttl = if ttl & 0x8000_0000 != 0 { 0 } else { ttl };
            return Ok(Answer { addr, ttl });
        }
    }
    Err(Error::NoAddress)
}

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, b: &[u8]) -> Result<(), Error> {
        let dst = self.buf.get_mut(self.pos..self.pos + b.len()).ok_or(Error::BufferTooSmall)?;
        dst.copy_from_slice(b);
        self.pos += b.len();
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), Error> {
        self.bytes(&[v])
    }

    fn u16(&mut self, v: u16) -> Result<(), Error> {
        self.bytes(&v.to_be_bytes())
    }
}

struct Reader<'a> {
    msg: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let b = self.msg.get(self.pos..self.pos + n).ok_or(Error::Malformed)?;
        self.pos += n;
        Ok(b)
    }

    fn skip(&mut self, n: usize) -> Result<(), Error> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    /// Skips a name; a compression pointer ends it (not followed, so no loops).
    fn skip_name(&mut self) -> Result<(), Error> {
        loop {
            let len = self.u8()?;
            match len & 0xc0 {
                0x00 if len == 0 => return Ok(()),
                0x00 => self.skip(usize::from(len))?,
                0xc0 => return self.skip(1),
                _ => return Err(Error::Malformed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &[u8] = &[
        0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, //
        4, b'm', b'q', b't', b't', 7, b'e', b'x', b'a', b'm', b'p', b'l', b'e', 3, b'o', b'r',
        b'g', 0, 0, 1, 0, 1,
    ];

    /// The query turned into a response with the given flags and answer records.
    fn response(flags: u16, answers: &[&[u8]]) -> ([u8; 512], usize) {
        let mut m = [0u8; 512];
        m[..QUERY.len()].copy_from_slice(QUERY);
        m[2..4].copy_from_slice(&flags.to_be_bytes());
        m[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
        let mut n = QUERY.len();
        for a in answers {
            m[n..n + a.len()].copy_from_slice(a);
            n += a.len();
        }
        (m, n)
    }

    // Name = pointer to the question name at offset 12.
    const A_10_0_0_5: &[u8] = &[0xc0, 12, 0, 1, 0, 1, 0, 0, 0x0e, 0x10, 0, 4, 10, 0, 0, 5];
    const CNAME: &[u8] = &[
        0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 6, 3, b'f', b'o', b'o', 0xc0, 17,
    ];

    #[test]
    fn query() {
        let mut buf = [0u8; 512];
        let n = build_query(0x1234, "mqtt.example.org", &mut buf).unwrap();
        assert_eq!(&buf[..n], QUERY);
        let n = build_query(0x1234, "mqtt.example.org.", &mut buf).unwrap();
        assert_eq!(&buf[..n], QUERY);
        assert_eq!(build_query(1, "a..b", &mut buf), Err(Error::InvalidName));
        assert_eq!(build_query(1, "", &mut buf), Err(Error::InvalidName));
        assert_eq!(build_query(1, ".", &mut buf), Err(Error::InvalidName));
        let long = [b'a'; 64];
        let long = core::str::from_utf8(&long).unwrap();
        assert_eq!(build_query(1, long, &mut buf), Err(Error::InvalidName));
        assert_eq!(build_query(1, "mqtt.example.org", &mut buf[..20]), Err(Error::BufferTooSmall));
    }

    #[test]
    fn answer() {
        let (m, n) = response(0x8180, &[A_10_0_0_5]);
        let a = parse_response(0x1234, "mqtt.example.org", &m[..n]).unwrap();
        assert_eq!(a, Answer { addr: [10, 0, 0, 5], ttl: 3600 });
        // Case-insensitive question match, trailing dot.
        assert!(parse_response(0x1234, "MQTT.Example.ORG.", &m[..n]).is_ok());
    }

    #[test]
    fn cname_then_a() {
        let (m, n) = response(0x8180, &[CNAME, A_10_0_0_5]);
        let a = parse_response(0x1234, "mqtt.example.org", &m[..n]).unwrap();
        assert_eq!(a.addr, [10, 0, 0, 5]);
        let (m, n) = response(0x8180, &[CNAME]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::NoAddress));
    }

    #[test]
    fn mismatch() {
        let (m, n) = response(0x8180, &[A_10_0_0_5]);
        assert_eq!(parse_response(0x1235, "mqtt.example.org", &m[..n]), Err(Error::Mismatch));
        assert_eq!(parse_response(0x1234, "mqtt.example.com", &m[..n]), Err(Error::Mismatch));
        assert_eq!(parse_response(0x1234, "example.org", &m[..n]), Err(Error::Mismatch));
        // Our own query (QR clear) echoed back.
        assert_eq!(parse_response(0x1234, "mqtt.example.org", QUERY), Err(Error::Mismatch));
    }

    #[test]
    fn errors() {
        let (m, n) = response(0x8183, &[]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::NotFound));
        let (m, n) = response(0x8182, &[]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::Rcode(2)));
        let (m, n) = response(0x8380, &[]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::Truncated));
    }

    #[test]
    fn malformed() {
        let (m, n) = response(0x8180, &[A_10_0_0_5]);
        // Every truncation fails cleanly.
        for len in 0..n {
            assert!(parse_response(0x1234, "mqtt.example.org", &m[..len]).is_err(), "len {len}");
        }
        // A record with 5 bytes of data.
        let bad = [0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 1, 0, 5, 10, 0, 0, 5, 0];
        let (m, n) = response(0x8180, &[&bad]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::Malformed));
        // Label type 0x40 (reserved).
        let bad = [0x40, 0, 1, 0, 1, 0, 0, 0, 1, 0, 4, 10, 0, 0, 5];
        let (m, n) = response(0x8180, &[&bad]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]), Err(Error::Malformed));
    }

    #[test]
    fn negative_ttl() {
        let mut a = [0u8; 16];
        a.copy_from_slice(A_10_0_0_5);
        a[6] = 0x80;
        let (m, n) = response(0x8180, &[&a]);
        assert_eq!(parse_response(0x1234, "mqtt.example.org", &m[..n]).unwrap().ttl, 0);
    }
}
