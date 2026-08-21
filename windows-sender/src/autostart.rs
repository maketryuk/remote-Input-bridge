//! "Start with Windows" via the per-user Run key. No service, no scheduled task, no elevation.

#[cfg(windows)]
pub fn set(enabled: bool) -> std::io::Result<()> {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "RemoteInputBridge";

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text).encode_wide().chain(std::iter::once(0)).collect()
    }

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

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> std::io::Result<()> {
    Ok(())
}
