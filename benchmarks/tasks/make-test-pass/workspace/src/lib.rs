/// Count the words in `text`, where words are whitespace-separated runs.
pub fn word_count(text: &str) -> usize {
    unimplemented!("word counting is not written yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_space_separated_words() {
        assert_eq!(word_count("one two three"), 3);
    }

    #[test]
    fn empty_text_has_no_words() {
        assert_eq!(word_count("   
 "), 0);
    }

    #[test]
    fn repeated_separators_count_once() {
        assert_eq!(word_count("a  b"), 2);
    }
}
