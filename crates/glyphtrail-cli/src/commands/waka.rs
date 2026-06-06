//! WakaTime summaries client (#486).
//!
//! Blocking HTTP via `ureq` — the repo's HTTP style (the `waka` crate is async /
//! reqwest+tokio, which this codebase doesn't use). Pulling summaries is an opt-in,
//! off-machine fetch announced to the user before any request; the API key is read
//! from `WAKATIME_API_KEY` and never stored.

use anyhow::{Result, anyhow, bail};
use glyphtrail_core::WakaStat;
use serde_json::Value;

/// Default WakaTime cloud API base; overridable (e.g. a self-hosted Wakapi).
pub const DEFAULT_BASE: &str = "https://wakatime.com/api/v1";

/// The host of the configured base URL, for the off-machine transparency banner.
pub fn host(base_url: Option<&str>) -> String {
    crate::commands::embed_provider::host_of(base_url.unwrap_or(DEFAULT_BASE))
}

/// The marginal per-day breakdowns WakaTime reports, mapped to our `dimension`
/// names. (The values are independent aggregations of the same day's time, so they
/// can't be cross-tabulated — each is a marginal total.)
const DIMENSIONS: &[(&str, &str)] = &[
    ("projects", "project"),
    ("languages", "language"),
    ("editors", "editor"),
    ("operating_systems", "os"),
    ("machines", "machine"),
    ("categories", "category"),
];

/// Fetch daily WakaTime summaries for the inclusive `[start, end]` range
/// (`YYYY-MM-DD`) and flatten them into per-`(date, dimension, name)` [`WakaStat`]
/// rows, plus a `total` row per day. Off-machine network call.
pub fn fetch_summaries(base_url: Option<&str>, start: &str, end: &str) -> Result<Vec<WakaStat>> {
    let key = std::env::var("WAKATIME_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .ok_or_else(|| anyhow!("set WAKATIME_API_KEY to sync WakaTime data"))?;
    let base = base_url.unwrap_or(DEFAULT_BASE).trim_end_matches('/');
    let url = format!("{base}/users/current/summaries?start={start}&end={end}");
    // WakaTime authenticates with the API key base64-encoded as HTTP Basic.
    let resp: Value = ureq::get(&url)
        .header(
            "Authorization",
            format!("Basic {}", base64(key.trim().as_bytes())),
        )
        .call()
        .map_err(|e| anyhow!("WakaTime request failed: {e}"))?
        .into_body()
        .read_json()
        .map_err(|e| anyhow!("WakaTime response was not JSON: {e}"))?;
    let days = resp["data"]
        .as_array()
        .ok_or_else(|| anyhow!("unexpected WakaTime response (no `data` array)"))?;
    let mut out = Vec::new();
    for day in days {
        let date = day["range"]["date"].as_str().unwrap_or_default();
        if date.is_empty() {
            continue;
        }
        if let Some(total) = day["grand_total"]["total_seconds"].as_f64()
            && total > 0.0
        {
            out.push(WakaStat {
                date: date.to_string(),
                dimension: "total".into(),
                name: String::new(),
                seconds: total as i64,
            });
        }
        for (field, dim) in DIMENSIONS {
            let Some(items) = day[*field].as_array() else {
                continue;
            };
            for item in items {
                let name = item["name"].as_str().unwrap_or_default();
                let secs = item["total_seconds"].as_f64().unwrap_or(0.0);
                if name.is_empty() || secs <= 0.0 {
                    continue;
                }
                out.push(WakaStat {
                    date: date.to_string(),
                    dimension: (*dim).to_string(),
                    name: name.to_string(),
                    seconds: secs as i64,
                });
            }
        }
    }
    if out.is_empty() {
        bail!("WakaTime returned no tracked time for {start}..={end}");
    }
    Ok(out)
}

/// Standard base64 (RFC 4648, padded) — avoids a dependency for one auth header.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn base64_matches_known_vectors() {
        check!(base64(b"") == "");
        check!(base64(b"f") == "Zg==");
        check!(base64(b"fo") == "Zm8=");
        check!(base64(b"foo") == "Zm9v");
        check!(base64(b"foob") == "Zm9vYg==");
        check!(base64(b"fooba") == "Zm9vYmE=");
        check!(base64(b"foobar") == "Zm9vYmFy");
    }
}
