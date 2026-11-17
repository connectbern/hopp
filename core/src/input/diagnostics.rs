//! fork: rate-limited diagnostics for remote input (mouse + keyboard injection).
//!
//! Why this exists: when a teammate "could see the screen but not control it", the
//! log either drowned in hundreds of identical `SendInput failed` lines without the
//! reason, or (when Windows accepted the input but nothing reacted) contained
//! nothing at all, because the success path only logs at debug level. This module
//! keeps counters and writes
//!
//! * one summary line every few seconds while remote input is arriving
//!   (`remote_input: last 5 s: received ... | passed on ... | refused ...`), so a log
//!   can tell "nothing arrived" from "arrived but refused" from "accepted but
//!   nothing reacted";
//! * refusals as one line per second with a count instead of one line per event.
//!
//! Privacy: nothing here ever logs typed characters. Only counts, and the *names* of
//! non-printing keys (e.g. "CapsLock") that were ignored.
//!
//! The module is platform independent and has no dependencies besides `log`, so
//! its logic is unit tested on any OS.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A summary line is written at most this often while remote input is arriving.
pub const SUMMARY_INTERVAL: Duration = Duration::from_secs(5);
/// Repeated failures are folded into one line per this interval.
pub const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(1);
/// While input is blocked, controllers that keep trying are told again this often
/// (covers a teammate who joined after the block started).
pub const BLOCK_REMINDER_INTERVAL: Duration = Duration::from_secs(3);
/// The "an administrator window is in front" notice is repeated at most this often.
pub const ELEVATED_NOTICE_INTERVAL: Duration = Duration::from_secs(30);

/// Something the people in the call should be told about remote input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputNotice {
    /// Nothing can be injected right now (lock screen, sign-in screen, UAC prompt).
    /// The text is a plain-language reason. Sent when the state changes and when
    /// a share starts in this state.
    Blocked(String),
    /// A controller tried to use the mouse or keyboard while input is blocked.
    /// `first` is true for the first attempt of each blocked period.
    BlockedAttempt { reason: String, first: bool },
    /// Injection is possible again.
    Unblocked,
    /// Windows accepts the input but drops it, because the window it is aimed at
    /// runs with higher privileges (e.g. "Taskmgr.exe").
    ElevatedWindow(String),
}

/// What kind of remote event arrived from a controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    Move,
    Click,
    Scroll,
    Key,
}

/// Why a remote key event was not passed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIgnored {
    /// The key string was empty.
    Empty,
    /// CapsLock / NumLock / ScrollLock: deliberately not replayed (see `key_rules`).
    LockKey,
    /// A named, non-printing key this platform has no key code for.
    UnknownNamedKey,
    /// A shortcut (Ctrl/Alt/Cmd + character) whose character is not on this
    /// keyboard layout, so there is no key to press.
    ShortcutWithoutKey,
}

/// Counters for one summary window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub moves: u64,
    pub clicks: u64,
    pub scrolls: u64,
    pub keys: u64,
    pub mouse_accepted: u64,
    pub mouse_refused: u64,
    pub keys_accepted: u64,
    pub keys_refused: u64,
    pub keys_ignored: u64,
    /// How often the real cursor position was compared with the injected target...
    pub cursor_checks: u64,
    /// ...and how often the cursor was NOT where the remote move put it.
    pub cursor_misses: u64,
}

impl Tally {
    pub fn is_empty(&self) -> bool {
        *self == Tally::default()
    }

    /// The summary line (without the log prefix).
    pub fn summary(&self, seconds: u64, controller: Option<&str>) -> String {
        let mut line = format!(
            "remote_input: last {seconds} s: received {} moves, {} clicks, {} scrolls, {} key events from {}",
            self.moves,
            self.clicks,
            self.scrolls,
            self.keys,
            controller.unwrap_or("?"),
        );
        line.push_str(&format!(
            " | passed on and accepted by the system: {} mouse, {} key events",
            self.mouse_accepted, self.keys_accepted
        ));
        line.push_str(&format!(
            " | REFUSED by the system: {} mouse, {} key events",
            self.mouse_refused, self.keys_refused
        ));
        if self.keys_ignored > 0 {
            line.push_str(&format!(" | {} key events ignored", self.keys_ignored));
        }
        if self.cursor_checks > 0 {
            line.push_str(&format!(
                " | cursor followed {} of {} checked moves",
                self.cursor_checks - self.cursor_misses,
                self.cursor_checks
            ));
            if self.cursor_misses == self.cursor_checks {
                line.push_str(
                    " (accepted but NOT applied: something else holds or moves the cursor)",
                );
            }
        }
        let arrived = self.moves + self.clicks + self.scrolls + self.keys;
        let passed = self.mouse_accepted + self.keys_accepted;
        let refused = self.mouse_refused + self.keys_refused;
        if arrived > 0 && passed == 0 && refused == 0 && self.keys_ignored == 0 {
            line.push_str(" (arrived but nothing was injected: the sender is not in control mode)");
        }
        line
    }
}

/// Collects a `Tally` and hands out a summary once the window is old enough.
#[derive(Debug)]
pub struct SummaryWindow {
    tally: Tally,
    started: Option<Instant>,
    controller: Option<String>,
}

impl SummaryWindow {
    pub const fn new() -> Self {
        Self {
            tally: Tally {
                moves: 0,
                clicks: 0,
                scrolls: 0,
                keys: 0,
                mouse_accepted: 0,
                mouse_refused: 0,
                keys_accepted: 0,
                keys_refused: 0,
                keys_ignored: 0,
                cursor_checks: 0,
                cursor_misses: 0,
            },
            started: None,
            controller: None,
        }
    }

    /// Adds to the current window (opening one if needed).
    pub fn record(
        &mut self,
        now: Instant,
        controller: Option<&str>,
        update: impl FnOnce(&mut Tally),
    ) {
        if self.started.is_none() {
            self.started = Some(now);
        }
        if let Some(controller) = controller {
            if self.controller.as_deref() != Some(controller) {
                self.controller = Some(controller.to_string());
            }
        }
        update(&mut self.tally);
    }

    /// Returns the summary line when the window has been open for `interval`.
    pub fn take_if_due(&mut self, now: Instant, interval: Duration) -> Option<String> {
        let started = self.started?;
        let age = now.saturating_duration_since(started);
        if age < interval {
            return None;
        }
        self.started = None;
        let tally = std::mem::take(&mut self.tally);
        if tally.is_empty() {
            return None;
        }
        Some(tally.summary(age.as_secs().max(1), self.controller.as_deref()))
    }
}

impl Default for SummaryWindow {
    fn default() -> Self {
        Self::new()
    }
}

/// Folds a burst of identical events into one log line per interval.
#[derive(Debug)]
pub struct Burst {
    last_log: Option<Instant>,
    pending: u64,
}

impl Burst {
    pub const fn new() -> Self {
        Self {
            last_log: None,
            pending: 0,
        }
    }

    /// Registers one event. Returns `Some(n)` when a line should be written now,
    /// `n` being the number of events that line stands for.
    pub fn hit(&mut self, now: Instant, interval: Duration) -> Option<u64> {
        self.pending += 1;
        match self.last_log {
            Some(last) if now.saturating_duration_since(last) < interval => None,
            _ => {
                self.last_log = Some(now);
                Some(std::mem::take(&mut self.pending))
            }
        }
    }

    /// Events counted since the last line, once the interval has passed (so the
    /// tail of a burst is not lost).
    pub fn flush(&mut self, now: Instant, interval: Duration) -> Option<u64> {
        if self.pending == 0 {
            return None;
        }
        match self.last_log {
            Some(last) if now.saturating_duration_since(last) < interval => None,
            _ => {
                self.last_log = Some(now);
                Some(std::mem::take(&mut self.pending))
            }
        }
    }
}

impl Burst {
    /// Events counted but not yet reported; resets the count.
    pub fn take_pending(&mut self) -> u64 {
        std::mem::take(&mut self.pending)
    }
}

impl Default for Burst {
    fn default() -> Self {
        Self::new()
    }
}

type Notifier = Box<dyn Fn(InputNotice) + Send>;

struct Global {
    summary: SummaryWindow,
    mouse_refused: Burst,
    keys_refused: Burst,
    keys_ignored: Burst,
    /// Plain-language reason while nothing can be injected.
    blocked: Option<String>,
    last_block_reminder: Option<Instant>,
    block_attempt_seen: bool,
    last_elevated_notice: Option<Instant>,
    notifier: Option<Notifier>,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    summary: SummaryWindow::new(),
    mouse_refused: Burst::new(),
    keys_refused: Burst::new(),
    keys_ignored: Burst::new(),
    blocked: None,
    last_block_reminder: None,
    block_attempt_seen: false,
    last_elevated_notice: None,
    notifier: None,
});

fn global() -> std::sync::MutexGuard<'static, Global> {
    GLOBAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn log_summary_if_due(global: &mut Global, now: Instant) {
    if let Some(line) = global.summary.take_if_due(now, SUMMARY_INTERVAL) {
        log::info!("{line}");
    }
}

/// Installs the callback that tells the people in the call about blocked input.
/// The callback must not call back into this module.
pub fn set_notifier(notifier: impl Fn(InputNotice) + Send + 'static) {
    let mut global = global();
    // A share that starts while the screen is already locked tells its viewers.
    if let Some(reason) = global.blocked.clone() {
        notifier(InputNotice::Blocked(reason));
    }
    global.notifier = Some(Box::new(notifier));
}

pub fn clear_notifier() {
    global().notifier = None;
}

/// A remote event arrived from `controller` (before any decision what to do with it).
pub fn received(kind: Received, controller: Option<&str>) {
    let now = Instant::now();
    let mut global = global();
    log_summary_if_due(&mut global, now);
    global.summary.record(now, controller, |tally| match kind {
        Received::Move => tally.moves += 1,
        Received::Click => tally.clicks += 1,
        Received::Scroll => tally.scrolls += 1,
        Received::Key => tally.keys += 1,
    });

    // Someone is trying to control a machine that cannot be controlled right now:
    // tell them (again), rate limited.
    if kind != Received::Move {
        if let Some(reason) = global.blocked.clone() {
            let due = global
                .last_block_reminder
                .is_none_or(|last| now.saturating_duration_since(last) >= BLOCK_REMINDER_INTERVAL);
            if due {
                global.last_block_reminder = Some(now);
                let first = !std::mem::replace(&mut global.block_attempt_seen, true);
                if let Some(notifier) = global.notifier.as_ref() {
                    notifier(InputNotice::BlockedAttempt { reason, first });
                }
            }
        }
    }
}

/// The system accepted an injected mouse event.
pub fn mouse_accepted() {
    let now = Instant::now();
    let mut global = global();
    global
        .summary
        .record(now, None, |tally| tally.mouse_accepted += 1);
}

/// The system accepted an injected key event.
pub fn key_accepted() {
    let now = Instant::now();
    let mut global = global();
    global
        .summary
        .record(now, None, |tally| tally.keys_accepted += 1);
}

/// The system refused an injected mouse event. Returns `Some(n)` when the caller
/// should write a log line now, standing for `n` refusals.
pub fn mouse_refused() -> Option<u64> {
    let now = Instant::now();
    let mut global = global();
    global
        .summary
        .record(now, None, |tally| tally.mouse_refused += 1);
    global.mouse_refused.hit(now, FAILURE_LOG_INTERVAL)
}

/// The system refused an injected key event. See [`mouse_refused`].
pub fn key_refused() -> Option<u64> {
    let now = Instant::now();
    let mut global = global();
    global
        .summary
        .record(now, None, |tally| tally.keys_refused += 1);
    global.keys_refused.hit(now, FAILURE_LOG_INTERVAL)
}

/// Result of comparing the real cursor position with the last injected move.
pub fn cursor_checked(followed: bool) {
    let now = Instant::now();
    let mut global = global();
    global.summary.record(now, None, |tally| {
        tally.cursor_checks += 1;
        if !followed {
            tally.cursor_misses += 1;
        }
    });
}

/// A remote key event was not passed on. `key_name` is logged only for named,
/// non-printing keys; for anything else only the category is logged, never the
/// character.
pub fn key_ignored(reason: KeyIgnored, key_name: &str) {
    let now = Instant::now();
    let mut global = global();
    global
        .summary
        .record(now, None, |tally| tally.keys_ignored += 1);
    if let Some(count) = global.keys_ignored.hit(now, FAILURE_LOG_INTERVAL) {
        let what = match reason {
            KeyIgnored::Empty => "an empty key".to_string(),
            KeyIgnored::LockKey => format!(
                "lock key {key_name} (not replayed: remote text already arrives with the right case)"
            ),
            KeyIgnored::UnknownNamedKey => format!("key {key_name} (no key code on this system)"),
            KeyIgnored::ShortcutWithoutKey => {
                "a shortcut whose character is not on this keyboard layout".to_string()
            }
        };
        log::info!("remote_input: ignored {what} ({count} ignored key events since the last line)");
    }
}

/// Sets or clears the "nothing can be injected" state and tells the call once per
/// change. `reason` is a plain-language sentence.
pub fn set_blocked(reason: Option<String>) {
    let mut global = global();
    if global.blocked == reason {
        return;
    }
    global.blocked = reason.clone();
    // The first attempt after the change is reported at once.
    global.last_block_reminder = None;
    global.block_attempt_seen = false;
    if let Some(notifier) = global.notifier.as_ref() {
        notifier(match reason {
            Some(reason) => InputNotice::Blocked(reason),
            None => InputNotice::Unblocked,
        });
    }
}

/// The plain-language reason while nothing can be injected (lock screen / secure
/// desktop), `None` when input is possible.
pub fn blocked_reason() -> Option<String> {
    global().blocked.clone()
}

/// Remote input is aimed at a window with higher privileges (`program`, e.g.
/// "Taskmgr.exe"). Tells the sharer, rate limited.
pub fn elevated_window_in_front(program: &str) {
    let now = Instant::now();
    let mut global = global();
    let due = global
        .last_elevated_notice
        .is_none_or(|last| now.saturating_duration_since(last) >= ELEVATED_NOTICE_INTERVAL);
    if !due {
        return;
    }
    global.last_elevated_notice = Some(now);
    log::warn!(
        "remote_input: the window receiving remote input belongs to {program}, which runs with \
         higher privileges than Hopp. Windows silently drops remote clicks and keys aimed at it \
         (no error is returned)."
    );
    if let Some(notifier) = global.notifier.as_ref() {
        notifier(InputNotice::ElevatedWindow(program.to_string()));
    }
}

/// Writes whatever is due. Called about once a second by the platform watcher (so
/// the tail of a burst and the last summary window are not left unwritten).
pub fn tick() {
    let now = Instant::now();
    let mut global = global();
    log_summary_if_due(&mut global, now);
    if let Some(count) = global.mouse_refused.flush(now, FAILURE_LOG_INTERVAL) {
        log::error!("send_input: SendInput failed x{count} more (mouse)");
    }
    if let Some(count) = global.keys_refused.flush(now, FAILURE_LOG_INTERVAL) {
        log::error!("keyboard_input: SendInput failed x{count} more");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_logs_first_event_then_one_line_per_interval() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut burst = Burst::new();
        // The first failure is reported at once.
        assert_eq!(burst.hit(start, second), Some(1));
        // 400 more within the same second: silent.
        for i in 0..400 {
            assert_eq!(burst.hit(start + Duration::from_millis(i), second), None);
        }
        // The next one after the interval reports the whole lot.
        assert_eq!(burst.hit(start + second, second), Some(401));
        // Nothing pending afterwards.
        assert_eq!(burst.flush(start + second * 3, second), None);
    }

    #[test]
    fn burst_flush_reports_the_tail() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut burst = Burst::new();
        assert_eq!(burst.hit(start, second), Some(1));
        assert_eq!(burst.hit(start + Duration::from_millis(10), second), None);
        assert_eq!(burst.hit(start + Duration::from_millis(20), second), None);
        // Too early: keep collecting.
        assert_eq!(
            burst.flush(start + Duration::from_millis(500), second),
            None
        );
        assert_eq!(burst.flush(start + second, second), Some(2));
        assert_eq!(burst.flush(start + second * 2, second), None);
    }

    #[test]
    fn summary_window_is_written_once_per_interval() {
        let start = Instant::now();
        let mut window = SummaryWindow::new();
        assert_eq!(window.take_if_due(start, SUMMARY_INTERVAL), None);
        window.record(start, Some("alice"), |t| t.moves += 3);
        window.record(start + Duration::from_secs(1), None, |t| {
            t.mouse_accepted += 3
        });
        assert_eq!(
            window.take_if_due(start + Duration::from_secs(4), SUMMARY_INTERVAL),
            None
        );
        let line = window
            .take_if_due(start + Duration::from_secs(5), SUMMARY_INTERVAL)
            .expect("summary due");
        assert!(line.contains("last 5 s"), "{line}");
        assert!(line.contains("received 3 moves"), "{line}");
        assert!(line.contains("from alice"), "{line}");
        assert!(line.contains("accepted by the system: 3 mouse"), "{line}");
        // The window is closed until something new arrives.
        assert_eq!(
            window.take_if_due(start + Duration::from_secs(60), SUMMARY_INTERVAL),
            None
        );
    }

    #[test]
    fn summary_tells_the_three_cases_apart() {
        // Arrived, nothing injected (sender not in control).
        let arrived_only = Tally {
            moves: 50,
            ..Default::default()
        };
        assert!(arrived_only
            .summary(5, Some("bob"))
            .contains("nothing was injected"));

        // Arrived and refused (lock screen).
        let refused = Tally {
            moves: 50,
            clicks: 2,
            mouse_refused: 52,
            ..Default::default()
        };
        let line = refused.summary(5, Some("bob"));
        assert!(line.contains("REFUSED by the system: 52 mouse"), "{line}");
        assert!(!line.contains("nothing was injected"), "{line}");

        // Accepted but the cursor never followed.
        let ignored = Tally {
            moves: 50,
            mouse_accepted: 50,
            cursor_checks: 10,
            cursor_misses: 10,
            ..Default::default()
        };
        let line = ignored.summary(5, Some("bob"));
        assert!(line.contains("cursor followed 0 of 10"), "{line}");
        assert!(line.contains("NOT applied"), "{line}");

        // Healthy.
        let healthy = Tally {
            moves: 50,
            mouse_accepted: 50,
            cursor_checks: 10,
            cursor_misses: 0,
            ..Default::default()
        };
        let line = healthy.summary(5, Some("bob"));
        assert!(line.contains("cursor followed 10 of 10"), "{line}");
        assert!(!line.contains("NOT applied"), "{line}");
    }

    #[test]
    fn summary_never_contains_typed_text() {
        // The summary is built from counters only; there is no API to put a key in.
        let tally = Tally {
            keys: 12,
            keys_accepted: 24,
            ..Default::default()
        };
        let line = tally.summary(5, Some("carol"));
        assert!(line.contains("12 key events"));
        assert!(line.contains("24 key events"));
    }
}
