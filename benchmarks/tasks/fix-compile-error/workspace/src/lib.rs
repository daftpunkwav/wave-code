pub fn bump(start: u32) -> u32 {
    let count = start;
    count += 1;
    count += 1;
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bump_adds_two() {
        assert_eq!(bump(1), 3);
    }
}
