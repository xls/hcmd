//! The HTML entities worth decoding.
//!
//! The five the format requires plus the handful that turn up in real prose. A
//! full table is thousands of entries and would be larger than the renderer it
//! serves; anything not here is left as written, which is legible and honest.

/// The entities worth decoding, and nothing else.
///
/// The five that HTML requires plus the space and the dashes that turn up in
/// real prose. Anything not here is left as written, which is legible.
pub fn entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        "nbsp" | "#160" => " ",
        "mdash" | "#8212" => " - ",
        "ndash" | "#8211" => "-",
        "hellip" | "#8230" => "...",
        "copy" => "(c)",
        _ => return None,
    })
}

/// Decode the entities in one run of text.
pub fn decode(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let after = rest.get(at.saturating_add(1)..).unwrap_or_default();
        match after.find(';').filter(|end| *end <= 8) {
            Some(end) => match after.get(..end).and_then(entity) {
                Some(text) => {
                    out.push_str(text);
                    rest = after.get(end.saturating_add(1)..).unwrap_or_default();
                }
                None => {
                    out.push('&');
                    rest = after;
                }
            },
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}
