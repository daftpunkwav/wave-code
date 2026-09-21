pub mod report;

pub fn fetch_rows(table: &str) -> Vec<String> {
    query_rows(table)
}

fn query_rows(table: &str) -> Vec<String> {
    vec![table.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_one_row_per_table() {
        assert_eq!(fetch_rows("t"), vec!["t".to_string()]);
    }
}
