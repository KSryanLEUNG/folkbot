//! Token counting via tiktoken.
//!
//! Use `cl100k_base` encoding by default — it's the OpenAI tokenizer family
//! and a decent approximation for any modern model (Claude, Gemini, Poe-routed
//! models, etc.). Provider-exact tokenizers can come later if precision
//! becomes critical.
//!
//! Lazy-init the BPE table once per process via `OnceLock`. If init fails
//! (shouldn't, but) we fall back to a quick char-weighted approximation.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

static BPE: OnceLock<Option<CoreBPE>> = OnceLock::new();

fn bpe() -> Option<&'static CoreBPE> {
    BPE.get_or_init(|| tiktoken_rs::cl100k_base().ok())
        .as_ref()
}

pub fn count(text: &str) -> usize {
    if let Some(bpe) = bpe() {
        bpe.encode_with_special_tokens(text).len()
    } else {
        approx_count(text)
    }
}

/// Char-weighted fallback: CJK ≈ 1 token/char, ASCII ≈ 0.25 token/char.
/// Used when tiktoken init fails. Roughly within ±20% for mixed content.
fn approx_count(text: &str) -> usize {
    let mut weight = 0.0_f64;
    for c in text.chars() {
        if (c as u32) < 128 {
            weight += 0.25;
        } else {
            weight += 1.0;
        }
    }
    (weight as usize) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_text_counts_more_than_one_token() {
        // "Hello, world!" — short ASCII, expect a small but non-zero count.
        let n = count("Hello, world!");
        assert!(n > 0 && n < 10, "ascii count out of range: {}", n);
    }

    #[test]
    fn cjk_text_counts_per_char_approx() {
        // Each CJK char tends to be ≥1 token in cl100k_base. 5 chars → ≥5.
        let n = count("家庭友善助理");
        assert!(n >= 5, "cjk count too low: {}", n);
    }

    #[test]
    fn empty_string_is_zero() {
        assert_eq!(count(""), 0);
    }

    #[test]
    fn approx_fallback_reasonable() {
        // Verify the fallback path returns something monotonic with input size.
        let small = approx_count("hi");
        let big = approx_count("the quick brown fox jumps over the lazy dog");
        assert!(big > small);
    }
}
