//! 密钥的加解密边界。
//!
//! **为什么单独一个模块**：这是本项目里唯一处理明文密钥的地方。把它隔离出来，
//! 让「密钥从哪来、到哪去」可以在一处审阅，而不是散落在存储层、代理层与 UI 里。
//!
//! **当前实现**：占位（密文即明文的 UTF-8 字节）。这样 Phase 1–3 的功能可以
//! 先跑通，而**接口形状已经定死**——接入 stronghold 时只需替换本文件的两个函数，
//! 调用方（bootstrap.rs、控制面）无需改动。
//!
//! **接入 stronghold 的做法**（Phase 4 收尾）：
//! 1. 首次启动生成主密钥，存进 `tauri-plugin-stronghold`（argon2 派生，纯 Rust，
//!    三平台一致，不依赖 Linux 的 Secret Service）；
//! 2. 用主密钥跑 AES-GCM，把「明文 key」加密成 `key_cipher`；
//! 3. `decrypt_key` 反向操作；
//! 4. 控制面只接收**密文**（见 control.rs 的 CreateAccountBody），因此本模块
//!    是唯一的解密点。
//!
//! 在此之前，数据库里的密文不具保护意义——因此 Phase 4 完成前不应把本应用
//! 当作「密钥安全存储」来宣传。

/// 解密一个账号密钥。
///
/// 返回明文 API key（`user_` 开头）。解密失败返回 Err，调用方应跳过该账号
/// 而不是让整个应用启动失败——一个坏账号不该阻止其他账号工作。
pub fn decrypt_key(cipher: &[u8]) -> Result<String, SecretError> {
    String::from_utf8(cipher.to_vec()).map_err(|e| SecretError::Corrupted(e.to_string()))
}

/// 加密一个账号密钥（写库前调用）。
pub fn encrypt_key(plaintext: &str) -> Result<Vec<u8>, SecretError> {
    Ok(plaintext.as_bytes().to_vec())
}

/// 生成密钥提示：`user_…ab12`。
///
/// 保留前 5 与后 4 位：足够用户辨认是哪一个 key，又不至于泄露可用信息。
/// 过短的 key 全部打码。
pub fn key_hint(plaintext: &str) -> String {
    let chars: Vec<char> = plaintext.chars().collect();
    if chars.len() < 12 {
        return "…".to_string();
    }
    let head: String = chars[..5].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 密钥处理失败。
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// 密文损坏或格式不符。
    #[error("密钥数据损坏：{0}")]
    Corrupted(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = "user_abcdefghijklmnop";
        let cipher = encrypt_key(key).unwrap();
        assert_eq!(decrypt_key(&cipher).unwrap(), key);
    }

    #[test]
    fn hint_keeps_head_and_tail_only() {
        let hint = key_hint("user_abcdefghijklmnop");
        assert!(hint.starts_with("user_"), "保留前缀便于辨认类型");
        assert!(hint.ends_with("mnop"), "保留尾部便于区分不同 key");
        assert!(hint.contains('…'));
        // 中间部分必须不可见
        assert!(!hint.contains("efgh"), "提示不得泄露中间字符：{hint}");
    }

    #[test]
    fn short_keys_are_fully_masked() {
        assert_eq!(key_hint("user_short"), "…", "过短的 key 应整体打码");
    }

    #[test]
    fn corrupted_cipher_is_an_error_not_a_panic() {
        let bad = vec![0xff, 0xfe, 0xfd];
        assert!(
            decrypt_key(&bad).is_err(),
            "坏密文应返回错误，由调用方跳过该账号"
        );
    }
}
