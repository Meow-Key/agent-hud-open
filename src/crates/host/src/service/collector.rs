//! The collector worker: collection passes, requests to publish, hooks'
//! reminders and the stat-only looks between them, one at a time under the
//! collection gate.
use super::fair::Fair;
use super::{DATA, REQUEST};
use crate::{backend::Backend, presentation};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, PoisonError,
};
use std::time::{Duration, Instant};
use tauri::Manager;

pub(super) fn run(
    app: tauri::AppHandle,
    worker_backend: Arc<Fair<Backend>>,
    worker_stopped: Arc<AtomicBool>,
    worker_wake: Arc<(Mutex<u8>, Condvar)>,
    worker_collection: agent_hud_runtime::CollectionGate,
) {
    // A pass reads what changed. Between passes, a stat-only look
    // every LOOK checks the logs clients hold open, whose writes
    // can reach the watcher late, and sends the windows the time.
    const LOOK: Duration = Duration::from_secs(3);
    enum Turn {
        Pass,
        Request,
        Reminder,
        Look,
    }
    // A pass, even a slow walk of every log folder, is always followed by a
    // rest measured from when it ended, so passes never run back to back.
    // The first pass is now.
    let mut rest = Rest::default();
    let mut rest_until = Instant::now();
    // A look found a pass due while the collector was resting.
    let mut look_due = false;
    let mut look_at = Instant::now() + LOOK;
    let mut reminder_at: Option<Instant> = None;
    // When the snapshot falls due with nothing changing.
    let mut due_at: Option<Instant> = None;
    // When a pass is due with no event: the walk of every log
    // folder, or a failed read's retry. The first pass is now.
    let mut pass_at = Some(Instant::now());
    // While logs are still being indexed they are read again every
    // 2 s without waiting for a file event (UsageCollector's
    // indexing interval).
    let mut indexing = false;
    while !worker_stopped.load(Ordering::Acquire) {
        let mut changed = worker_wake.0.lock().unwrap_or_else(PoisonError::into_inner);
        let turn = loop {
            if worker_stopped.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            let reading = *changed & DATA != 0 || indexing || look_due;
            // Due with no event, but never before the rest ends.
            let pass_due = pass_at.map(|at| at.max(rest_until));
            if (reading && rest_until <= now) || pass_due.is_some_and(|at| at <= now) {
                break Turn::Pass;
            }
            if *changed & REQUEST != 0 || due_at.is_some_and(|at| at <= now) {
                break Turn::Request;
            }
            if reminder_at.is_some_and(|at| at <= now) {
                break Turn::Reminder;
            }
            if look_at <= now {
                break Turn::Look;
            }
            let next = [
                Some(look_at),
                reading.then_some(rest_until),
                reminder_at,
                due_at,
                pass_due,
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(look_at);
            (changed, _) = worker_wake
                .1
                .wait_timeout(changed, next.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner);
        };
        match turn {
            Turn::Pass => {
                *changed = 0;
                look_due = false;
            }
            Turn::Request => *changed &= !REQUEST,
            Turn::Reminder | Turn::Look => {}
        }
        drop(changed);
        let _turn = worker_collection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut backend = worker_backend
            .lock_last()
            .unwrap_or_else(PoisonError::into_inner);
        let method = match turn {
            Turn::Pass => Some("snapshot"),
            Turn::Request => Some("request_snapshot"),
            // A hook's follow-up: read only when its log changed.
            Turn::Reminder => backend
                .runtime
                .reminder_look()
                .then_some("request_snapshot"),
            Turn::Look => {
                let due = backend.runtime.look();
                if due && Instant::now() >= rest_until {
                    Some("snapshot")
                } else {
                    // A pass a look finds due while resting waits for
                    // the rest to end.
                    look_due |= due;
                    // Nothing changed: the windows and the menu
                    // bar get only the time.
                    backend.keep_snapshot(&app);
                    None
                }
            }
        };
        if let Some(method) = method {
            // Core is asked for changes only while the display
            // preferences the snapshot was projected with stand.
            let projected = Some(app.state::<presentation::Presentation>().revision());
            let changes_only = backend.snapshot.is_some() && backend.projected == projected;
            let _ = backend.request(
                &app,
                method,
                serde_json::json!({ "changesOnly": changes_only }),
            );
            if method == "snapshot" {
                // Measured from the pass's end, not its start: a walk slower
                // than the spacing used to be due again the moment it ended
                // and kept a core busy (about 120% CPU, 2026-11-16).
                rest_until = Instant::now() + rest.after_pass(backend.runtime.waiting_reads());
            }
        }
        // Only a pass or a look checks the logs clients hold open.
        // Requests and hooks' reminders arrive every few seconds
        // while other sessions run, and must not keep postponing
        // it (a VS Code Codex rollout waited 23 s, 2026-10-26).
        if method == Some("snapshot") || matches!(turn, Turn::Look) {
            look_at = Instant::now() + LOOK;
        }
        reminder_at = backend.runtime.next_log_reminder();
        due_at = backend.runtime.next_snapshot_due();
        pass_at = backend.runtime.next_pass_due();
        indexing = backend.runtime.indexing();
    }
}

/// The least rest after a pass: file events and account readings are read at
/// most this often (UsageCollector's 2 s read spacing).
const SPACING: Duration = Duration::from_secs(2);
/// The longest rest, after passes that keep leaving the same logs waiting.
const MAX_REST: Duration = Duration::from_secs(30);

/// How long the collector rests after each pass. While logs wait, a pass that
/// leaves no fewer waiting than the one before doubles the rest, up to
/// MAX_REST; one that shrinks them, or leaves none, goes back to SPACING.
#[derive(Debug)]
struct Rest {
    gap: Duration,
    waiting: Option<usize>,
}

impl Default for Rest {
    fn default() -> Self {
        Self {
            gap: SPACING,
            waiting: None,
        }
    }
}

impl Rest {
    /// The rest after a pass that left `waiting` logs to read.
    fn after_pass(&mut self, waiting: usize) -> Duration {
        self.gap = match self.waiting {
            Some(before) if waiting > 0 && waiting >= before => (self.gap * 2).min(MAX_REST),
            _ => SPACING,
        };
        self.waiting = (waiting > 0).then_some(waiting);
        self.gap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_busy_pass_keeps_the_minimum_gap() {
        let mut rest = Rest::default();
        assert_eq!(rest.after_pass(500), SPACING);
    }

    #[test]
    fn a_pass_that_does_not_shrink_pending_backs_off() {
        let mut rest = Rest::default();
        rest.after_pass(500);
        assert_eq!(rest.after_pass(500), Duration::from_secs(4));
        assert_eq!(rest.after_pass(600), Duration::from_secs(8));
        assert_eq!(rest.after_pass(600), Duration::from_secs(16));
        assert_eq!(rest.after_pass(600), MAX_REST);
        assert_eq!(rest.after_pass(600), MAX_REST);
    }

    #[test]
    fn a_pass_that_shrinks_pending_resets_the_gap() {
        let mut rest = Rest::default();
        rest.after_pass(500);
        rest.after_pass(500);
        rest.after_pass(500);
        assert_eq!(rest.after_pass(499), SPACING);
        rest.after_pass(499);
        assert_eq!(rest.after_pass(0), SPACING);
        assert_eq!(rest.after_pass(0), SPACING);
        assert_eq!(rest.after_pass(10), SPACING);
    }
}
