//! The two scalar formats `config.toml` uses that TOML has no native type for:
//! `"256MiB"` and `"10s"`.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A size in bytes, written as `"8MiB"`, `"1MiB"`, `"500MiB"` - or as a plain
/// integer number of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteSize(pub u64);

impl ByteSize {
    /// `n` mebibytes.
    pub const fn mib(n: u64) -> Self {
        Self(n * 1024 * 1024)
    }

    /// The value in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Parse `"256MiB"`, `"1 GB"`, `"4096"`. Case-insensitive; both the
    /// binary (`KiB`) and decimal (`kB`) suffixes are accepted, and a bare `K`
    /// is read as binary because that is what everyone means.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let split = text
            .find(|c: char| !c.is_ascii_digit() && c != '_')
            .unwrap_or(text.len());
        let (digits, suffix) = text.split_at(split);
        let digits = digits.replace('_', "");
        if digits.is_empty() {
            return Err(format!("{text:?} does not start with a number"));
        }
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("{digits:?} is not a whole number"))?;
        let mult = match suffix.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1_u64,
            "k" | "kb" | "kib" => 1024,
            "m" | "mb" | "mib" => 1024 * 1024,
            "g" | "gb" | "gib" => 1024 * 1024 * 1024,
            "t" | "tb" | "tib" => 1024_u64.pow(4),
            other => return Err(format!("unknown size suffix {other:?}")),
        };
        n.checked_mul(mult)
            .map(Self)
            .ok_or_else(|| format!("{text:?} overflows a 64-bit byte count"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(&str, u64); 4] = [
            ("TiB", 1024_u64.pow(4)),
            ("GiB", 1024 * 1024 * 1024),
            ("MiB", 1024 * 1024),
            ("KiB", 1024),
        ];
        for (name, size) in UNITS {
            if self.0 >= size && self.0.is_multiple_of(size) {
                return write!(f, "{}{name}", self.0 / size);
            }
        }
        write!(f, "{}", self.0)
    }
}

/// A duration, written as `"10s"`, `"500ms"`, `"2m"` - or as a plain integer
/// number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Timeout(pub Duration);

impl Timeout {
    /// The wrapped duration.
    pub const fn duration(self) -> Duration {
        self.0
    }

    /// Parse `"10s"`, `"500ms"`, `"2m"`, `"1h"`, `"30"`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let split = text
            .find(|c: char| !c.is_ascii_digit() && c != '_')
            .unwrap_or(text.len());
        let (digits, suffix) = text.split_at(split);
        let digits = digits.replace('_', "");
        if digits.is_empty() {
            return Err(format!("{text:?} does not start with a number"));
        }
        let n: u64 = digits
            .parse()
            .map_err(|_| format!("{digits:?} is not a whole number"))?;
        let d = match suffix.trim().to_ascii_lowercase().as_str() {
            "ms" => Duration::from_millis(n),
            "" | "s" | "sec" | "secs" => Duration::from_secs(n),
            "m" | "min" | "mins" => Duration::from_secs(n.saturating_mul(60)),
            "h" | "hr" | "hrs" => Duration::from_secs(n.saturating_mul(3600)),
            other => return Err(format!("unknown duration suffix {other:?}")),
        };
        Ok(Self(d))
    }
}

impl fmt::Display for Timeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.0.as_millis();
        if ms.is_multiple_of(1000) {
            write!(f, "{}s", ms / 1000)
        } else {
            write!(f, "{ms}ms")
        }
    }
}

/// Deserialize either a string with a suffix or a bare integer.
macro_rules! scalar_de {
    ($ty:ty, $visitor:ident, $expect:literal, $from_int:expr) => {
        struct $visitor;

        impl<'de> Visitor<'de> for $visitor {
            type Value = $ty;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str($expect)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                <$ty>::parse(v).map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok($from_int(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v < 0 {
                    return Err(E::custom(format!("{v} is negative")));
                }
                Ok($from_int(v.unsigned_abs()))
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                d.deserialize_any($visitor)
            }
        }

        impl Serialize for $ty {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }
    };
}

scalar_de!(
    ByteSize,
    ByteSizeVisitor,
    "a byte size such as \"8MiB\"",
    |v| { ByteSize(v) }
);
scalar_de!(Timeout, TimeoutVisitor, "a duration such as \"10s\"", |v| {
    Timeout(Duration::from_secs(v))
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes_from_the_example_config() {
        assert_eq!(ByteSize::parse("1MiB"), Ok(ByteSize::mib(1)));
        assert_eq!(ByteSize::parse("8MiB"), Ok(ByteSize::mib(8)));
        assert_eq!(ByteSize::parse("256MiB"), Ok(ByteSize::mib(256)));
        assert_eq!(ByteSize::parse("500MiB"), Ok(ByteSize::mib(500)));
        assert_eq!(ByteSize::parse("32MiB"), Ok(ByteSize::mib(32)));
        assert_eq!(ByteSize::parse("4096"), Ok(ByteSize(4096)));
        assert!(ByteSize::parse("MiB").is_err());
        assert!(ByteSize::parse("8 furlongs").is_err());
    }

    #[test]
    fn byte_sizes_round_trip_through_display() {
        assert_eq!(ByteSize::mib(256).to_string(), "256MiB");
        assert_eq!(ByteSize(4096).to_string(), "4KiB");
        assert_eq!(ByteSize(1).to_string(), "1");
    }

    #[test]
    fn durations_from_the_example_config() {
        assert_eq!(Timeout::parse("10s"), Ok(Timeout(Duration::from_secs(10))));
        assert_eq!(Timeout::parse("30s"), Ok(Timeout(Duration::from_secs(30))));
        assert_eq!(
            Timeout::parse("500ms"),
            Ok(Timeout(Duration::from_millis(500)))
        );
        assert!(Timeout::parse("soon").is_err());
    }
}
