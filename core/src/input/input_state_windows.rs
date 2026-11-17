//! fork (Windows only): why remote input is refused or ignored.
//!
//! Windows only lets a normal program inject mouse and keyboard input into the
//! desktop the user is working on, and only into windows of the same or a lower
//! privilege level:
//!
//! * While the screen is **locked**, or a **secure desktop** is in front (sign-in
//!   screen, Ctrl+Alt+Del screen, UAC "Do you want to allow..." prompt), `SendInput`
//!   fails for every event (error 5, access denied). Screen capture cannot read
//!   that desktop either, so viewers keep seeing a picture while nothing reacts.
//! * While a window of a program **running as administrator** is the target,
//!   `SendInput` *succeeds* but Windows drops the input (UIPI). No error is
//!   returned, so the only way to notice is to look at the target window.
//!
//! This module looks at both: a watcher thread polls the input desktop once a
//! second (one cheap system call), writes every change into the log and hands the
//! state to `diagnostics`, which tells the people in the call.

use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HWND, POINT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW,
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, WindowFromPoint,
};

use super::diagnostics;

/// How often the watcher looks at the input desktop.
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// The target window's privilege level is looked up at most this often.
const TARGET_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// `DESKTOP_READOBJECTS`: the least access that proves the desktop can be opened.
const DESKTOP_READOBJECTS: u32 = 0x0001;
/// `WTSINFOEX_LEVEL1.SessionFlags` values.
const WTS_SESSIONSTATE_LOCK: i32 = 0;
const WTS_SESSIONSTATE_UNLOCK: i32 = 1;
/// Mandatory integrity levels (the last sub-authority of the token's label SID).
const INTEGRITY_LOW: u32 = 0x1000;
const INTEGRITY_MEDIUM: u32 = 0x2000;
const INTEGRITY_HIGH: u32 = 0x3000;
const INTEGRITY_SYSTEM: u32 = 0x4000;

/// The desktop that currently receives keyboard and mouse input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDesktop {
    /// It could be opened; this is its name ("Default" is the normal desktop).
    Open(String),
    /// It could not be opened (the Win32 error code). For a program that is not
    /// running as SYSTEM this is what the lock screen and UAC prompts look like.
    NotAccessible(u32),
}

/// Asks Windows which desktop currently receives input.
pub fn input_desktop() -> InputDesktop {
    unsafe {
        let desktop = match OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS),
        ) {
            Ok(desktop) => desktop,
            Err(_) => return InputDesktop::NotAccessible(GetLastError().0),
        };
        let mut name = [0u16; 128];
        let mut needed = 0u32;
        let result = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(desktop);
        match result {
            Ok(()) => {
                let length = name
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(name.len());
                InputDesktop::Open(String::from_utf16_lossy(&name[..length]))
            }
            Err(_) => InputDesktop::Open("?".to_string()),
        }
    }
}

/// Whether this Windows session is locked. `None` when Windows does not say.
pub fn session_locked() -> Option<bool> {
    unsafe {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut bytes,
        )
        .ok()?;
        if buffer.is_null() {
            return None;
        }
        let locked = if (bytes as usize) >= std::mem::size_of::<WTSINFOEXW>() {
            let info = std::ptr::read_unaligned(buffer.0 as *const WTSINFOEXW);
            if info.Level == 1 {
                match info.Data.WTSInfoExLevel1.SessionFlags {
                    WTS_SESSIONSTATE_LOCK => Some(true),
                    WTS_SESSIONSTATE_UNLOCK => Some(false),
                    _ => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        WTSFreeMemory(buffer.0.cast());
        locked
    }
}

/// A plain-language reason when nothing can be injected right now, `None` on the
/// normal desktop.
pub fn block_reason(desktop: &InputDesktop, locked: Option<bool>) -> Option<String> {
    match desktop {
        InputDesktop::Open(name) if name.eq_ignore_ascii_case("Default") => None,
        _ if locked == Some(true) => Some("the screen is locked".to_string()),
        InputDesktop::Open(name) if name.eq_ignore_ascii_case("Screen-saver") => {
            Some("the screen saver is running".to_string())
        }
        InputDesktop::Open(name) => Some(format!("Windows switched to its \"{name}\" screen")),
        InputDesktop::NotAccessible(_) => Some(
            "a Windows security screen is in front (sign-in, Ctrl+Alt+Del or an administrator prompt)"
                .to_string(),
        ),
    }
}

fn describe_desktop(desktop: &InputDesktop) -> String {
    match desktop {
        InputDesktop::Open(name) => format!("\"{name}\""),
        InputDesktop::NotAccessible(error) => {
            format!("not accessible, {}", describe_error(*error))
        }
    }
}

fn describe_locked(locked: Option<bool>) -> &'static str {
    match locked {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// A Win32 error code with the names of the ones that matter here.
pub fn describe_error(error: u32) -> String {
    let name = match error {
        0 => " (no error code: blocked by another program, or by a window with higher privileges)",
        5 => " (access denied: the input desktop is not the normal desktop)",
        87 => " (invalid parameter)",
        170 => " (resource busy)",
        1459 => " (requires an interactive window station)",
        _ => "",
    };
    format!("error {error}{name}")
}

fn integrity_name(level: u32) -> &'static str {
    match level {
        level if level >= INTEGRITY_SYSTEM => "System",
        level if level >= INTEGRITY_HIGH => "High (administrator)",
        level if level > INTEGRITY_MEDIUM => "Medium+",
        level if level >= INTEGRITY_MEDIUM => "Medium (normal)",
        level if level >= INTEGRITY_LOW => "Low",
        _ => "Untrusted",
    }
}

/// The integrity level of the process behind `process` (a handle with query access).
unsafe fn process_integrity(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    // A TOKEN_MANDATORY_LABEL followed by its SID; u64 storage keeps it aligned.
    let mut buffer = [0u64; 16];
    let mut length = 0u32;
    let result = GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buffer.as_mut_ptr().cast()),
        std::mem::size_of_val(&buffer) as u32,
        &mut length,
    );
    let _ = CloseHandle(token);
    result.ok()?;
    let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    if sid.is_invalid() {
        return None;
    }
    let count = *GetSidSubAuthorityCount(sid);
    if count == 0 {
        return None;
    }
    Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
}

/// This process's own integrity level (looked up once).
fn own_integrity() -> Option<u32> {
    static OWN: OnceLock<Option<u32>> = OnceLock::new();
    *OWN.get_or_init(|| unsafe { process_integrity(GetCurrentProcess()) })
}

/// The program a window belongs to and its privilege level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOwner {
    /// File name of the program ("Taskmgr.exe"), or "process <id>" when unknown.
    pub program: String,
    /// `None` when Windows did not let us look, which for a window of the same
    /// user means the program is protected or belongs to the system.
    pub integrity: Option<u32>,
}

/// Who owns `window`. `None` for no window.
pub fn window_owner(window: HWND) -> Option<WindowOwner> {
    if window.0.is_null() {
        return None;
    }
    unsafe {
        let mut process_id = 0u32;
        GetWindowThreadProcessId(window, Some(&mut process_id));
        if process_id == 0 {
            return None;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) else {
            return Some(WindowOwner {
                program: format!("process {process_id}"),
                integrity: None,
            });
        };
        let mut path = [0u16; 512];
        let mut length = path.len() as u32;
        let program = match QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut length,
        ) {
            Ok(()) => {
                let full = String::from_utf16_lossy(&path[..length as usize]);
                // Only the file name: no folder names (user name) in the log.
                full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
            }
            Err(_) => format!("process {process_id}"),
        };
        let integrity = process_integrity(process);
        let _ = CloseHandle(process);
        Some(WindowOwner { program, integrity })
    }
}

/// True when input aimed at a window of `owner` is dropped by Windows because
/// the program runs with higher privileges than this process (UIPI).
pub fn is_above(owner: &WindowOwner, own: Option<u32>) -> bool {
    match (owner.integrity, own) {
        (Some(theirs), Some(ours)) => theirs > ours,
        // Windows would not even let us look: protected or system process.
        (None, _) => true,
        (Some(_), None) => false,
    }
}

fn describe_owner(owner: Option<&WindowOwner>) -> String {
    match owner {
        None => "none".to_string(),
        Some(owner) => match owner.integrity {
            Some(level) => format!(
                "{} (privilege level: {})",
                owner.program,
                integrity_name(level)
            ),
            None => format!(
                "{} (privilege level: not readable, protected or system)",
                owner.program
            ),
        },
    }
}

/// Everything known about why an injection just failed, for the log line.
pub fn failure_context(last_error: u32) -> String {
    let desktop = input_desktop();
    let locked = session_locked();
    let foreground = window_owner(unsafe { GetForegroundWindow() });
    format!(
        "{}; input desktop: {}; session locked: {}; window in front: {}; Hopp privilege level: {}",
        describe_error(last_error),
        describe_desktop(&desktop),
        describe_locked(locked),
        describe_owner(foreground.as_ref()),
        own_integrity().map(integrity_name).unwrap_or("unknown"),
    )
}

/// Rate limiter shared by the two target checks below.
fn target_check_due() -> bool {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    if last.is_some_and(|last| now.saturating_duration_since(last) < TARGET_CHECK_INTERVAL) {
        return false;
    }
    *last = Some(now);
    true
}

fn report_if_above(window: HWND) {
    if let Some(owner) = window_owner(window) {
        if is_above(&owner, own_integrity()) {
            diagnostics::elevated_window_in_front(&owner.program);
        }
    }
}

/// Call after a remote click was accepted: looks (at most once a second) whether
/// the window under the cursor silently ignores it.
pub fn check_click_target() {
    if !target_check_due() {
        return;
    }
    unsafe {
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_ok() {
            report_if_above(WindowFromPoint(point));
        }
    }
}

/// Call after a remote key press was accepted: looks (at most once a second)
/// whether the window that has the keyboard silently ignores it.
pub fn check_key_target() {
    if !target_check_due() {
        return;
    }
    report_if_above(unsafe { GetForegroundWindow() });
}

/// Where the mouse cursor is right now, in screen pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

fn watch() {
    let mut last_reason: Option<Option<String>> = None;
    loop {
        let desktop = input_desktop();
        let normal =
            matches!(&desktop, InputDesktop::Open(name) if name.eq_ignore_ascii_case("Default"));
        // The lock state is only asked for when it matters or changes the wording.
        let locked = if normal {
            Some(false)
        } else {
            session_locked()
        };
        let reason = block_reason(&desktop, locked);
        if last_reason.as_ref() != Some(&reason) {
            match (&reason, last_reason.is_some()) {
                (Some(reason), _) => log::warn!(
                    "input_state: remote control is NOT possible right now: {reason} \
                     (input desktop: {}; session locked: {}). Windows refuses injected mouse and \
                     keyboard input until the normal desktop is back.",
                    describe_desktop(&desktop),
                    describe_locked(locked),
                ),
                (None, true) => log::info!(
                    "input_state: the normal desktop is back, remote control is possible again"
                ),
                (None, false) => log::info!(
                    "input_state: watching the input desktop (now: {}; Hopp privilege level: {})",
                    describe_desktop(&desktop),
                    own_integrity().map(integrity_name).unwrap_or("unknown"),
                ),
            }
            diagnostics::set_blocked(reason.clone());
            last_reason = Some(reason);
        }
        diagnostics::tick();
        std::thread::sleep(WATCH_INTERVAL);
    }
}

/// Starts the watcher thread (once per process). It runs for the lifetime of
/// hopp_core, so the log also shows locks that happen while nobody is controlling.
pub fn start_watcher() {
    static START: Once = Once::new();
    START.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("input-state".to_string())
            .spawn(watch);
        if let Err(e) = spawned {
            log::warn!("input_state: cannot start the watcher thread: {e:?}");
        }
    });
}
