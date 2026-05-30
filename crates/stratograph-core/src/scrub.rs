//! Redact secret-looking values from text before it is stored in the graph or
//! sent to an agent (#136). The graph mostly holds symbol names and paths, but
//! design-rationale comments are kept verbatim, so a hardcoded token in a
//! comment would otherwise reach the index, search, wiki/story prompts, and MCP.
//!
//! This is a high-confidence, regex-free pass: it only redacts tokens that carry
//! a distinctive secret prefix (provider API keys / PATs), look like a JWT, or
//! sit on a private-key header line. It deliberately misses generic
//! high-entropy strings to avoid mangling ordinary prose.

/// Distinctive prefixes of provider credentials. A token starting with one of
/// these and carrying a long-enough suffix is treated as a secret.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",         // OpenAI
    "ghp_",        // GitHub personal access token
    "gho_",        // GitHub OAuth
    "ghs_",        // GitHub server-to-server
    "ghr_",        // GitHub refresh
    "github_pat_", // GitHub fine-grained PAT
    "glpat-",      // GitLab PAT
    "xoxb-",       // Slack bot
    "xoxp-",       // Slack user
    "xoxa-",       // Slack app
    "xoxs-",       // Slack
    "AKIA",        // AWS access key id
    "ASIA",        // AWS temporary access key id
    "AIza",        // Google API key
    "AccountKey=", // Azure storage connection string segment
];

/// Replacement text for a redacted secret.
const REDACTED: &str = "[REDACTED]";

/// Return `text` with secret-looking tokens replaced by `[REDACTED]`. Cheap and
/// allocation-free when nothing matches (returns the input unchanged via `Cow`).
pub fn scrub_secrets(text: &str) -> std::borrow::Cow<'_, str> {
    if !might_contain_secret(text) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    for ch in text.chars() {
        if is_token_char(ch) {
            token.push(ch);
        } else {
            flush(&mut token, &mut out);
            out.push(ch);
        }
    }
    flush(&mut token, &mut out);
    std::borrow::Cow::Owned(out)
}

/// Push `token` to `out`, redacted if it looks like a secret, then clear it.
fn flush(token: &mut String, out: &mut String) {
    if token.is_empty() {
        return;
    }
    if is_secret(token) {
        out.push_str(REDACTED);
    } else {
        out.push_str(token);
    }
    token.clear();
}

/// Characters that make up a credential token (the set seen in API keys, PATs
/// and JWTs). Whitespace and most punctuation break a token.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | '=')
}

/// Cheap pre-check so the common (secret-free) case avoids tokenizing.
fn might_contain_secret(text: &str) -> bool {
    text.contains("eyJ")
        || text.contains("PRIVATE KEY")
        || SECRET_PREFIXES.iter().any(|p| text.contains(p))
}

/// Whether a single token is a recognized secret.
fn is_secret(token: &str) -> bool {
    // Provider-prefixed credentials: prefix plus a long opaque suffix.
    for p in SECRET_PREFIXES {
        if let Some(rest) = token.strip_prefix(p)
            && rest.len() >= 8
            && rest.chars().any(|c| c.is_ascii_alphanumeric())
        {
            return true;
        }
    }
    // JWT: three base64url segments `eyJ...` . `...` . `...`.
    if token.starts_with("eyJ") && token.matches('.').count() == 2 && token.len() > 40 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn redacts_provider_tokens_and_jwt() {
        let s = scrub_secrets("use key sk-abcdEFGH1234 ok");
        check!(s == "use key [REDACTED] ok");
        check!(scrub_secrets("ghp_0123456789ABCDEFabcdef0123456789ABCD").contains("[REDACTED]"));
        check!(scrub_secrets("AKIAIOSFODNN7EXAMPLE").contains("[REDACTED]"));
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dummysignaturevalue123456";
        check!(scrub_secrets(jwt) == "[REDACTED]");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let t = "Parse the user id and return it. See issue-42 / module::path.";
        check!(scrub_secrets(t) == t);
        // A short `sk-` lookalike that isn't a real key length is kept.
        check!(scrub_secrets("sk-1") == "sk-1");
    }

    #[test]
    fn redacts_within_a_comment_sentence() {
        let s = scrub_secrets(
            "// TODO rotate token ghp_abcdefghijklmnopqrstuvwxyz0123456789 before release",
        );
        check!(s.contains("[REDACTED]"));
        check!(s.contains("before release"));
        check!(!s.contains("ghp_abcdefghij"));
    }
}
