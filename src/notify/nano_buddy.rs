//! NanoBuddy notification via Darwin Notification API.

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::raw::c_int;

    const EXT_NOTIFICATION: &str = "owayo.nanobuddy.ext";
    const STOP_NOTIFICATION: &str = "owayo.nanobuddy.stop";

    extern "C" {
        fn notify_register_check(name: *const i8, out_token: *mut c_int) -> u32;
        fn notify_set_state(token: c_int, state: u64) -> u32;
        fn notify_post(name: *const i8) -> u32;
    }

    /// Encode extension string as little-endian u64 (max 8 bytes).
    pub fn encode_ext(ext: &str) -> u64 {
        let mut value: u64 = 0;
        for (i, byte) in ext.bytes().take(8).enumerate() {
            value |= (byte as u64) << (i * 8);
        }
        value
    }

    /// Notify NanoBuddy that an extension hook completed.
    pub fn notify_extension_hook(ext: &str) {
        let name = match CString::new(EXT_NOTIFICATION) {
            Ok(n) => n,
            Err(_) => return,
        };

        unsafe {
            let mut token: c_int = 0;
            if notify_register_check(name.as_ptr(), &mut token) != 0 {
                return;
            }
            let _ = notify_set_state(token, encode_ext(ext));
            let _ = notify_post(name.as_ptr());
        }
    }

    /// Notify NanoBuddy that monitoring has stopped.
    pub fn notify_stop_hook() {
        let name = match CString::new(STOP_NOTIFICATION) {
            Ok(n) => n,
            Err(_) => return,
        };

        unsafe {
            let _ = notify_post(name.as_ptr());
        }
    }
}

/// Notify NanoBuddy that an extension hook completed.
pub fn notify_extension_hook(ext: &str) {
    #[cfg(target_os = "macos")]
    macos::notify_extension_hook(ext);

    #[cfg(not(target_os = "macos"))]
    let _ = ext;
}

/// Notify NanoBuddy that monitoring has stopped.
pub fn notify_stop_hook() {
    #[cfg(target_os = "macos")]
    macos::notify_stop_hook();
}

/// Encode extension as little-endian u64 (public for tests).
#[cfg(test)]
pub fn encode_ext(ext: &str) -> u64 {
    #[cfg(target_os = "macos")]
    return macos::encode_ext(ext);

    #[cfg(not(target_os = "macos"))]
    {
        let mut value: u64 = 0;
        for (i, byte) in ext.bytes().take(8).enumerate() {
            value |= (byte as u64) << (i * 8);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_ext_rs() {
        assert_eq!(encode_ext("rs"), 29554);
    }

    #[test]
    fn test_encode_ext_c() {
        assert_eq!(encode_ext("c"), 99);
    }

    #[test]
    fn test_encode_ext_empty() {
        assert_eq!(encode_ext(""), 0);
    }

    #[test]
    fn test_notify_does_not_panic() {
        notify_extension_hook("rs");
        notify_stop_hook();
    }
}
