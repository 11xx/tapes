//! Byte counts as people write them: `1048576`, `64KiB`, `8m`, `1.5GB`.
//!
//! A unit is binary unless it says otherwise: `k`, `m`, `g`, and `t`, alone or
//! spelled `Ki`/`KiB`, multiply by powers of 1024, while `KB`, `MB`, `GB`, and
//! `TB` multiply by powers of 1000. Units match without regard to case, and a
//! size is always bytes, never bits.

use std::fmt;
use std::str::FromStr;

/// A byte count parsed from a plain integer or a number with a unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(u64);

const BINARY_UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
const EXAMPLES: &str = "use a byte count or a size such as 64KiB, 8m, or 1GB";

impl ByteSize {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The count as a `usize`, saturating where `usize` is narrower.
    pub fn get_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

/// An exact binary multiple prints in its largest whole unit, so a default
/// reads `512MiB`; any other count prints as plain bytes.
impl fmt::Display for ByteSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut value = self.0;
        let mut unit = None;
        for name in BINARY_UNITS {
            if value == 0 || !value.is_multiple_of(1024) {
                break;
            }
            value /= 1024;
            unit = Some(name);
        }
        match unit {
            Some(unit) => write!(formatter, "{value}{unit}"),
            None => write!(formatter, "{}", self.0),
        }
    }
}

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        let number_end = text
            .find(|character: char| !(character.is_ascii_digit() || character == '.'))
            .unwrap_or(text.len());
        let (number, unit) = text.split_at(number_end);
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        if (whole.is_empty() && fraction.is_empty()) || fraction.contains('.') {
            return Err(format!("{text:?} is not a size; {EXAMPLES}"));
        }
        let unit = unit.trim_start();
        let multiplier =
            multiplier(unit).ok_or_else(|| format!("unknown size unit {unit:?}; {EXAMPLES}"))?;

        let whole = if whole.is_empty() {
            0
        } else {
            whole.parse::<u128>().map_err(|_| too_large(text))?
        };
        let mut bytes = whole
            .checked_mul(multiplier)
            .ok_or_else(|| too_large(text))?;
        if !fraction.is_empty() {
            let scale = u32::try_from(fraction.len())
                .ok()
                .and_then(|digits| 10u128.checked_pow(digits))
                .ok_or_else(|| too_large(text))?;
            let scaled = fraction
                .parse::<u128>()
                .map_err(|_| too_large(text))?
                .checked_mul(multiplier)
                .ok_or_else(|| too_large(text))?;
            if scaled % scale != 0 {
                return Err(format!("{text:?} is not a whole number of bytes"));
            }
            bytes = bytes
                .checked_add(scaled / scale)
                .ok_or_else(|| too_large(text))?;
        }
        u64::try_from(bytes).map(Self).map_err(|_| too_large(text))
    }
}

fn multiplier(unit: &str) -> Option<u128> {
    let unit = unit.to_ascii_lowercase();
    if matches!(unit.as_str(), "" | "b") {
        return Some(1);
    }
    let exponent = match unit.chars().next()? {
        'k' => 1,
        'm' => 2,
        'g' => 3,
        't' => 4,
        _ => return None,
    };
    let base: u128 = match &unit[1..] {
        "" | "i" | "ib" => 1024,
        "b" => 1000,
        _ => return None,
    };
    Some(base.pow(exponent))
}

fn too_large(text: &str) -> String {
    format!("{text:?} is more bytes than a 64-bit count holds")
}

#[cfg(test)]
mod tests {
    use super::ByteSize;

    fn parse(text: &str) -> Result<u64, String> {
        text.parse::<ByteSize>().map(ByteSize::get)
    }

    #[test]
    fn a_bare_or_binary_unit_counts_in_powers_of_1024() {
        for (text, bytes) in [
            ("0", 0),
            ("1048576", 1_048_576),
            ("12b", 12),
            ("12B", 12),
            ("64k", 65_536),
            ("64K", 65_536),
            ("64Ki", 65_536),
            ("64KiB", 65_536),
            ("64kib", 65_536),
            ("8m", 8 << 20),
            ("8MiB", 8 << 20),
            ("1g", 1 << 30),
            ("1t", 1 << 40),
            ("512 MiB", 512 << 20),
            (" 4m ", 4 << 20),
        ] {
            assert_eq!(parse(text), Ok(bytes), "{text}");
        }
    }

    #[test]
    fn a_b_unit_counts_in_powers_of_1000() {
        for (text, bytes) in [
            ("64KB", 64_000),
            ("64kb", 64_000),
            ("10mb", 10_000_000),
            ("10MB", 10_000_000),
            ("1GB", 1_000_000_000),
            ("2TB", 2_000_000_000_000),
        ] {
            assert_eq!(parse(text), Ok(bytes), "{text}");
        }
    }

    #[test]
    fn a_fraction_must_come_to_whole_bytes() {
        for (text, bytes) in [
            ("1.5k", 1_536),
            ("1.5KB", 1_500),
            (".5m", 524_288),
            ("2.0", 2),
        ] {
            assert_eq!(parse(text), Ok(bytes), "{text}");
        }
        for text in ["0.1k", "1.5"] {
            let error = parse(text).unwrap_err();
            assert!(
                error.contains("not a whole number of bytes"),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn malformed_and_oversized_sizes_are_refused() {
        for text in ["", "k", "-1", "1.2.3", "10 furlongs", "10mbit", "10bytes"] {
            assert!(parse(text).is_err(), "{text}");
        }
        for text in ["18446744073709551616", "16777216t"] {
            let error = parse(text).unwrap_err();
            assert!(error.contains("64-bit"), "{text}: {error}");
        }
    }

    #[test]
    fn display_uses_the_largest_exact_binary_unit_and_parses_back() {
        for (bytes, text) in [
            (0, "0"),
            (1_000, "1000"),
            (1_024, "1KiB"),
            (1_536, "1536"),
            (4 << 20, "4MiB"),
            (512 << 20, "512MiB"),
            (8 << 30, "8GiB"),
            (1 << 50, "1024TiB"),
        ] {
            let size = ByteSize::new(bytes);
            assert_eq!(size.to_string(), text);
            assert_eq!(parse(text), Ok(bytes));
        }
    }
}
