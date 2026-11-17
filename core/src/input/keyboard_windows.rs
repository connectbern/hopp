use super::{get_modifiers, KeyModifier, KeyboardEventTrait, KeyboardLayoutTrait};
use crate::input::{diagnostics, input_state_windows};

use windows::Win32::Foundation::GetLastError;
use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        GetKeyboardLayout, MapVirtualKeyExW, SendInput, ToUnicodeEx, HKL, INPUT, INPUT_0,
        INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
        KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, VIRTUAL_KEY, VK_APPS, VK_BACK, VK_CAPITAL, VK_CONTROL,
        VK_DELETE, VK_DIVIDE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT,
        VK_LWIN, VK_MENU, VK_NEXT, VK_NUMPAD0, VK_PAUSE, VK_PRIOR, VK_RCONTROL, VK_RETURN,
        VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_SHIFT, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
    },
    WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
};

/// fork: names of the function keys; F1..F24 are consecutive virtual-key codes.
const FUNCTION_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

use std::collections::HashMap;

fn map_modifier_to_virtual_key(modifier: KeyModifier) -> u32 {
    let virtual_key = match modifier {
        KeyModifier::Cmd => VK_LWIN,
        KeyModifier::Shift => VK_SHIFT,
        KeyModifier::AlphaLock => VK_CAPITAL,
        KeyModifier::Option => VK_MENU,
        KeyModifier::Ctrl => VK_CONTROL,
        KeyModifier::RightShift => VK_RSHIFT,
        KeyModifier::RightOption => VK_RMENU,
        KeyModifier::RightCtrl => VK_RCONTROL,
    };
    virtual_key.0 as u32
}

pub struct KeyboardLayout {
    layout: HKL,
}

impl Default for KeyboardLayout {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardLayout {
    pub fn new() -> Self {
        let layout = unsafe {
            let thread_id = GetWindowThreadProcessId(GetForegroundWindow(), None);
            GetKeyboardLayout(thread_id)
        };
        Self { layout }
    }
}

impl KeyboardLayoutTrait for KeyboardLayout {
    fn key_translate(&self, keycode: u16, modifiers: u32) -> Option<String> {
        // fork: leave the numeric keypad out of the key map. Its keys produce the
        // same characters as main-block keys ("." "-" "+" "*" "/" and the digits)
        // and have lower key codes, so the lookup for "." found the keypad's
        // decimal key (and Ctrl+. became Ctrl+NumpadDecimal).
        if (VK_NUMPAD0.0..=VK_DIVIDE.0).contains(&keycode) {
            return None;
        }
        let scan_code = unsafe { MapVirtualKeyExW(keycode as u32, MAPVK_VK_TO_VSC, self.layout) };
        if scan_code == 0 {
            return None;
        }
        // fork: start from a clean keyboard state instead of GetKeyboardState(), so a
        // stale Ctrl/Shift/Caps Lock in this thread's state cannot poison the key map.
        let mut keyboard_state = [0u8; 256];

        let modifiers = get_modifiers(modifiers);
        for modifier in modifiers {
            // On Windows Alt alone does not change the character a key produces;
            // AltGr (= Ctrl+Alt) does (@ # \ | [ ] { } ~ on Swiss/German layouts),
            // so the "Option" pass of the key map is built with AltGr.
            if matches!(modifier, KeyModifier::Option) {
                keyboard_state[VK_CONTROL.0 as usize] |= 0x80;
            }
            let virtual_key = map_modifier_to_virtual_key(modifier);
            if virtual_key != 0 {
                keyboard_state[virtual_key as usize] |= 0x80;
            }
        }

        let mut res: [u16; 4] = [0; 4];
        // fork: translate with the foreground window's layout (ToUnicode used this
        // thread's layout) and flag 0x4 = leave the kernel keyboard state alone, so
        // scanning dead keys (^ ´ ` ¨ ~) while building the map has no side effects.
        let unicode = unsafe {
            ToUnicodeEx(
                keycode as u32,
                scan_code,
                &keyboard_state,
                &mut res,
                0x4,
                self.layout,
            )
        };
        if unicode <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&res[..unicode as usize]))
    }

    fn has_changed(&mut self) -> bool {
        let layout = unsafe {
            let thread_id = GetWindowThreadProcessId(GetForegroundWindow(), None);
            GetKeyboardLayout(thread_id)
        };
        let has_changed = layout != self.layout;
        if has_changed {
            self.layout = layout;
        }
        has_changed
    }

    fn get_independent_codes(&self) -> HashMap<&'static str, u16> {
        let mut map: HashMap<&'static str, u16> = HashMap::new();
        map.insert("Enter", VK_RETURN.0);
        map.insert("Tab", VK_TAB.0);
        map.insert("Backspace", VK_BACK.0);
        map.insert("Escape", VK_ESCAPE.0);
        map.insert("ArrowUp", VK_UP.0);
        map.insert("ArrowDown", VK_DOWN.0);
        map.insert("ArrowLeft", VK_LEFT.0);
        map.insert("ArrowRight", VK_RIGHT.0);
        map.insert("PageUp", VK_PRIOR.0);
        map.insert("PageDown", VK_NEXT.0);
        map.insert("Home", VK_HOME.0);
        map.insert("End", VK_END.0);
        map.insert("Delete", VK_DELETE.0);
        map.insert("Control", VK_CONTROL.0);
        map.insert("Shift", VK_SHIFT.0);
        map.insert("Meta", VK_LWIN.0);
        map.insert("Alt", VK_MENU.0);
        // fork: a Windows viewer's AltGr arrives as "AltGraph"; press Right Alt so
        // AltGr characters (@ # \ | [ ] { } ~ on Swiss/German layouts) can be typed.
        map.insert("AltGraph", VK_RMENU.0);
        map.insert("CapsLock", VK_CAPITAL.0);
        // fork: Space is pressed as the real space bar (down and up), exactly like
        // the sharer's own keyboard, instead of being injected as text.
        map.insert(" ", VK_SPACE.0);
        map.insert("Space", VK_SPACE.0);
        // fork: named keys that had no key code on Windows (the function keys did
        // nothing; the others typed the first letter of their name).
        for (index, name) in FUNCTION_KEYS.iter().enumerate() {
            map.insert(name, VK_F1.0 + index as u16);
        }
        map.insert("Insert", VK_INSERT.0);
        map.insert("ContextMenu", VK_APPS.0);
        map.insert("PrintScreen", VK_SNAPSHOT.0);
        map.insert("Pause", VK_PAUSE.0);
        map.insert("Super", VK_LWIN.0);
        map
    }
}

fn keyboard_input(keycode: u16, unicode: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(keycode),
                wScan: unicode,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_input(input: &[INPUT]) {
    let input_size = std::mem::size_of::<INPUT>() as i32;
    let input_len = input.len() as u32;
    let result = unsafe { SendInput(input, input_size) };
    if result != input_len {
        // fork: read the reason before another call can overwrite it, and write
        // one line per second with a count instead of one line per event.
        // (The number this line used to end with was the count of accepted
        // events, not an error code.)
        let last_error = unsafe { GetLastError() }.0;
        if let Some(count) = diagnostics::key_refused() {
            log::error!(
                "keyboard_input: SendInput failed x{count} ({result} of {input_len} events accepted): {}",
                input_state_windows::failure_context(last_error)
            );
        }
        return;
    }
    diagnostics::key_accepted();
    // SAFETY: only INPUT_KEYBOARD events are built in this file.
    let is_key_down = input.iter().any(
        |event| unsafe { event.Anonymous.ki.dwFlags } & KEYEVENTF_KEYUP == KEYBD_EVENT_FLAGS(0),
    );
    if is_key_down {
        // Accepted is not the same as delivered: a window running as administrator
        // silently ignores it.
        input_state_windows::check_key_target();
    }
}

pub struct KeyboardEvent {
    flags: KEYBD_EVENT_FLAGS,
    unicode: u16,
    keycode: u16,
}

impl KeyboardEvent {
    pub fn new(
        keycode: u16,
        _modifier: u32,
        down: bool,
        _target_process_id: Option<i32>,
    ) -> Option<Self> {
        let mut flags = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        // fork: right-hand Alt/Ctrl are extended keys; right Alt is AltGr on
        // Swiss/German layouts, where Windows then adds the implicit left Ctrl.
        if keycode == VK_RMENU.0 || keycode == VK_RCONTROL.0 {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        Some(Self {
            keycode,
            unicode: 0,
            flags,
        })
    }
}

impl KeyboardEventTrait for KeyboardEvent {
    fn override_utf(&mut self, key: &str) {
        self.unicode = match key.encode_utf16().next() {
            Some(unicode) => unicode,
            None => {
                log::error!("override_utf: key: {key} is not a valid unicode");
                sentry_utils::upload_logs_event("KeyboardEvent override_utf failed".to_string());
                0
            }
        };
        self.flags |= KEYEVENTF_UNICODE;
        self.keycode = 0;
    }

    fn send(&self) {
        let input = keyboard_input(self.keycode, self.unicode, self.flags);
        let inputs = vec![input];
        send_input(&inputs);
    }
}
