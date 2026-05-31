//! Output styling. A human reading a terminal gets the polished
//! glyphtrail.dev look (middots, glyphs, aligned columns); a coding agent
//! calling the CLI gets plain, parse-friendly lines instead.
//!
//! The agent is detected from the environment via `detect_coding_agent` (e.g.
//! Claude Code, Cursor, Copilot setting their marker variables). It's a
//! best-effort signal, so `GLYPHTRAIL_PLAIN=1` forces plain output for CI and
//! scripts, and the existing `--json` / `--yaml` flags remain the contract for
//! machine consumption.

use std::sync::OnceLock;

/// Whether to render the styled (human) form. False when a coding agent is
/// detected or plain output is forced.
pub fn pretty() -> bool {
    static PRETTY: OnceLock<bool> = OnceLock::new();
    *PRETTY.get_or_init(|| {
        std::env::var_os("GLYPHTRAIL_PLAIN").is_none_or(|v| v == "0")
            && !detect_coding_agent::is_agent()
    })
}

/// A thousands-separated count, e.g. `6840` → `6,840`.
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A compact duration, e.g. `1.8s` or `420ms`.
pub fn duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 1.0 {
        format!("{secs:.1}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn count_groups_thousands() {
        check!(count(0) == "0");
        check!(count(840) == "840");
        check!(count(6840) == "6,840");
        check!(count(11209) == "11,209");
        check!(count(1234567) == "1,234,567");
    }

    #[test]
    fn duration_switches_unit() {
        check!(duration(std::time::Duration::from_millis(420)) == "420ms");
        check!(duration(std::time::Duration::from_secs_f64(1.84)) == "1.8s");
    }
}
