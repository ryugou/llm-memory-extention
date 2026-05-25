//! Concept name validation. Concept names live in URLs (wiki_read concept param),
//! in MCP tool args (wiki_rebuild concept), and in LLM-generated output (Haiku
//! affected_existing / new_concepts). LLM output is not a trust boundary, so the
//! same validator gates every entry point.

/// Returns true iff `s` is a valid concept name:
/// - length 2-64
/// - first char `[a-z0-9]`
/// - remaining chars `[a-z0-9-]`
///
/// Matches the format declared in `EXTRACT_CONCEPTS_SYSTEM` prompt.
/// 同形の regex を持つ [`crate::id::SharedMemoryId`] は最小長 1 だが、こちらは
/// 最小長 2 にしている: concept 名は LLM 生成が中心で 1 文字は hallucination /
/// garbage がほとんどなので reject、一方 SharedMemoryId は運用者が手で割り当てる
/// 識別子なので 1 文字を許容する (例: `a`)。
pub fn is_valid(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9][a-z0-9-]{1,63}$").unwrap());
    re.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_concepts() {
        assert!(is_valid("vegapunk"));
        assert!(is_valid("team-frontend"));
        assert!(is_valid("rust-2024"));
        assert!(is_valid("a1"));
    }

    #[test]
    fn rejects_too_short() {
        assert!(!is_valid(""));
        assert!(!is_valid("a"));
    }

    #[test]
    fn rejects_too_long() {
        let s = "a".repeat(65);
        assert!(!is_valid(&s));
    }

    #[test]
    fn accepts_max_length() {
        let s = "a".repeat(64);
        assert!(is_valid(&s));
    }

    #[test]
    fn rejects_uppercase() {
        assert!(!is_valid("Vegapunk"));
        assert!(!is_valid("API"));
    }

    #[test]
    fn rejects_whitespace_and_specials() {
        assert!(!is_valid("with space"));
        assert!(!is_valid("dot.case"));
        assert!(!is_valid("snake_case"));
        assert!(!is_valid("slash/x"));
        assert!(!is_valid("quote\"x"));
    }

    #[test]
    fn rejects_leading_hyphen() {
        assert!(!is_valid("-leading"));
    }

    #[test]
    fn accepts_trailing_hyphen() {
        // SharedMemoryId と同じ regex shape を採用: 末尾 hyphen は許容。
        assert!(is_valid("trailing-"));
    }
}
