use pretty_assertions::assert_eq;

use super::*;

#[test]
fn mentions_are_words_that_start_with_an_at_sign() {
    assert_eq!(
        mentioned_names("Compare with @try-sqlite and @fork-1a2b3c4d. Also @try-sqlite again."),
        vec!["try-sqlite".to_string(), "fork-1a2b3c4d".to_string()]
    );
    assert_eq!(
        mentioned_names("mail me at dev@example.com"),
        Vec::<String>::new()
    );
    assert_eq!(
        mentioned_names("看看@试验分支 的结论"),
        vec!["试验分支".to_string()]
    );
    assert_eq!(mentioned_names("an @ alone"), Vec::<String>::new());
}
