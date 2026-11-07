//! Accent- and case-insensitive text folding for keyword matching.
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// Decomposes (NFD), drops combining marks, then lowercases: "Ação" -> "acao".
pub(super) fn normalize(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}
