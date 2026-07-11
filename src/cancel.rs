//! Ctrl-C → backend query cancellation.
//!
//! A single SIGINT handler is installed lazily on first arm. While a query is
//! running (the handler is "armed") Ctrl-C cancels the in-flight statement
//! instead of killing nsql; at any other time the handler preserves the
//! default behaviour (exit 130). The handler is armed only around DB
//! execution — never during compose (PHASE3 Tier 0).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

type CancelFn = Box<dyn Fn() + Send>;

static ARMED: Mutex<Option<CancelFn>> = Mutex::new(None);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static HANDLER: OnceLock<()> = OnceLock::new();

fn install_handler() {
    HANDLER.get_or_init(|| {
        let _ = ctrlc::set_handler(|| {
            let armed = ARMED.lock().ok().and_then(|mut g| g.take());
            match armed {
                Some(cancel) => {
                    INTERRUPTED.store(true, Ordering::SeqCst);
                    eprintln!("\nnsql: cancelling query…");
                    cancel();
                }
                None => std::process::exit(130),
            }
        });
    });
}

/// Arm cancellation for the duration of the returned guard. Does NOT clear a
/// pending interrupt — a Ctrl-C that lands between two armed windows must not
/// be swallowed. Callers clear explicitly with `reset()` at the start of a
/// logical operation or loop.
pub fn arm(cancel: impl Fn() + Send + 'static) -> Guard {
    install_handler();
    if let Ok(mut g) = ARMED.lock() {
        *g = Some(Box::new(cancel));
    }
    Guard
}

/// Clear the interrupted flag before a new operation/loop.
pub fn reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

/// True if Ctrl-C fired while armed since the last `reset()`. Loop drivers
/// (--watch, --repeat) use this to stop iterating.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut g) = ARMED.lock() {
            *g = None;
        }
    }
}
