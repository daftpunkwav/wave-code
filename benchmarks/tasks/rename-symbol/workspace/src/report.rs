use crate::fetch_rows;

pub fn headline() -> String {
    format!("{} rows", fetch_rows("events").len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_rows() {
        assert_eq!(headline(), "1 rows");
    }
}
