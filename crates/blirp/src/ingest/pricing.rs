//! Cost estimates for transcripts that record tokens but not cost.
//!
//! Static list prices in USD per million tokens, per model family, as
//! published by the vendors and last reviewed on 2026-09-25. Estimates
//! only: they ignore tiered long-context pricing, batch discounts and
//! subscriptions. Models that match no family cost 0. When a transcript
//! reports its own cost (Claude `cost-state`, opencode, pi, aider) that
//! value is used instead.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const fn anthropic(input: f64, output: f64) -> Price {
    Price {
        input,
        output,
        cache_read: input * 0.1,
        cache_write: input * 1.25,
    }
}

const fn openai(input: f64, output: f64, cache_read: f64) -> Price {
    Price {
        input,
        output,
        cache_read,
        cache_write: input,
    }
}

/// Substring of the lowercased model id -> price. First match wins, so
/// specific entries precede their family.
const TABLE: &[(&str, Price)] = &[
    // Anthropic
    ("opus-4-1", anthropic(15.0, 75.0)),
    ("opus-4-0", anthropic(15.0, 75.0)),
    ("opus-4-2025", anthropic(15.0, 75.0)),
    ("3-opus", anthropic(15.0, 75.0)),
    ("opus", anthropic(5.0, 25.0)),
    ("sonnet", anthropic(3.0, 15.0)),
    ("3-5-haiku", anthropic(0.8, 4.0)),
    ("3-haiku", anthropic(0.25, 1.25)),
    ("haiku", anthropic(1.0, 5.0)),
    // OpenAI
    ("gpt-5-nano", openai(0.05, 0.4, 0.005)),
    ("gpt-5-mini", openai(0.25, 2.0, 0.025)),
    ("codex-mini", openai(1.5, 6.0, 0.375)),
    ("gpt-5", openai(1.25, 10.0, 0.125)),
    ("gpt-4.1-nano", openai(0.1, 0.4, 0.025)),
    ("gpt-4.1-mini", openai(0.4, 1.6, 0.1)),
    ("gpt-4.1", openai(2.0, 8.0, 0.5)),
    ("gpt-4o-mini", openai(0.15, 0.6, 0.075)),
    ("gpt-4o", openai(2.5, 10.0, 1.25)),
    ("o4-mini", openai(1.1, 4.4, 0.275)),
    ("o3", openai(2.0, 8.0, 0.5)),
    // Google
    ("gemini-3", openai(2.0, 12.0, 0.2)),
    ("gemini-2.5-pro", openai(1.25, 10.0, 0.125)),
    ("gemini-2.5-flash-lite", openai(0.1, 0.4, 0.01)),
    ("gemini-2.5-flash", openai(0.3, 2.5, 0.03)),
    ("gemini-2.0-flash", openai(0.1, 0.4, 0.025)),
    // DeepSeek
    ("deepseek", openai(0.28, 0.42, 0.028)),
];

pub fn price(model: &str) -> Option<Price> {
    let m = model.to_ascii_lowercase();
    TABLE.iter().find(|(k, _)| m.contains(k)).map(|(_, p)| *p)
}

/// Token counts of one request or a running total. `input` excludes cached
/// reads and cache writes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

/// Estimated USD cost of `usage` on `model`; 0 for unknown models.
pub fn estimate(model: &str, usage: Usage) -> f64 {
    let Some(p) = price(model) else {
        return 0.0;
    };
    let m = |tokens: i64, per_million: f64| tokens.max(0) as f64 * per_million / 1_000_000.0;
    m(usage.input, p.input)
        + m(usage.output, p.output)
        + m(usage.cache_read, p.cache_read)
        + m(usage.cache_write, p.cache_write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_and_unknowns() {
        assert_eq!(price("claude-opus-4-1-20250805").unwrap().input, 15.0);
        assert_eq!(price("claude-opus-4-5-20251101").unwrap().input, 5.0);
        assert_eq!(price("claude-sonnet-4-5").unwrap().output, 15.0);
        assert_eq!(price("gpt-5-codex").unwrap().input, 1.25);
        assert_eq!(price("gpt-5-mini").unwrap().input, 0.25);
        assert!(price("some-local-model").is_none());
        let u = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 0,
        };
        assert!((estimate("claude-sonnet-4", u) - 18.3).abs() < 1e-9);
        assert_eq!(estimate("mystery", u), 0.0);
    }
}
