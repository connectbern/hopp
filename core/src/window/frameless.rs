//! Keeps Windows from painting a classic title bar into the app's undecorated,
//! transparent windows (the screen-share overlay and the drawing window).
//!
//! winit keeps `WS_CAPTION` on undecorated windows and removes the frame by handing the
//! whole window to the client area in `WM_NCCALCSIZE`. When DWM is not drawing the frame,
//! `DefWindowProc` still paints the classic caption (title text and buttons) into the top
//! rows of the window on `WM_NCACTIVATE` / `WM_NCPAINT`, and a transparent window shows
//! that strip through its transparent pixels. See gethopp/hopp#277 and
//! rust-windowing/winit#4733 / #4771. Subclassing the window and answering those messages
//! ourselves keeps the frame out. No-op on other platforms.

use winit::window::Window;

#[cfg(target_os = "windows")]
const FRAMELESS_SUBCLASS_ID: usize = 2;

#[cfg(target_os = "windows")]
unsafe extern "system" fn frameless_subclass_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _uid_subclass: usize,
    _ref_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::{LPARAM, LRESULT};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCACTIVATE, WM_NCDESTROY, WM_NCPAINT};

    match msg {
        // There is no frame to paint: the client area covers the whole window.
        WM_NCPAINT => return LRESULT(0),
        // An `lParam` of -1 tells `DefWindowProc` not to repaint the non-client area on an
        // activation change, which is what paints the classic caption strip. winit's own
        // focus bookkeeping still sees the message.
        // SAFETY: forwarding the callback arguments is required by the subclass contract.
        WM_NCACTIVATE => return unsafe { DefSubclassProc(hwnd, msg, wparam, LPARAM(-1)) },
        WM_NCDESTROY => {
            // SAFETY: the subclass is still installed on this window.
            unsafe {
                let _ = RemoveWindowSubclass(
                    hwnd,
                    Some(frameless_subclass_proc),
                    FRAMELESS_SUBCLASS_ID,
                );
            }
        }
        _ => {}
    }

    // SAFETY: forwarding the untouched callback arguments is required by the subclass contract.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Stops Windows from drawing a caption bar into an undecorated, transparent window.
#[cfg(target_os = "windows")]
pub(crate) fn suppress_nonclient_painting(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::UI::Shell::SetWindowSubclass;

    let Ok(raw_handle) = window.window_handle() else {
        log::warn!("suppress_nonclient_painting: failed to get window handle");
        return;
    };
    let RawWindowHandle::Win32(handle) = raw_handle.as_raw() else {
        log::warn!("suppress_nonclient_painting: not a Win32 handle");
        return;
    };

    // SAFETY: `hwnd` is a live window owned by winit on this thread; the subclass carries
    // no reference data and removes itself on `WM_NCDESTROY`.
    let ok = unsafe {
        let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);
        SetWindowSubclass(hwnd, Some(frameless_subclass_proc), FRAMELESS_SUBCLASS_ID, 0)
    };
    if ok.as_bool() {
        log::info!("suppress_nonclient_painting: installed");
    } else {
        log::warn!("suppress_nonclient_painting: SetWindowSubclass failed");
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn suppress_nonclient_painting(_window: &Window) {}
