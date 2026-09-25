use http::HeaderMap;
use std::collections::BTreeSet;

pub(super) struct Context {
    pub trace: [u8; 16],
    pub parent: [u8; 8],
    pub sampled: bool,
    pub header: String,
    pub state: String,
}

pub(super) fn extract(headers: &HeaderMap) -> Option<Context> {
    let mut values = headers.get_all("traceparent").iter();
    let value = values.next()?.to_str().ok()?.trim_matches([' ', '\t']);
    if values.next().is_some() || !(55..=1024).contains(&value.len()) {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }
    let version = decode::<1>(&bytes[..2])?[0];
    if version == 255
        || (version == 0 && bytes.len() != 55)
        || (bytes.len() > 55 && bytes[55] != b'-')
    {
        return None;
    }
    let trace = decode(&bytes[3..35])?;
    let parent = decode(&bytes[36..52])?;
    let flags = decode::<1>(&bytes[53..55])?[0];
    if trace == [0; 16] || parent == [0; 8] {
        return None;
    }
    Some(Context {
        trace,
        parent,
        sampled: flags & 1 == 1,
        header: value.into(),
        state: tracestate(headers).unwrap_or_default(),
    })
}

fn decode<const N: usize>(value: &[u8]) -> Option<[u8; N]> {
    fn digit(value: u8) -> Option<u8> {
        match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            _ => None,
        }
    }
    let mut output = [0; N];
    for (i, byte) in output.iter_mut().enumerate() {
        *byte = digit(value[i * 2])? * 16 + digit(value[i * 2 + 1])?;
    }
    Some(output)
}

fn tracestate(headers: &HeaderMap) -> Option<String> {
    let mut keys = BTreeSet::new();
    let mut entries = Vec::new();
    for header in headers.get_all("tracestate") {
        for entry in header.to_str().ok()?.split(',') {
            let entry = entry.trim_matches([' ', '\t']);
            if entry.is_empty() {
                continue;
            }
            let (key, value) = entry.split_once('=')?;
            if !valid_key(key)
                || value.is_empty()
                || value.len() > 256
                || !value
                    .bytes()
                    .all(|b| (b' '..=b'~').contains(&b) && b != b'=')
                || !keys.insert(key)
                || keys.len() > 32
            {
                return None;
            }
            entries.push(entry);
        }
    }
    // Bound propagated state without cutting a vendor's opaque value in half.
    // W3C recommends removing oversized entries before dropping the oldest tail.
    let length = |items: &[&str]| {
        items.iter().map(|s| s.len()).sum::<usize>() + items.len().saturating_sub(1)
    };
    if length(&entries) > 512 {
        entries.retain(|e| e.len() <= 128);
    }
    while length(&entries) > 512 {
        entries.pop();
    }
    Some(entries.join(","))
}

fn valid_key(key: &str) -> bool {
    let valid = |part: &str| {
        part.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-*/".contains(&b))
    };
    let lower = |part: &str| part.as_bytes().first().is_some_and(u8::is_ascii_lowercase);
    if let Some((tenant, system)) = key.split_once('@') {
        (1..=241).contains(&tenant.len())
            && (1..=14).contains(&system.len())
            && tenant.as_bytes()[0].is_ascii_alphanumeric()
            && lower(system)
            && valid(tenant)
            && valid(system)
    } else {
        (1..=256).contains(&key.len()) && lower(key) && valid(key)
    }
}
