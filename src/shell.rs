pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
