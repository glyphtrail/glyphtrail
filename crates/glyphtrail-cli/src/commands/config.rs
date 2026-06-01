//! `glyphtrail config` — inspect and edit the per-repo config file
//! (`.glyphtrail/config.toml`) without hand-editing TOML. Get/set work on dotted
//! keys (`security.record_sensitive_files`, `impact.test_globs`); a `set` value
//! is parsed as TOML, so `true`, `42`, and `["a","b"]` keep their types, and any
//! edit is validated against the config schema before it is written.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use glyphtrail_core::config::{Config, RepoPaths};

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print the config file (or note that defaults are in effect).
    Show {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Print the config file path.
    Path {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Get a value by dotted key, e.g. `impact.test_globs`.
    Get {
        key: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Set a value by dotted key. The value is parsed as TOML (so `true`, `42`,
    /// `["a","b"]` keep their types); anything that isn't valid TOML is a string.
    Set {
        key: String,
        value: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove a key by dotted path.
    Unset {
        key: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

pub fn run(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Show { repo } => show(&repo),
        ConfigCmd::Path { repo } => {
            println!("{}", config_path(&repo).display());
            Ok(())
        }
        ConfigCmd::Get { key, repo } => get(&repo, &key),
        ConfigCmd::Set { key, value, repo } => set(&repo, &key, &value),
        ConfigCmd::Unset { key, repo } => unset(&repo, &key),
    }
}

fn config_path(repo: &Path) -> PathBuf {
    RepoPaths::new(repo).config_path()
}

/// Load the raw config document, or an empty table when the file is absent.
fn load_table(path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text.parse::<toml::Table>()?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(e.into()),
    }
}

/// Serialize, validate against the config schema, then write — so an edit never
/// leaves an invalid config behind.
fn save_table(path: &Path, table: &toml::Table) -> Result<()> {
    let text = toml::to_string_pretty(table)?;
    Config::from_toml_str(&text)
        .map_err(|e| anyhow!("that edit would make the config invalid: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn show(repo: &Path) -> Result<()> {
    let path = config_path(repo);
    match std::fs::read_to_string(&path) {
        Ok(text) => print!("{text}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("no config at {} — defaults are in effect", path.display());
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn get(repo: &Path, key: &str) -> Result<()> {
    let table = load_table(&config_path(repo))?;
    match lookup(&table, key) {
        Some(value) => println!("{value}"),
        None => println!("{key} is not set (the default applies)"),
    }
    Ok(())
}

fn set(repo: &Path, key: &str, value: &str) -> Result<()> {
    let path = config_path(repo);
    let mut table = load_table(&path)?;
    set_key(&mut table, key, parse_value(value))?;
    save_table(&path, &table)?;
    println!("set {key} ({})", path.display());
    Ok(())
}

fn unset(repo: &Path, key: &str) -> Result<()> {
    let path = config_path(repo);
    let mut table = load_table(&path)?;
    if !unset_key(&mut table, key) {
        bail!("{key} is not set in {}", path.display());
    }
    save_table(&path, &table)?;
    println!("unset {key} ({})", path.display());
    Ok(())
}

/// Parse `s` as a TOML value (`true`, `42`, `"x"`, `["a","b"]`); a fragment that
/// isn't valid TOML is taken as a bare string, so `set x foo` stores `"foo"`.
fn parse_value(s: &str) -> toml::Value {
    format!("v = {s}")
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(s.to_string()))
}

fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let parts: Vec<&str> = key.split('.').collect();
    let (leaf, parents) = parts.split_last()?;
    let mut cur = table;
    for p in parents {
        cur = cur.get(*p)?.as_table()?;
    }
    cur.get(*leaf)
}

fn set_key(table: &mut toml::Table, key: &str, value: toml::Value) -> Result<()> {
    let parts: Vec<&str> = key.split('.').collect();
    let (leaf, parents) = parts.split_last().ok_or_else(|| anyhow!("empty key"))?;
    let mut cur = table;
    for p in parents {
        let entry = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = entry
            .as_table_mut()
            .ok_or_else(|| anyhow!("cannot set '{key}': '{p}' is not a table"))?;
    }
    cur.insert(leaf.to_string(), value);
    Ok(())
}

fn unset_key(table: &mut toml::Table, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    let Some((leaf, parents)) = parts.split_last() else {
        return false;
    };
    let mut cur = table;
    for p in parents {
        match cur.get_mut(*p).and_then(|v| v.as_table_mut()) {
            Some(t) => cur = t,
            None => return false,
        }
    }
    cur.remove(*leaf).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    #[test]
    fn set_get_unset_dotted_keys_with_typed_values() {
        let mut table = toml::Table::new();
        set_key(
            &mut table,
            "security.record_sensitive_files",
            parse_value("true"),
        )
        .unwrap();
        set_key(
            &mut table,
            "impact.test_globs",
            parse_value(r#"["**/*_test.rs"]"#),
        )
        .unwrap();

        check!(
            lookup(&table, "security.record_sensitive_files") == Some(&toml::Value::Boolean(true))
        );
        let globs = lookup(&table, "impact.test_globs").unwrap();
        check!(globs.as_array().unwrap().len() == 1);

        check!(unset_key(&mut table, "security.record_sensitive_files"));
        check!(lookup(&table, "security.record_sensitive_files").is_none());
        check!(!unset_key(&mut table, "nope.missing"));
    }

    #[test]
    fn bare_word_falls_back_to_string() {
        check!(parse_value("hello") == toml::Value::String("hello".into()));
        check!(parse_value("42") == toml::Value::Integer(42));
    }
}
