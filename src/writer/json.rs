/// Serialize any `serde` value as compact JSON.
pub fn render<T: serde::Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string(value)
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_strings_and_renders_numeric_fields() {
        let rows = vec![vec!["X", "NV4"], vec!["NV4", "X"]];
        let rendered = super::render(&rows);
        assert!(rendered.is_ok());
        assert_eq!(
            rendered.unwrap_or_else(|error| panic!("JSON serialization failed: {error}")),
            "[[\"X\",\"NV4\"],[\"NV4\",\"X\"]]"
        );
    }
}
