//! Trace and span identifiers and the W3C Trace Context headers.
//!
//! Parsing is strict (lowercase hex, exact lengths, no all-zero identifier,
//! one `traceparent` header) because the headers come from untrusted
//! clients; see [`extract`].

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use http::{HeaderMap, HeaderValue};

/// `traceparent` header name.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// `tracestate` header name.
pub const TRACESTATE_HEADER: &str = "tracestate";

/// `"00-" + 32 + "-" + 16 + "-" + 2`.
const TRACEPARENT_LENGTH: usize = 55;
const MAX_TRACESTATE_LENGTH: usize = 512;
const MAX_TRACESTATE_ITEMS: usize = 32;
const HEX: &[u8; 16] = b"0123456789abcdef";

/// A 16-byte trace identifier; all zeros is invalid.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct TraceId([u8; 16]);

/// An 8-byte span identifier; all zeros is invalid.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct SpanId([u8; 8]);

impl TraceId {
    /// Wraps raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Whether any byte is non-zero.
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.0 != [0; 16]
    }

    /// Parses 32 lowercase hex digits.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let mut bytes = [0; 16];
        decode_hex(&mut bytes, text.as_bytes())?;
        Some(Self(bytes))
    }
}

impl SpanId {
    /// Wraps raw bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 8] {
        self.0
    }

    /// Whether any byte is non-zero.
    #[must_use]
    pub fn is_valid(self) -> bool {
        self.0 != [0; 8]
    }

    /// Parses 16 lowercase hex digits.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let mut bytes = [0; 8];
        decode_hex(&mut bytes, text.as_bytes())?;
        Some(Self(bytes))
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "TraceId({self})")
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(formatter, &self.0)
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "SpanId({self})")
    }
}

fn write_hex(formatter: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    let mut text = [0_u8; 32];
    for (index, byte) in bytes.iter().enumerate() {
        text[2 * index] = HEX[usize::from(byte >> 4)];
        text[2 * index + 1] = HEX[usize::from(byte & 0x0f)];
    }
    // Only ASCII hex digits were written.
    formatter.write_str(std::str::from_utf8(&text[..2 * bytes.len()]).map_err(|_| fmt::Error)?)
}

fn lower_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn decode_hex(target: &mut [u8], text: &[u8]) -> Option<()> {
    if text.len() != 2 * target.len() {
        return None;
    }
    for (index, byte) in target.iter_mut().enumerate() {
        *byte = lower_hex_value(text[2 * index])? << 4 | lower_hex_value(text[2 * index + 1])?;
    }
    Some(())
}

/// The sampled bit of the W3C trace flags.
pub const FLAG_SAMPLED: u8 = 0x01;

/// The identity of a span as it crosses process and task boundaries.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SpanContext {
    /// Trace identifier.
    pub trace_id: TraceId,
    /// Span identifier.
    pub span_id: SpanId,
    /// W3C trace flags; only [`FLAG_SAMPLED`] is kept.
    pub flags: u8,
    /// Validated `tracestate`, forwarded unchanged; empty when absent.
    pub trace_state: String,
    /// Whether the context came from another process.
    pub remote: bool,
}

impl SpanContext {
    /// Whether both identifiers are valid.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.trace_id.is_valid() && self.span_id.is_valid()
    }

    /// Whether the sampled flag is set.
    #[must_use]
    pub fn is_sampled(&self) -> bool {
        self.flags & FLAG_SAMPLED != 0
    }

    /// The `traceparent` value of this context (version `00`).
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!(
            "00-{}-{}-{:02x}",
            self.trace_id,
            self.span_id,
            self.flags & FLAG_SAMPLED
        )
    }

    /// Writes `traceparent` (and `tracestate` when set) into `headers`, to
    /// propagate this context to a downstream service. Only send it to
    /// services that may see your trace identifiers.
    pub fn inject(&self, headers: &mut HeaderMap) {
        if !self.is_valid() {
            return;
        }
        if let Ok(value) = HeaderValue::from_str(&self.traceparent()) {
            headers.insert(TRACEPARENT_HEADER, value);
        }
        headers.remove(TRACESTATE_HEADER);
        if !self.trace_state.is_empty()
            && let Ok(value) = HeaderValue::from_str(&self.trace_state)
        {
            headers.insert(TRACESTATE_HEADER, value);
        }
    }
}

/// Parses a `traceparent` header value.
///
/// Version `00` must have exactly 55 characters; higher versions may carry
/// more fields after a `-`, version `ff` is invalid. Only the sampled flag
/// is kept. Identifiers must be lowercase hex and not all zero.
#[must_use]
pub fn parse_traceparent(value: &str) -> Option<SpanContext> {
    let bytes = value.as_bytes();
    if bytes.len() < TRACEPARENT_LENGTH {
        return None;
    }
    let version = lower_hex_value(bytes[0])? << 4 | lower_hex_value(bytes[1])?;
    if version == 0xff || bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }
    if version == 0 && bytes.len() != TRACEPARENT_LENGTH {
        return None;
    }
    if version > 0 && bytes.len() > TRACEPARENT_LENGTH && bytes[TRACEPARENT_LENGTH] != b'-' {
        return None;
    }
    let trace_id = TraceId::from_hex(value.get(3..35)?)?;
    let span_id = SpanId::from_hex(value.get(36..52)?)?;
    let flags = lower_hex_value(bytes[53])? << 4 | lower_hex_value(bytes[54])?;
    let context = SpanContext {
        trace_id,
        span_id,
        flags: flags & FLAG_SAMPLED,
        trace_state: String::new(),
        remote: false,
    };
    context.is_valid().then_some(context)
}

/// Validates a `tracestate` value: at most 32 `key=value` members, unique
/// keys, the W3C key and value grammars, at most 512 bytes once empty
/// members are removed. Returns the normalized value, or `None`.
#[must_use]
pub fn normalize_tracestate(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    if value.len() > 2 * MAX_TRACESTATE_LENGTH {
        return None;
    }
    let mut members: Vec<&str> = Vec::new();
    for member in value.split(',') {
        let member = member.trim_matches([' ', '\t']);
        if member.is_empty() {
            continue;
        }
        if members.len() == MAX_TRACESTATE_ITEMS {
            return None;
        }
        let (key, val) = member.split_once('=')?;
        if !valid_tracestate_key(key) || !valid_tracestate_value(val) {
            return None;
        }
        if members
            .iter()
            .any(|existing| existing.split_once('=').map(|(k, _)| k) == Some(key))
        {
            return None;
        }
        members.push(member);
    }
    let normalized = members.join(",");
    (normalized.len() <= MAX_TRACESTATE_LENGTH).then_some(normalized)
}

fn valid_tracestate_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    match key.split_once('@') {
        None => {
            (1..=256).contains(&bytes.len())
                && bytes[0].is_ascii_lowercase()
                && bytes[1..].iter().all(|byte| key_byte(*byte))
        }
        Some((tenant, system)) => {
            let tenant = tenant.as_bytes();
            let system = system.as_bytes();
            (1..=241).contains(&tenant.len())
                && (tenant[0].is_ascii_lowercase() || tenant[0].is_ascii_digit())
                && tenant[1..].iter().all(|byte| key_byte(*byte))
                && (1..=14).contains(&system.len())
                && system[0].is_ascii_lowercase()
                && system[1..].iter().all(|byte| key_byte(*byte))
        }
    }
}

fn key_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-' | b'*' | b'/')
}

fn valid_tracestate_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=256).contains(&bytes.len())
        && bytes[bytes.len() - 1] != b' '
        && bytes
            .iter()
            .all(|byte| (0x20..=0x7e).contains(byte) && *byte != b',' && *byte != b'=')
}

/// Reads the incoming trace context: exactly one valid `traceparent` header
/// and, when every `tracestate` line together is valid, the trace state.
/// Returns a remote context, or `None`.
///
/// Callers decide whether to trust it (`FORGE_TELEMETRY_TRUST_TRACEPARENT`).
#[must_use]
pub fn extract(headers: &HeaderMap) -> Option<SpanContext> {
    let mut values = headers.get_all(TRACEPARENT_HEADER).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let mut context = parse_traceparent(first.to_str().ok()?)?;
    let states: Vec<&str> = headers
        .get_all(TRACESTATE_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    if !states.is_empty()
        && let Some(state) = normalize_tracestate(&states.join(","))
    {
        context.trace_state = state;
    }
    context.remote = true;
    Some(context)
}

/// Fills `target` from the operating system's CSPRNG; if that ever fails, a
/// process-local generator seeded from the clock keeps identifiers unique
/// (they are correlation identifiers, not secrets).
pub(crate) fn random_bytes(target: &mut [u8]) {
    if getrandom::fill(target).is_ok() {
        return;
    }
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    for chunk in target.chunks_mut(8) {
        // SplitMix64.
        let mut z = STATE
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed)
            .wrapping_add(seed);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_be_bytes()[..chunk.len()]);
    }
}

/// A new valid trace identifier.
pub(crate) fn new_trace_id() -> TraceId {
    loop {
        let mut bytes = [0; 16];
        random_bytes(&mut bytes);
        let id = TraceId(bytes);
        if id.is_valid() {
            return id;
        }
    }
}

/// A new valid span identifier.
pub(crate) fn new_span_id() -> SpanId {
    loop {
        let mut bytes = [0; 8];
        random_bytes(&mut bytes);
        let id = SpanId(bytes);
        if id.is_valid() {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn traceparent_round_trips() {
        let context = parse_traceparent(VALID).expect("valid");
        assert_eq!(
            context.trace_id.to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(context.span_id.to_string(), "00f067aa0ba902b7");
        assert!(context.is_sampled());
        assert_eq!(context.traceparent(), VALID);
    }

    #[test]
    fn invalid_traceparents_are_refused() {
        for value in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00_4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-zz",
            "0g-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
        ] {
            assert!(parse_traceparent(value).is_none(), "{value}");
        }
        // A future version may append fields.
        assert!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-future")
                .is_some()
        );
        // Only the sampled flag is kept.
        let other_flags =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-ff")
                .expect("valid");
        assert_eq!(other_flags.flags, FLAG_SAMPLED);
    }

    #[test]
    fn tracestate_is_validated() {
        assert_eq!(
            normalize_tracestate("rojo=00f067aa0ba902b7, congo=t61rcWkgMzE,,").as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE")
        );
        assert_eq!(
            normalize_tracestate("tenant@system=1").as_deref(),
            Some("tenant@system=1")
        );
        for bad in [
            "Upper=1",
            "a=1,a=2",
            "a",
            "a=",
            "a=x,b=y=z",
            "a=ctl\u{7f}",
            "1abc=1",
        ] {
            assert!(normalize_tracestate(bad).is_none(), "{bad}");
        }
        let too_many: Vec<String> = (0..33).map(|index| format!("k{index}=v")).collect();
        assert!(normalize_tracestate(&too_many.join(",")).is_none());
        assert!(normalize_tracestate(&format!("a={}", "x".repeat(600))).is_none());
    }

    #[test]
    fn extract_requires_exactly_one_traceparent() {
        let mut headers = HeaderMap::new();
        assert!(extract(&headers).is_none());
        headers.insert(TRACEPARENT_HEADER, HeaderValue::from_static(VALID));
        headers.append(TRACESTATE_HEADER, HeaderValue::from_static("a=1"));
        headers.append(TRACESTATE_HEADER, HeaderValue::from_static("b=2"));
        let context = extract(&headers).expect("one valid header");
        assert!(context.remote);
        assert_eq!(context.trace_state, "a=1,b=2");

        headers.append(TRACEPARENT_HEADER, HeaderValue::from_static(VALID));
        assert!(extract(&headers).is_none(), "repeated traceparent");

        let mut invalid_state = HeaderMap::new();
        invalid_state.insert(TRACEPARENT_HEADER, HeaderValue::from_static(VALID));
        invalid_state.insert(TRACESTATE_HEADER, HeaderValue::from_static("BAD"));
        assert_eq!(
            extract(&invalid_state).expect("parent kept").trace_state,
            ""
        );
    }

    #[test]
    fn inject_writes_both_headers() {
        let mut context = parse_traceparent(VALID).expect("valid");
        context.trace_state = "a=1".to_owned();
        let mut headers = HeaderMap::new();
        headers.insert(TRACESTATE_HEADER, HeaderValue::from_static("stale=1"));
        context.inject(&mut headers);
        assert_eq!(headers[TRACEPARENT_HEADER], VALID);
        assert_eq!(headers[TRACESTATE_HEADER], "a=1");
        context.trace_state.clear();
        context.inject(&mut headers);
        assert!(!headers.contains_key(TRACESTATE_HEADER));
    }

    #[test]
    fn generated_identifiers_are_valid_and_distinct() {
        let first = new_trace_id();
        let second = new_trace_id();
        assert!(first.is_valid() && second.is_valid());
        assert_ne!(first, second);
        assert!(new_span_id().is_valid());
    }

    /// Arbitrary bytes never panic the parsers (a light, deterministic fuzz).
    #[test]
    fn parsers_never_panic_on_arbitrary_input() {
        let mut state = 0x1234_5678_u64;
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let length = (state % 80) as usize;
            let text: String = (0..length)
                .map(|index| {
                    let pick = (state >> (index % 56)) as u8;
                    b"0123456789abcdefABCDEF-=,@ \t\xff"[usize::from(pick) % 29] as char
                })
                .collect();
            let _ = parse_traceparent(&text);
            let _ = normalize_tracestate(&text);
        }
    }
}
