//! `glyphtrail config` — inspect and edit the unified per-repo config file
//! (`glyphtrail.toml`, committed; `.glyphtrail/glyphtrail.toml` with `--local`)
//! without hand-editing TOML. Get/set work on dotted keys
//! (`security.record_sensitive_files`, `impact.test_globs`); a `set` value is
//! parsed as TOML, so `true`, `42`, and `["a","b"]` keep their types, and any
//! edit is validated against the config schema before it is written.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::commands::config_file;

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// Print the config file(s) (or note that defaults are in effect).
    Show {
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Print the config file path (committed, or the personal override with --local).
    Path {
        #[arg(long)]
        local: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Get a value by dotted key (personal override wins over committed).
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
        /// Write the personal override instead of the committed file.
        #[arg(long)]
        local: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Remove a key by dotted path.
    Unset {
        key: String,
        #[arg(long)]
        local: bool,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

pub fn run(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Show { repo } => show(&repo),
        ConfigCmd::Path { local, repo } => {
            println!("{}", config_file::target(&repo, local).display());
            Ok(())
        }
        ConfigCmd::Get { key, repo } => get(&repo, &key),
        ConfigCmd::Set {
            key,
            value,
            local,
            repo,
        } => set(&repo, local, &key, &value),
        ConfigCmd::Unset { key, local, repo } => unset(&repo, local, &key),
    }
}

fn show(repo: &Path) -> Result<()> {
    let mut any = false;
    for (label, path) in [
        ("committed", config_file::committed(repo)),
        ("local", config_file::local(repo)),
    ] {
        if let Ok(text) = std::fs::read_to_string(&path) {
            any = true;
            println!("# {label}: {}", path.display());
            print!("{text}");
            if !text.ends_with('\n') {
                println!();
            }
        }
    }
    if !any {
        println!("no config — defaults are in effect");
    }
    Ok(())
}

fn get(repo: &Path, key: &str) -> Result<()> {
    // Personal override wins over the committed file.
    for path in [config_file::local(repo), config_file::committed(repo)] {
        let table = config_file::load_table(&path)?;
        if let Some(value) = lookup(&table, key) {
            println!("{value}");
            return Ok(());
        }
    }
    println!("{key} is not set (the default applies)");
    Ok(())
}

fn set(repo: &Path, local: bool, key: &str, value: &str) -> Result<()> {
    config_file::migrate_legacy(repo)?;
    let path = config_file::target(repo, local);
    let mut table = config_file::load_table(&path)?;
    set_key(&mut table, key, parse_value(value))?;
    config_file::save(&path, &table)?;
    println!("set {key} ({})", path.display());
    Ok(())
}

fn unset(repo: &Path, local: bool, key: &str) -> Result<()> {
    config_file::migrate_legacy(repo)?;
    let path = config_file::target(repo, local);
    let mut table = config_file::load_table(&path)?;
    if !unset_key(&mut table, key) {
        bail!("{key} is not set in {}", path.display());
    }
    config_file::save(&path, &table)?;
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
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("empty key"))?;
    let mut cur = table;
    for p in parents {
        let entry = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        cur = entry
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("cannot set '{key}': '{p}' is not a table"))?;
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
        check!(
            lookup(&table, "impact.test_globs")
                .unwrap()
                .as_array()
                .unwrap()
                .len()
                == 1
        );

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
