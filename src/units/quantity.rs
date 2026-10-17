//! Parsing of user-facing quantities: VRAM sizes and percentages.

use std::fmt;

use nom::{
    IResult, Parser,
    character::complete::{char, digit1},
    combinator::{opt, recognize},
    sequence::preceded,
};

/// A VRAM threshold: an absolute byte size or a percentage of total memory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Vram {
    Bytes(u64),
    Percent(Percent),
}

/// A percentage in the range 0 through 100.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Percent(pub f64);

impl Vram {
    /// Parse a size such as `500M`, `1GiB`, `1.5G`, or `5%`.
    ///
    /// Units are case-insensitive and binary (`K` is KiB, `M` is MiB, and so
    /// on). A number without a unit is taken as MiB.
    ///
    /// # Errors
    ///
    /// Returns a [`ValueError`] pointing at the first problematic character.
    pub fn parse(input: &str) -> Result<Self, ValueError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ValueError::new(input, 0, "size is empty"));
        }
        if input.starts_with('-') {
            return Err(ValueError::new(input, 0, "size must not be negative"));
        }

        let (rest, token) =
            number(input).map_err(|_| ValueError::new(input, 0, "expected a number"))?;
        let offset = input.len() - rest.len();

        if let Some(tail) = rest.strip_prefix('%') {
            if !tail.is_empty() {
                return Err(ValueError::new(
                    input,
                    input.len() - tail.len(),
                    "expected end of input after `%`",
                ));
            }
            return Ok(Self::Percent(Percent(percent(token, input)?)));
        }

        let factor = if rest.is_empty() {
            1024.0f64.powi(2)
        } else {
            unit_factor(rest)
                .ok_or_else(|| ValueError::new(input, offset, format!("unknown unit `{rest}`")))?
        };
        Ok(Self::Bytes(bytes(token, factor, input)?))
    }
}

impl Percent {
    /// Parse a percentage such as `5%` or `5`.
    ///
    /// # Errors
    ///
    /// Returns a [`ValueError`] pointing at the first problematic character.
    pub fn parse(input: &str) -> Result<Self, ValueError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(ValueError::new(input, 0, "percentage is empty"));
        }
        if input.starts_with('-') {
            return Err(ValueError::new(input, 0, "percentage must not be negative"));
        }

        let (rest, token) =
            number(input).map_err(|_| ValueError::new(input, 0, "expected a number"))?;
        let rest = rest.strip_prefix('%').unwrap_or(rest);
        if !rest.is_empty() {
            let message = if rest.starts_with('%') {
                "unexpected `%`"
            } else {
                "expected `%`"
            };
            return Err(ValueError::new(input, input.len() - rest.len(), message));
        }
        Ok(Self(percent(token, input)?))
    }
}

/// Match an unsigned decimal number, returning the matched text.
fn number(input: &str) -> IResult<&str, &str> {
    recognize((digit1, opt(preceded(char('.'), digit1)))).parse(input)
}

/// Binary multiplier for a size suffix, case-insensitive.
fn unit_factor(unit: &str) -> Option<f64> {
    match unit.to_ascii_lowercase().as_str() {
        "b" => Some(1.0),
        "k" | "kb" | "kib" => Some(1024.0),
        "m" | "mb" | "mib" => Some(1024.0f64.powi(2)),
        "g" | "gb" | "gib" => Some(1024.0f64.powi(3)),
        "t" | "tb" | "tib" => Some(1024.0f64.powi(4)),
        _ => None,
    }
}

/// Convert a decimal number and a binary multiplier into bytes.
fn bytes(token: &str, factor: f64, input: &str) -> Result<u64, ValueError> {
    let value = number_value(token, input)? * factor;
    if value >= 2.0f64.powi(64) {
        return Err(ValueError::new(input, 0, "size is too large"));
    }
    // The range check keeps this cast inside u64; rounding picks the nearest
    // byte.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value.round() as u64)
}

/// Parse a percentage value, capped at 100.
fn percent(token: &str, input: &str) -> Result<f64, ValueError> {
    let value = number_value(token, input)?;
    if value > 100.0 {
        return Err(ValueError::new(
            input,
            0,
            "percentage must be between 0 and 100",
        ));
    }
    Ok(value)
}

/// Parse the numeric token into a finite `f64`.
fn number_value(token: &str, input: &str) -> Result<f64, ValueError> {
    let value = token
        .parse::<f64>()
        .map_err(|_| ValueError::new(input, 0, "expected a number"))?;
    if !value.is_finite() {
        return Err(ValueError::new(input, 0, "number is too large"));
    }
    Ok(value)
}

/// A rejected size or percentage, positioned at the character to fix.
#[derive(Debug)]
pub struct ValueError {
    input: String,
    position: usize,
    message: String,
}

impl ValueError {
    fn new(input: &str, offset: usize, message: impl Into<String>) -> Self {
        let position = input
            .char_indices()
            .take_while(|&(index, _)| index < offset)
            .count()
            + 1;
        Self {
            input: input.to_owned(),
            position,
            message: message.into(),
        }
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            input,
            position,
            message,
        } = self;
        if input.is_empty() {
            return f.write_str(message);
        }
        writeln!(f, "{message}")?;
        writeln!(f, "  {input}")?;
        write!(f, "  {}^", " ".repeat(position.saturating_sub(1)))
    }
}

impl std::error::Error for ValueError {}

#[cfg(test)]
mod tests {
    use super::{Percent, Vram};

    fn size_error(input: &str) -> String {
        let Err(error) = Vram::parse(input) else {
            panic!("expected `{input}` to be rejected");
        };
        error.to_string()
    }

    fn percent_error(input: &str) -> String {
        let Err(error) = Percent::parse(input) else {
            panic!("expected `{input}` to be rejected");
        };
        error.to_string()
    }

    #[test]
    fn parses_vram_sizes() {
        assert_eq!(
            Vram::parse("500M").ok(),
            Some(Vram::Bytes(500 * 1024 * 1024))
        );
        assert_eq!(
            Vram::parse("1GiB").ok(),
            Some(Vram::Bytes(1024 * 1024 * 1024))
        );
        assert_eq!(Vram::parse("1.5G").ok(), Some(Vram::Bytes(1_610_612_736)));
        assert_eq!(
            Vram::parse("500").ok(),
            Some(Vram::Bytes(500 * 1024 * 1024))
        );
        assert_eq!(Vram::parse("2kib").ok(), Some(Vram::Bytes(2048)));
        assert_eq!(Vram::parse("1T").ok(), Some(Vram::Bytes(1_099_511_627_776)));
    }

    #[test]
    fn parses_vram_percentage() {
        assert_eq!(Vram::parse("5%").ok(), Some(Vram::Percent(Percent(5.0))));
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(size_error(""), "size is empty");
        assert_eq!(size_error("abc"), "expected a number\n  abc\n  ^");
        assert_eq!(size_error("-5G"), "size must not be negative\n  -5G\n  ^");
        assert_eq!(size_error("1X"), "unknown unit `X`\n  1X\n   ^");
        assert_eq!(
            size_error("5%x"),
            "expected end of input after `%`\n  5%x\n    ^"
        );
    }

    #[test]
    fn parses_percentages() {
        assert_eq!(Percent::parse("5%").ok(), Some(Percent(5.0)));
        assert_eq!(Percent::parse("5").ok(), Some(Percent(5.0)));
        assert_eq!(Percent::parse(" 5.5% ").ok(), Some(Percent(5.5)));
        assert_eq!(Percent::parse("0%").ok(), Some(Percent(0.0)));
        assert_eq!(Percent::parse("100%").ok(), Some(Percent(100.0)));
    }

    #[test]
    fn rejects_bad_percentages() {
        assert_eq!(percent_error(""), "percentage is empty");
        assert_eq!(
            percent_error("-1%"),
            "percentage must not be negative\n  -1%\n  ^"
        );
        assert_eq!(
            percent_error("101%"),
            "percentage must be between 0 and 100\n  101%\n  ^"
        );
        assert_eq!(percent_error("5GB"), "expected `%`\n  5GB\n   ^");
        assert_eq!(percent_error("5%%"), "unexpected `%`\n  5%%\n    ^");
    }
}
