//! Parsing individual GPU index ranges such as `0` or `0-3`.

use std::{fmt, ops::Range};

use nom::{
    IResult, Parser,
    branch::alt,
    character::complete::{char, digit1},
    combinator::{all_consuming, cut, map},
    error::{ErrorKind as NomErrorKind, ParseError},
    sequence::separated_pair,
};

type Parsed<'a, T> = IResult<&'a str, T, ParseFailure<'a>>;

/// Parse one inclusive GPU index range into Rust's half-open range type.
///
/// A single index such as `3` becomes `3..4`; `0-3` becomes `0..4`.
pub fn parse(input: &str) -> Result<Range<usize>, RangeError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(RangeError::new(input, 0, ErrorKind::Empty));
    }

    let mut parser = all_consuming(alt((span, map(index, |value| (value, value)))));
    let (start, end) = match parser.parse(input) {
        Ok((_, range)) => range,
        Err(error) => return Err(RangeError::from_nom(input, &error)),
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

fn index(input: &str) -> Parsed<'_, usize> {
    let (rest, digits) = digit1(input)?;
    digits.parse().map_or(
        Err(nom::Err::Failure(ParseFailure {
            input,
            kind: ErrorKind::IndexTooLarge,
        })),
        |value| Ok((rest, value)),
    )
}

fn span(input: &str) -> Parsed<'_, (usize, usize)> {
    separated_pair(index, char('-'), cut(index)).parse(input)
}

#[derive(Clone, Copy, Debug)]
struct ParseFailure<'a> {
    input: &'a str,
    kind: ErrorKind,
}

impl<'a> ParseError<&'a str> for ParseFailure<'a> {
    fn from_error_kind(input: &'a str, kind: NomErrorKind) -> Self {
        let kind = match kind {
            NomErrorKind::Digit => ErrorKind::ExpectedIndex,
            NomErrorKind::Eof => ErrorKind::UnexpectedTrailing,
            _ => ErrorKind::ExpectedRange,
        };
        Self { input, kind }
    }

    fn append(_input: &'a str, _kind: NomErrorKind, other: Self) -> Self {
        other
    }

    fn or(self, other: Self) -> Self {
        if other.input.len() < self.input.len() {
            other
        } else {
            self
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ErrorKind {
    Empty,
    ExpectedIndex,
    ExpectedRange,
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

    fn from_nom(input: &str, error: &nom::Err<ParseFailure<'_>>) -> Self {
        let failure = match error {
            nom::Err::Error(failure) | nom::Err::Failure(failure) => failure,
            nom::Err::Incomplete(_) => {
                return Self::new(input, input.len(), ErrorKind::ExpectedIndex);
            }
        };
        let offset = input.len() - failure.input.len();
        Self::new(input, offset, failure.kind)
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
            ErrorKind::ExpectedRange => "expected `-` between GPU indices",
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
