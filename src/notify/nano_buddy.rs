//! NanoBuddy notification via Darwin Notification API.

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::raw::c_int;

    const EXT_NOTIFICATION: &str = "owayo.nanobuddy.ext";
    const STOP_NOTIFICATION: &str = "owayo.nanobuddy.stop";
    const SUBAGENT_START_NOTIFICATION: &str = "owayo.nanobuddy.subagent.start";
    const SUBAGENT_STOP_NOTIFICATION: &str = "owayo.nanobuddy.subagent.stop";

    extern "C" {
        fn notify_register_check(name: *const i8, out_token: *mut c_int) -> u32;
        fn notify_set_state(token: c_int, state: u64) -> u32;
        fn notify_post(name: *const i8) -> u32;
    }

    // Core Foundation types for DistributedNotificationCenter
    type CFNotificationCenterRef = *const std::ffi::c_void;
    type CFStringRef = *const std::ffi::c_void;

    extern "C" {
        fn CFNotificationCenterGetDistributedCenter() -> CFNotificationCenterRef;
        fn CFNotificationCenterPostNotification(
            center: CFNotificationCenterRef,
            name: CFStringRef,
            object: CFStringRef,
            user_info: *const std::ffi::c_void,
            deliver_immediately: bool,
        );
        fn CFStringCreateWithBytes(
            alloc: *const std::ffi::c_void,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: bool,
        ) -> CFStringRef;
        fn CFRelease(cf: *const std::ffi::c_void);
    }

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x08000100;

    /// Create a CFString from a Rust string slice. Returns null on failure.
    /// Caller must CFRelease non-null results.
    unsafe fn cfstring_from_str(s: &str) -> CFStringRef {
        CFStringCreateWithBytes(
            std::ptr::null(),
            s.as_ptr(),
            s.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
            false,
        )
    }

    /// Post a DistributedNotification with the given name and object string.
    fn post_distributed_notification(name: &str, object: &str) {
        unsafe {
            let center = CFNotificationCenterGetDistributedCenter();
            if center.is_null() {
                return;
            }
            let cf_name = cfstring_from_str(name);
            if cf_name.is_null() {
                return;
            }
            let cf_object = cfstring_from_str(object);
            if cf_object.is_null() {
                CFRelease(cf_name);
                return;
            }
            CFNotificationCenterPostNotification(
                center,
                cf_name,
                cf_object,
                std::ptr::null(),
                true,
            );
            CFRelease(cf_name);
            CFRelease(cf_object);
        }
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

    /// Format subagent object string, optionally including session ID as JSON.
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn format_subagent_object(subagent_type: &str, session_id: Option<&str>) -> String {
        match session_id {
            Some(sid) => format!(r#"{{"type":"{}","sid":"{}"}}"#, subagent_type, sid),
            None => subagent_type.to_string(),
        }
    }

    /// Notify NanoBuddy that a subagent has started.
    /// Uses DistributedNotificationCenter to support arbitrary-length subagent type strings.
    pub fn notify_subagent_start(subagent_type: &str, session_id: Option<&str>) {
        let object = format_subagent_object(subagent_type, session_id);
        post_distributed_notification(SUBAGENT_START_NOTIFICATION, &object);
    }

    /// Notify NanoBuddy that a subagent has stopped.
    /// Uses DistributedNotificationCenter to support arbitrary-length subagent type strings.
    pub fn notify_subagent_stop(subagent_type: &str, session_id: Option<&str>) {
        let object = format_subagent_object(subagent_type, session_id);
        post_distributed_notification(SUBAGENT_STOP_NOTIFICATION, &object);
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

/// Notify NanoBuddy that a subagent has started.
pub fn notify_subagent_start(subagent_type: &str, session_id: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::notify_subagent_start(subagent_type, session_id);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = subagent_type;
        let _ = session_id;
    }
}

/// Notify NanoBuddy that a subagent has stopped.
pub fn notify_subagent_stop(subagent_type: &str, session_id: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::notify_subagent_stop(subagent_type, session_id);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = subagent_type;
        let _ = session_id;
    }
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

    #[test]
    fn test_notify_subagent_does_not_panic() {
        notify_subagent_start("explore", None);
        notify_subagent_start("generalPurpose", None);
        notify_subagent_stop("explore", None);
        notify_subagent_stop("generalPurpose", None);
    }

    #[test]
    fn test_format_subagent_object_with_session_id() {
        #[cfg(target_os = "macos")]
        {
            let result = macos::format_subagent_object("explore", Some("abc-123"));
            assert_eq!(result, r#"{"type":"explore","sid":"abc-123"}"#);
        }
    }

    #[test]
    fn test_format_subagent_object_without_session_id() {
        #[cfg(target_os = "macos")]
        {
            let result = macos::format_subagent_object("explore", None);
            assert_eq!(result, "explore");
        }
    }
}
