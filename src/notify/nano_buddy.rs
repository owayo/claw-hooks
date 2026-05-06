//! NanoBuddy通知（Darwin Notification API経由）

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::raw::c_int;

    const EXT_NOTIFICATION: &str = "owayo.nanobuddy.ext";
    const STOP_NOTIFICATION: &str = "owayo.nanobuddy.stop";
    const SUBAGENT_START_NOTIFICATION: &str = "owayo.nanobuddy.subagent.start";
    const SUBAGENT_STOP_NOTIFICATION: &str = "owayo.nanobuddy.subagent.stop";

    unsafe extern "C" {
        fn notify_register_check(name: *const i8, out_token: *mut c_int) -> u32;
        fn notify_set_state(token: c_int, state: u64) -> u32;
        fn notify_post(name: *const i8) -> u32;
        // Apple notify(3): registration を解放する。
        // notify_register_check で取得した token は notify_cancel で解放しないと
        // プロセス内のリソースがリークする。
        fn notify_cancel(token: c_int) -> u32;
    }

    // DistributedNotificationCenter用のCore Foundation型定義
    type CFNotificationCenterRef = *const std::ffi::c_void;
    type CFStringRef = *const std::ffi::c_void;

    unsafe extern "C" {
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

    /// Rust文字列スライスからCFStringを生成。失敗時はnullを返す。
    /// 非nullの戻り値は呼び出し側でCFReleaseすること。
    unsafe fn cfstring_from_str(s: &str) -> CFStringRef {
        unsafe {
            CFStringCreateWithBytes(
                std::ptr::null(),
                s.as_ptr(),
                s.len() as isize,
                K_CF_STRING_ENCODING_UTF8,
                false,
            )
        }
    }

    /// 指定されたnameとobject文字列でDistributedNotificationを送信する。
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

    /// 拡張子文字列をリトルエンディアンu64にエンコード（最大8バイト）
    pub fn encode_ext(ext: &str) -> u64 {
        let mut value: u64 = 0;
        for (i, byte) in ext.bytes().take(8).enumerate() {
            value |= (byte as u64) << (i * 8);
        }
        value
    }

    /// 拡張子フックの完了をNanoBuddyに通知する。
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
            // notify_register_check で取得した token を解放する。
            // 解放しないと extension hook が呼ばれるたびにプロセス内の
            // registration リソースが累積する。
            let _ = notify_cancel(token);
        }
    }

    /// 監視停止をNanoBuddyに通知する。
    pub fn notify_stop_hook() {
        let name = match CString::new(STOP_NOTIFICATION) {
            Ok(n) => n,
            Err(_) => return,
        };

        unsafe {
            let _ = notify_post(name.as_ptr());
        }
    }

    /// サブエージェント通知のobject文字列を生成（session_idがある場合はJSON形式）
    #[cfg_attr(test, allow(dead_code))]
    pub(super) fn format_subagent_object(subagent_type: &str, session_id: Option<&str>) -> String {
        match session_id {
            Some(sid) => {
                #[derive(serde::Serialize)]
                struct SubagentObject<'a> {
                    #[serde(rename = "type")]
                    subagent_type: &'a str,
                    sid: &'a str,
                }
                serde_json::to_string(&SubagentObject { subagent_type, sid })
                    .expect("SubagentObject のシリアライズは失敗しない")
            }
            None => subagent_type.to_string(),
        }
    }

    /// サブエージェントの開始をNanoBuddyに通知する。
    /// 任意長のサブエージェントタイプ文字列に対応するためDistributedNotificationCenterを使用。
    pub fn notify_subagent_start(subagent_type: &str, session_id: Option<&str>) {
        let object = format_subagent_object(subagent_type, session_id);
        post_distributed_notification(SUBAGENT_START_NOTIFICATION, &object);
    }

    /// サブエージェントの停止をNanoBuddyに通知する。
    /// 任意長のサブエージェントタイプ文字列に対応するためDistributedNotificationCenterを使用。
    pub fn notify_subagent_stop(subagent_type: &str, session_id: Option<&str>) {
        let object = format_subagent_object(subagent_type, session_id);
        post_distributed_notification(SUBAGENT_STOP_NOTIFICATION, &object);
    }
}

/// 拡張子フックの完了をNanoBuddyに通知する。
pub fn notify_extension_hook(ext: &str) {
    #[cfg(target_os = "macos")]
    macos::notify_extension_hook(ext);

    #[cfg(not(target_os = "macos"))]
    let _ = ext;
}

/// 監視停止をNanoBuddyに通知する。
pub fn notify_stop_hook() {
    #[cfg(target_os = "macos")]
    macos::notify_stop_hook();
}

/// サブエージェントの開始をNanoBuddyに通知する。
pub fn notify_subagent_start(subagent_type: &str, session_id: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::notify_subagent_start(subagent_type, session_id);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = subagent_type;
        let _ = session_id;
    }
}

/// サブエージェントの停止をNanoBuddyに通知する。
pub fn notify_subagent_stop(subagent_type: &str, session_id: Option<&str>) {
    #[cfg(target_os = "macos")]
    macos::notify_subagent_stop(subagent_type, session_id);

    #[cfg(not(target_os = "macos"))]
    {
        let _ = subagent_type;
        let _ = session_id;
    }
}

/// 拡張子をリトルエンディアンu64にエンコード（テスト用公開）
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

    #[test]
    fn test_format_subagent_object_json_roundtrip() {
        #[cfg(target_os = "macos")]
        {
            // 特殊文字・制御文字を含む値で有効なJSONが生成されること
            let typ = "te\"st\n\t";
            let sid = "a\\b\"c\r\u{0008}";
            let s = macos::format_subagent_object(typ, Some(sid));
            let v: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(v["type"], typ);
            assert_eq!(v["sid"], sid);
        }
    }

    // === encode_ext 境界値テスト ===

    #[test]
    fn test_encode_ext_exactly_8_bytes() {
        // 8バイトちょうどの拡張子がすべてエンコードされることを確認
        let ext = "12345678";
        let encoded = encode_ext(ext);
        assert_ne!(encoded, 0);
        // 各バイトが正しい位置にあることを確認
        assert_eq!((encoded & 0xFF) as u8, b'1');
        assert_eq!(((encoded >> 56) & 0xFF) as u8, b'8');
    }

    #[test]
    fn test_encode_ext_over_8_bytes_truncates() {
        // 9バイト以上の拡張子は8バイトまでで切り詰められる
        let ext_8 = "12345678";
        let ext_9 = "123456789";
        assert_eq!(encode_ext(ext_8), encode_ext(ext_9));
    }

    #[test]
    fn test_encode_ext_single_char() {
        // 1バイト文字のエンコード
        let encoded = encode_ext("a");
        assert_eq!(encoded, b'a' as u64);
    }

    #[test]
    fn test_encode_ext_multibyte_utf8() {
        // マルチバイトUTF-8はバイト単位でエンコードされる
        let encoded = encode_ext("あ");
        assert_ne!(encoded, 0);
        // "あ" は UTF-8 で 3 バイト（0xE3, 0x81, 0x82）
        assert_eq!((encoded & 0xFF) as u8, 0xE3);
        assert_eq!(((encoded >> 8) & 0xFF) as u8, 0x81);
        assert_eq!(((encoded >> 16) & 0xFF) as u8, 0x82);
    }

    // === 通知関数パニックなしテスト ===

    #[test]
    fn test_notify_extension_hook_empty_string() {
        // 空文字列でもパニックしない
        notify_extension_hook("");
    }

    #[test]
    fn test_notify_subagent_with_session_id() {
        // session_id 付きでもパニックしない
        notify_subagent_start("explore", Some("session-abc-123"));
        notify_subagent_stop("explore", Some("session-abc-123"));
    }

    #[test]
    fn test_notify_subagent_with_empty_type() {
        // 空文字列の subagent_type でもパニックしない
        notify_subagent_start("", None);
        notify_subagent_stop("", None);
    }

    // === format_subagent_object 追加テスト ===

    #[test]
    fn test_format_subagent_object_empty_strings() {
        #[cfg(target_os = "macos")]
        {
            let result = macos::format_subagent_object("", None);
            assert_eq!(result, "");

            let result = macos::format_subagent_object("", Some(""));
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["type"], "");
            assert_eq!(v["sid"], "");
        }
    }

    #[test]
    fn test_format_subagent_object_long_strings() {
        #[cfg(target_os = "macos")]
        {
            let long_type = "a".repeat(1000);
            let long_sid = "b".repeat(1000);
            let result = macos::format_subagent_object(&long_type, Some(&long_sid));
            let v: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(v["type"], long_type);
            assert_eq!(v["sid"], long_sid);
        }
    }
}
