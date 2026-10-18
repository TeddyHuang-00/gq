//! Render records as CSV, quoting and escaping fields as needed.

use anyhow::{Context, Result};
use serde::Serialize;

/// Renders `headers` and `rows` as CSV, one record per line.
pub fn render<H, T>(headers: H, rows: &[T]) -> Result<String>
where
    H: IntoIterator,
    H::Item: AsRef<str>,
    T: Serialize,
{
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .from_writer(Vec::new());
    let headers = headers
        .into_iter()
        .map(|field| field.as_ref().to_owned())
        .collect::<Vec<_>>();
    writer.write_record(&headers)?;
    for row in rows {
        writer.serialize(row)?;
    }
    let bytes = writer.into_inner().context("cannot flush CSV output")?;
    String::from_utf8(bytes).context("CSV output is not UTF-8")
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    #[derive(Serialize)]
    struct Row {
        index: usize,
        name: &'static str,
    }

    #[test]
    fn renders_headers_and_serialized_rows() {
        let rows = [Row {
            index: 1,
            name: "a,\"b",
        }];
        assert_eq!(
            super::render(["index", "name"], &rows).unwrap_or_else(|err| panic!("{err}")),
            "index,name\n1,\"a,\"\"b\"\n"
        );
    }
}
