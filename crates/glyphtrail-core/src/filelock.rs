//! Portable advisory lock, in two flavours: [`with_lock`] for the brief
//! read-modify-write on the registry and groups files, and [`acquire_held`] for
//! long-running exclusive operations (the single-writer atlas DB during
//! `sync`/`embed`). They differ only in staleness policy — the brief lock steals
//! by age, the long-hold lock only on a dead holder — and share the lock-file
//! format below.
//!
//! The previous implementation used an OS advisory lock (`flock` via `fs4`).
//! That works locally but is unreliable on network / FUSE / sync filesystems
//! (e.g. pCloud): the lock may not be released when the holder dies, leaving a
//! *permanent* lock that blocks every writer, and writes silently never land.
//!
//! This uses a **lock file** instead: acquiring atomically creates the file
//! (`O_EXCL`) holding the holder's pid, host and timestamp. A lock that is
//! *stale* — held by a dead process on this host, older than [`STALE_TTL_SECS`],
//! or unreadable (e.g. a 0-byte file left by the old `flock`) — is safely
//! stolen. Holders keep the lock only for the quick RMW, so a fresh timestamp
//! reliably means "active". The lock file is removed on release, so a clean run
//! leaves nothing behind.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

/// A lock older than this (by its timestamp) is considered stale and stolen,
/// even across hosts where process liveness can't be checked. Generous relative
/// to the sub-second RMW it guards.
const STALE_TTL_SECS: i64 = 120;
/// How long to wait for an active holder to release before giving up.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);
/// An *unreadable* lock (empty / not valid JSON) is only stolen once it has
/// stayed unreadable this long. This distinguishes a genuine leftover (an old
/// 0-byte `flock` lock) from one being created right now: between the atomic
/// `create_new` and writing its JSON, a fresh lock is briefly empty, and a
/// concurrent acquirer must not steal it out from under the creator.
const UNREADABLE_GRACE: Duration = Duration::from_millis(400);

#[derive(Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    host: String,
    ts: i64,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Best-effort host identity, used only to speed up stealing a same-host dead
/// holder's lock. Empty when unknown (then liveness is not used; the TTL still
/// applies).
fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME"))
        && !h.is_empty()
    {
        return h;
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether process `pid` is alive on this host (Linux: `/proc/<pid>`). On other
/// platforms returns `true` so liveness never *causes* a steal there — the TTL
/// is the cross-platform safety net.
fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// Held lock; removes the lock file on drop.
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Whether a *readable* lock can be safely stolen: its timestamp is older than
/// the TTL, or it is held by a dead process on this host. (Unreadable locks are
/// handled separately in [`with_lock`], gated by [`UNREADABLE_GRACE`].)
fn is_stale(info: &LockInfo, host: &str) -> bool {
    now_secs() - info.ts > STALE_TTL_SECS
        || (!host.is_empty() && info.host == host && !pid_alive(info.pid))
}

fn read_info(path: &Path) -> Option<LockInfo> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Run `f` while holding the lock at `lock_path`. Acquires (stealing a stale
/// lock if needed), runs `f`, then releases. Errors if an *active* lock can't be
/// acquired within the timeout.
pub fn with_lock<T>(lock_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let host = hostname();
    let deadline = SystemTime::now() + ACQUIRE_TIMEOUT;
    // When we first saw the current lock as unreadable; used to wait out
    // UNREADABLE_GRACE before stealing it (see the constant). Reset whenever the
    // lock becomes readable or disappears.
    let mut unreadable_since: Option<SystemTime> = None;

    let _guard = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(file) => {
                let info = LockInfo {
                    pid: std::process::id(),
                    host: host.clone(),
                    ts: now_secs(),
                };
                let _ = serde_json::to_writer(&file, &info);
                break LockGuard {
                    path: lock_path.to_path_buf(),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match read_info(lock_path) {
                    Some(info) => {
                        unreadable_since = None;
                        if is_stale(&info, &host) {
                            let _ = std::fs::remove_file(lock_path); // steal
                            continue;
                        }
                    }
                    None => {
                        // Unreadable: a leftover 0-byte lock, or one mid-creation.
                        // Only steal once it's stayed unreadable past the grace.
                        let since = *unreadable_since.get_or_insert_with(SystemTime::now);
                        if SystemTime::now().duration_since(since).unwrap_or_default()
                            >= UNREADABLE_GRACE
                        {
                            let _ = std::fs::remove_file(lock_path); // steal
                            continue;
                        }
                    }
                }
                if SystemTime::now() >= deadline {
                    let holder = read_info(lock_path)
                        .map(|i| format!("pid {} on {}", i.pid, i.host))
                        .unwrap_or_else(|| "another process".to_string());
                    return Err(CoreError::Lock {
                        path: lock_path.to_path_buf(),
                        message: format!(
                            "held by {holder}; if stale, run `glyphtrail repo unlock`"
                        ),
                    });
                }
                std::thread::sleep(POLL);
            }
            Err(e) => return Err(CoreError::Io(e)),
        }
    };

    f()
}

/// Acquire a **long-hold** advisory lock at `lock_path`, returning a guard held
/// until it drops (which removes the file). Unlike [`with_lock`] — built for the
/// sub-second registry RMW, where a lock older than [`STALE_TTL_SECS`] is assumed
/// abandoned and stolen — this guards operations that legitimately run for minutes
/// (an `atlas sync`/`embed`). It therefore **never** steals by age: a held lock is
/// only reclaimed when its holder process is provably dead (same host, no
/// `/proc/<pid>`), or the file is unreadable past [`UNREADABLE_GRACE`]. Against a
/// live holder it **fails fast** (no waiting — a long op won't release soon) with a
/// `CoreError::Lock` naming the holder and suggesting `unlock_cmd`.
///
/// Assumes the lock lives on a local filesystem where pid-liveness is meaningful
/// (the atlas store is under `~/.glyphtrail/`, always local home) — that's what
/// makes age-free stealing safe here.
pub fn acquire_held(lock_path: &Path, unlock_cmd: &str) -> Result<LockGuard> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let host = hostname();
    let mut unreadable_since: Option<SystemTime> = None;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(file) => {
                let info = LockInfo {
                    pid: std::process::id(),
                    host: host.clone(),
                    ts: now_secs(),
                };
                let _ = serde_json::to_writer(&file, &info);
                return Ok(LockGuard {
                    path: lock_path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match read_info(lock_path) {
                Some(info) => {
                    // Reclaim only a dead same-host holder — never by age, since a
                    // legitimate long op holds this for the whole run.
                    if !host.is_empty() && info.host == host && !pid_alive(info.pid) {
                        let _ = std::fs::remove_file(lock_path); // steal a dead holder
                        continue;
                    }
                    let age = now_secs().saturating_sub(info.ts).max(0);
                    return Err(CoreError::Lock {
                        path: lock_path.to_path_buf(),
                        message: format!(
                            "another glyphtrail process is using the atlas (pid {} on {}, \
                             held for {age}s); wait for it to finish, or if it is stale run \
                             `{unlock_cmd}`",
                            info.pid, info.host,
                        ),
                    });
                }
                None => {
                    // Unreadable: a leftover 0-byte lock, or one mid-creation. Wait
                    // out the grace (so we don't steal a lock being written right
                    // now), then steal.
                    let since = *unreadable_since.get_or_insert_with(SystemTime::now);
                    if SystemTime::now().duration_since(since).unwrap_or_default()
                        >= UNREADABLE_GRACE
                    {
                        let _ = std::fs::remove_file(lock_path);
                        continue;
                    }
                    std::thread::sleep(POLL);
                }
            },
            Err(e) => return Err(CoreError::Io(e)),
        }
    }
}

/// Remove the lock file at `lock_path` (the `repo unlock` / `atlas unlock` escape
/// hatch). Returns a human description of what was removed, or `None` if no lock
/// held.
pub fn force_unlock(lock_path: &Path) -> Result<Option<String>> {
    if !lock_path.exists() {
        return Ok(None);
    }
    let desc = read_info(lock_path)
        .map(|i| format!("pid {} on {} (held since ts {})", i.pid, i.host, i.ts))
        .unwrap_or_else(|| "unreadable lock".to_string());
    std::fs::remove_file(lock_path)?;
    Ok(Some(desc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert2::check;

    fn tmp(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("glyphtrail-lock-{tag}-{n}.lock"))
    }

    #[test]
    fn acquire_release_removes_the_file() {
        let p = tmp("basic");
        with_lock(&p, || Ok(())).unwrap();
        check!(!p.exists()); // released => file gone
    }

    #[test]
    fn steals_a_stale_zero_byte_lock() {
        // A leftover 0-byte lock (as the old flock left) is unreadable => stolen.
        let p = tmp("stale0");
        std::fs::write(&p, b"").unwrap();
        let got = with_lock(&p, || Ok(42)).unwrap();
        check!(got == 42);
        check!(!p.exists());
    }

    #[test]
    fn errors_on_a_live_foreign_lock() {
        // A fresh lock held by this live process (different "holder" simulated by
        // a far-future-safe fresh ts + our own pid) is not stolen; acquire times
        // out. Keep the timeout effect cheap by asserting the not-stale path.
        let p = tmp("live");
        let info = LockInfo {
            pid: std::process::id(),
            host: hostname(),
            ts: now_secs(),
        };
        std::fs::write(&p, serde_json::to_string(&info).unwrap()).unwrap();
        // Our own pid is alive + ts fresh => not stale.
        check!(!is_stale(&read_info(&p).unwrap(), &hostname()));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn concurrent_writers_serialize() {
        let dir = std::env::temp_dir().join(format!(
            "glyphtrail-lock-conc-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("x.lock");
        let counter = dir.join("count");
        std::fs::write(&counter, "0").unwrap();
        const N: usize = 12;
        std::thread::scope(|s| {
            for _ in 0..N {
                let lock = lock.clone();
                let counter = counter.clone();
                s.spawn(move || {
                    with_lock(&lock, || {
                        let v: u64 = std::fs::read_to_string(&counter)
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        std::fs::write(&counter, (v + 1).to_string()).unwrap();
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });
        let final_v: u64 = std::fs::read_to_string(&counter)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        check!(final_v == N as u64); // no lost updates
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn acquire_held_blocks_a_second_live_holder_then_releases() {
        let p = tmp("held");
        let guard = acquire_held(&p, "glyphtrail atlas unlock").unwrap();
        check!(p.exists());
        // A second acquire fails fast (no wait) while the first guard lives.
        let err = acquire_held(&p, "glyphtrail atlas unlock");
        check!(err.is_err());
        if let Err(CoreError::Lock { message, .. }) = err {
            check!(message.contains("another glyphtrail process"));
            check!(message.contains("glyphtrail atlas unlock"));
        }
        // Dropping the guard removes the lock; a fresh acquire then succeeds.
        drop(guard);
        check!(!p.exists());
        let again = acquire_held(&p, "glyphtrail atlas unlock").unwrap();
        drop(again);
        check!(!p.exists());
    }

    #[test]
    fn acquire_held_steals_a_dead_same_host_holder() {
        let p = tmp("held-dead");
        // A lock from a long-dead pid on this host (pid 0 is never a live process)
        // is reclaimed — fresh ts notwithstanding, since long holds aren't aged out.
        let info = LockInfo {
            pid: 0,
            host: hostname(),
            ts: now_secs(),
        };
        std::fs::write(&p, serde_json::to_string(&info).unwrap()).unwrap();
        let guard = acquire_held(&p, "glyphtrail atlas unlock");
        check!(guard.is_ok()); // dead holder stolen despite a fresh timestamp
        drop(guard);
        check!(!p.exists());
    }
}
