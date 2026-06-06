//! Cooperative CTRL-C handling for graceful shutdown.
//!
//! A first SIGINT sets a flag instead of killing the process, so long operations
//! (notably `atlas sync`, which writes to LadybugDB) can stop at a safe point —
//! after the current transaction commits — leaving the database clean and saving
//! incremental progress. A second SIGINT aborts immediately as an escape hatch.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Install the CTRL-C handler. First press requests a graceful stop; a second press
/// aborts now. Best-effort: a failure to install just leaves the default behaviour.
pub fn install() {
    let _ = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            // Already asked once — the user wants out now.
            eprintln!("\naborting (the database recovers from its WAL on next open)");
            std::process::exit(130);
        }
        eprintln!("\nstopping at the next safe point — press CTRL-C again to abort immediately");
    });
}

/// Whether a graceful stop has been requested (a SIGINT was received). Long loops
/// should check this at transaction boundaries and stop cleanly when it's set.
pub fn requested() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}
