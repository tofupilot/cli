//! Unified CLI output formatting.
//!
//! Symbols and colors match the shell install/deploy scripts
//! so every TofuPilot surface looks identical.
//!
//! All helpers write to **stderr** (stdout is reserved for
//! machine-readable output like JSON).

use std::io::IsTerminal;

/// Enable Virtual Terminal processing + UTF-8 output on Windows.
///
/// - VT processing makes ANSI escape sequences render as colors instead
///   of literal `←[38;...` text on legacy conhost.
/// - UTF-8 output (codepage 65001) lets us print non-ASCII glyphs (✓ ✗
///   ✈ → ! arrows) instead of `?`. PS5.1 conhost defaults to OEM CP437
///   / CP850 which can't represent any of the symbols we use.
///
/// No-op on non-Windows. Safe to call multiple times. Errors are
/// swallowed: if the console rejects either change, output degrades
/// gracefully (raw escapes / `?` glyphs) rather than crashing.
pub fn enable_vt() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
        };
        const CP_UTF8: u32 = 65001;
        unsafe {
            let _ = SetConsoleOutputCP(CP_UTF8);
            for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let handle = GetStdHandle(which);
                if handle.is_null() {
                    continue;
                }
                let mut mode: u32 = 0;
                if GetConsoleMode(handle, &mut mode) == 0 {
                    continue;
                }
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

// ANSI escape sequences -- same values as the shell scripts.
const GREEN: &str = "\x1b[1;32m";
const BLUE: &str = "\x1b[38;2;59;130;246m";
const RED: &str = "\x1b[0;31m";
const YELLOW: &str = "\x1b[38;2;234;179;8m";
const NC: &str = "\x1b[0m";

fn use_color() -> bool {
    std::io::stderr().is_terminal()
}

/// Success mark. `✓` on Unix; `√` on Windows because conhost's default
/// font + CP 850 fallback can't render U+2713 reliably while U+221A is
/// always present (same trick npm/pip use).
#[cfg(windows)]
const SUCCESS_MARK: &str = "V";
#[cfg(not(windows))]
const SUCCESS_MARK: &str = "✓";

#[cfg(windows)]
const FAIL_MARK: &str = "X";
#[cfg(not(windows))]
const FAIL_MARK: &str = "✗";

pub fn success(msg: &str) {
    if use_color() {
        eprintln!("{GREEN}{SUCCESS_MARK} {msg}{NC}");
    } else {
        eprintln!("{SUCCESS_MARK} {msg}");
    }
}

/// → Info (blue).
pub fn info(msg: &str) {
    if use_color() {
        eprintln!("{BLUE}→ {msg}{NC}");
    } else {
        eprintln!("→ {msg}");
    }
}

/// Error (red). Does **not** exit.
pub fn error(msg: &str) {
    if use_color() {
        eprintln!("{RED}{FAIL_MARK} {msg}{NC}");
    } else {
        eprintln!("{FAIL_MARK} {msg}");
    }
}

/// ! Warning (yellow).
pub fn warn(msg: &str) {
    if use_color() {
        eprintln!("{YELLOW}! {msg}{NC}");
    } else {
        eprintln!("! {msg}");
    }
}
