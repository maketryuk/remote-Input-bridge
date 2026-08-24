//! "Start with Windows" via the per-user Run key. No service, no scheduled task, no elevation.
//!
//! The installer can write the same value (its optional startup task), so the registry - not the
//! config file - is the single source of truth for whether the bridge starts with Windows.

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE_NAME: &str = "RemoteInputBridge";

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub fn set(enabled: bool) -> std::io::Result<()> {
    use std::io;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    let exe = std::env::current_exe()?;
    let command = format!("\"{}\"", exe.display());
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_SET_VALUE, &mut key)
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let name = wide(VALUE_NAME);
    let status = if enabled {
        let value = wide(&command);
        unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                (value.len() * 2) as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, name.as_ptr()) }
    };
    unsafe { RegCloseKey(key) };
    // Deleting a value that was never there is a success as far as the caller is concerned.
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    if status != 0 && !(!enabled && status == ERROR_FILE_NOT_FOUND) {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// Whether Windows will start the bridge at logon.
///
/// Only the presence of the value is checked, not the path inside it: after an update the
/// executable is replaced at the same location, and a mismatch would otherwise make the setting
/// look switched off for no reason a user could act on.
#[cfg(windows)]
pub fn is_enabled() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
    };

    let mut size: u32 = 0;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(VALUE_NAME).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    // A string of one wide NUL is an empty value, which is not an autostart entry.
    status == 0 && size > 2
}

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}
