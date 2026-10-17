/// Render rows of text fields as CSV, quoting and escaping each field as
/// needed.
pub fn render<I, R, C>(rows: I) -> String
where
    I: IntoIterator<Item = R>,
    R: IntoIterator<Item = C>,
    C: AsRef<str>,
{
    let mut output = String::new();
    for row in rows {
        let mut first = true;
        for cell in row {
            if !first {
                output.push(',');
            }
            first = false;
            let value = cell.as_ref();
            if value.contains([',', '"', '\n', '\r']) {
                output.push('"');
                output.push_str(&value.replace('"', "\"\""));
                output.push('"');
            } else {
                output.push_str(value);
            }
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_generic_rows_and_escapes_fields() {
        let rows = [["index", "name"], ["1", "a,\"b"]];
        assert_eq!(super::render(rows), "index,name\n1,\"a,\"\"b\"\n");
    }
}
