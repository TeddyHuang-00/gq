//! Parsing individual GPU index ranges such as `0` or `0-3`.

use std::{fmt, ops::Range};

/// Parse one inclusive GPU index range into Rust's half-open range type.
///
/// A single index such as `3` becomes `3..4`; `0-3` becomes `0..4`.
pub fn parse(input: &str) -> Result<Range<usize>, RangeError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(RangeError::new(input, 0, ErrorKind::Empty));
    }
    let (start, end) = if let Some((start, rest)) = input.split_once('-') {
        (
            index(start, input, 0)?,
            index(rest, input, start.len() + 1)?,
        )
    } else {
        let value = index(input, input, 0)?;
        (value, value)
    };
    if start > end {
        return Err(RangeError::new(
            input,
            0,
            ErrorKind::Reversed { start, end },
        ));
    }
    let exclusive_end = end
        .checked_add(1)
        .ok_or_else(|| RangeError::new(input, 0, ErrorKind::IndexTooLarge))?;
    Ok(start..exclusive_end)
}

/// Parses one digit run, reporting errors at `offset` in the whole input.
fn index(text: &str, input: &str, offset: usize) -> Result<usize, RangeError> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(RangeError::new(input, offset, ErrorKind::ExpectedIndex));
    }
    let value = text[..digits]
        .parse()
        .map_err(|_| RangeError::new(input, offset, ErrorKind::IndexTooLarge))?;
    if digits == text.len() {
        return Ok(value);
    }
    Err(RangeError::new(
        input,
        offset + digits,
        ErrorKind::UnexpectedTrailing,
    ))
}

#[derive(Clone, Copy, Debug)]
enum ErrorKind {
    Empty,
    ExpectedIndex,
    UnexpectedTrailing,
    IndexTooLarge,
    Reversed { start: usize, end: usize },
}

/// An invalid range value, displayed with the failing input and a caret.
#[derive(Debug)]
pub struct RangeError {
    input: String,
    position: usize,
    kind: ErrorKind,
}

impl RangeError {
    fn new(input: &str, offset: usize, kind: ErrorKind) -> Self {
        let position = input[..offset.min(input.len())].chars().count() + 1;
        Self {
            input: input.to_owned(),
            position,
            kind,
        }
    }
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            input,
            position,
            kind,
        } = self;
        if let ErrorKind::Reversed { start, end } = kind {
            return write!(
                f,
                "invalid range: start {start} is greater than end {end}\n  {input}\n  {underline}",
                underline = "^".repeat(input.chars().count())
            );
        }
        let message = match kind {
            ErrorKind::Empty => return f.write_str("range is empty"),
            ErrorKind::ExpectedIndex => "expected a GPU index",
            ErrorKind::UnexpectedTrailing => "unexpected trailing characters",
            ErrorKind::IndexTooLarge => "GPU index too large",
            ErrorKind::Reversed { .. } => unreachable!(),
        };
        writeln!(f, "invalid range: {message}")?;
        writeln!(f, "  {input}")?;
        write!(f, "  {}^", " ".repeat(position.saturating_sub(1)))
    }
}

impl std::error::Error for RangeError {}

#[cfg(test)]
mod tests {
    use super::parse;

    fn error(input: &str) -> String {
        match parse(input) {
            Ok(_) => panic!("range should be rejected"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn parses_single_index_and_inclusive_span() {
        assert_eq!(parse("0").ok(), Some(0..1));
        assert_eq!(parse("0-3").ok(), Some(0..4));
        assert_eq!(parse(" 3-3 ").ok(), Some(3..4));
    }

    #[test]
    fn displays_positioned_errors() {
        assert_eq!(
            error("1-"),
            "invalid range: expected a GPU index\n  1-\n    ^"
        );
        assert_eq!(
            error("1-2-3"),
            "invalid range: unexpected trailing characters\n  1-2-3\n     ^"
        );
        assert_eq!(
            error("5-3"),
            "invalid range: start 5 is greater than end 3\n  5-3\n  ^^^"
        );
    }

    #[test]
    fn rejects_empty_and_overflowing_ranges() {
        assert_eq!(error("   "), "range is empty");
        assert!(error(&usize::MAX.to_string()).contains("GPU index too large"));
    }
}
