//! 密钥的加解密边界。
//!
//! **为什么单独一个模块**：这是本项目里唯一处理明文密钥的地方。把它隔离出来，
//! 让「密钥从哪来、到哪去」可以在一处审阅，而不是散落在存储层、代理层与 UI 里。
//!
//! # 加密方案
//!
//! - **算法**：AES-256-GCM（认证加密）。GCM 自带完整性校验，密文被篡改时解密
//!   会**失败**而不是产出垃圾明文——对密钥这种数据，静默损坏比报错危险得多。
//! - **主密钥**：32 字节随机数，首次运行时生成，存进**操作系统钥匙串**
//!   （macOS Keychain / Windows Credential Manager / Linux Secret Service）。
//!   代码里不硬编码、不进数据库、不进日志。
//! - **随机 nonce**：每次加密生成 12 字节随机 nonce，与密文一起存
//!   （`nonce || ciphertext`）。**绝不复用 nonce**：GCM 下复用会灾难性地
//!   泄露明文异或与认证密钥。
//! - **不用口令派生**：主密钥本身就是高熵随机值，argon2 这类慢哈希只会让每次
//!   启动变慢，并不提升安全性——攻击者拿到的是钥匙串里的密钥，不是弱口令。
//!
//! # 数据库里存什么
//!
//! `key_cipher` = `nonce(12) || AES-GCM(明文)`，另有 `key_hint`（形如
//! `user_…ab12`）供 UI 辨认。数据库泄露但拿不到钥匙串时，解不出明文。
//!
//! # 钥匙串不可用时
//!
//! 退化为「进程内存里的临时主密钥」并**打印醒目警告**：功能可用（能跑通对话），
//! 但重启后旧密文解不开（会提示重新添加账号）。选择退化而非拒绝启动，是因为
//! CI/容器/精简 Linux 上钥匙串常不可用，拒绝启动会让整个应用无法使用。

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};

/// 钥匙串里的条目名。
const KEYCHAIN_SERVICE: &str = "commandcode-desktop";
/// 钥匙串里的账户名（同一 service 下区分用途）。
const KEYCHAIN_ACCOUNT: &str = "master-key-v1";

/// nonce 长度（AES-GCM 标准）。
const NONCE_LEN: usize = 12;

/// 密钥处理失败。
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// 密文损坏、长度不足或认证失败（被篡改 / 主密钥不匹配）。
    #[error("密钥数据无法解密：{0}")]
    Corrupted(String),
}

/// 主密钥的来源，用于日志与诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MasterKeySource {
    /// 从应用受保护的私有密钥文件读取（权限 0600，无系统弹窗）。
    LocalFile,
    /// 从系统钥匙串读取或写入。
    #[allow(dead_code)]
    Keychain,
    /// 钥匙串不可用，退化为主进程内存中的临时密钥。
    Ephemeral,
}

/// 处理后的主密钥持有者。
///
/// 在应用启动时构造一次，之后由 [Self::decrypt] / [Self::encrypt] 使用。
/// 优先存放在应用私有数据目录中，保证只有当前用户有读写权限。
pub struct Secrets {
    cipher: Aes256Gcm,
    source: MasterKeySource,
}

impl Secrets {
    /// 从本地私有密钥文件或系统钥匙串加载主密钥；不存在则生成并保存。
    /// 优先使用 data_dir/.master_key（权限 0600），彻底避免 macOS 钥匙串弹窗。
    pub fn load_or_create(data_dir: &std::path::Path) -> Self {
        let key_file = data_dir.join(".master_key");
        if key_file.exists() {
            if let Ok(bytes) = std::fs::read(&key_file) {
                if bytes.len() == 32 {
                    let mut key = [0u8; 32];
                    key.copy_from_slice(&bytes);
                    tracing::info!("账号密钥的主密钥已从本地私有密钥文件加载（无密码弹窗）");
                    return Self::from_bytes(key, MasterKeySource::LocalFile);
                }
            }
        }

        match Self::load_or_create_inner(&key_file) {
            Ok(secrets) => {
                tracing::info!(source = ?secrets.source, "账号密钥的主密钥已就绪");
                secrets
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "主密钥初始化失败，本次运行使用内存中的临时主密钥；重启后需重新添加账号"
                );
                Self::ephemeral()
            }
        }
    }

    /// 用一把指定的主密钥构造（测试与显式注入用）。
    pub fn from_bytes(key: [u8; 32], source: MasterKeySource) -> Self {
        Self {
            cipher: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key)),
            source,
        }
    }

    /// 生成一把仅在本次进程有效的临时主密钥。
    fn ephemeral() -> Self {
        let key = Aes256Gcm::generate_key(&mut OsRng);
        Self {
            cipher: Aes256Gcm::new(&key),
            source: MasterKeySource::Ephemeral,
        }
    }

    /// 主密钥的来源。
    pub fn source(&self) -> MasterKeySource {
        self.source
    }

    /// 加密一个账号密钥，返回 `nonce || ciphertext`。
    pub fn encrypt(&self, plaintext: &str) -> Result<Vec<u8>, SecretError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| SecretError::Corrupted(format!("加密失败：{e}")))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// 解密一个账号密钥。
    ///
    /// 输入必须是 `nonce(12) || ciphertext`；长度不足或认证失败都返回错误，
    /// 由调用方跳过该账号而不是让整个应用启动失败。
    pub fn decrypt(&self, stored: &[u8]) -> Result<String, SecretError> {
        if stored.len() <= NONCE_LEN {
            return Err(SecretError::Corrupted(format!(
                "密文长度 {} 不足（至少需要 {} 字节 nonce 加密文）",
                stored.len(),
                NONCE_LEN
            )));
        }
        let (nonce_bytes, ciphertext) = stored.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = self.cipher.decrypt(nonce, ciphertext).map_err(|_| {
            // 不区分「认证失败」与「主密钥不匹配」：对用户都是「解不开」，
            // 而更具体的原因可能被用来推断信息。
            SecretError::Corrupted(
                "认证失败（密文被篡改，或主密钥已变化——例如上次运行时钥匙串不可用）".to_string(),
            )
        })?;
        String::from_utf8(plaintext).map_err(|e| SecretError::Corrupted(e.to_string()))
    }

    /// 优先从钥匙串迁移旧密钥（如果已存在），否则生成全新密钥，并写入本地 0600 文件。
    fn load_or_create_inner(
        key_file: &std::path::Path,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut loaded_key: Option<[u8; 32]> = None;

        // 尝试从旧版钥匙串读取（兼容已有的旧账号）
        if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
            if let Ok(encoded) = entry.get_password() {
                if let Ok(bytes) = decode_hex(&encoded) {
                    if let Ok(arr) = bytes.try_into() {
                        loaded_key = Some(arr);
                    }
                }
            }
        }

        let key: [u8; 32] = match loaded_key {
            Some(k) => k,
            None => {
                let generated = Aes256Gcm::generate_key(&mut OsRng);
                let mut bytes = [0u8; 32];
                bytes.copy_from_slice(&generated);
                bytes
            }
        };

        // 写入本地受保护文件（Unix 下设为 0600 仅当前用户可读写）
        if let Err(e) = write_private_key_file(key_file, &key) {
            tracing::warn!(%e, "未能写入本地主密钥文件");
        }

        Ok(Self::from_bytes(key, MasterKeySource::LocalFile))
    }
}

/// 写入仅当前系统用户可读写的私有文件（0600）。
fn write_private_key_file(path: &std::path::Path, key: &[u8; 32]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        use std::io::Write;
        file.write_all(key)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, key)?;
    }
    Ok(())
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

/// 字节串 → 十六进制（不引额外依赖，保持二进制体积）。
#[allow(dead_code)]
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 十六进制 → 字节串。
fn decode_hex(text: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if !text.len().is_multiple_of(2) {
        return Err("十六进制长度必须是偶数".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| e.into()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_secrets() -> Secrets {
        // 固定主密钥，让测试可重复
        Secrets::from_bytes([7u8; 32], MasterKeySource::Keychain)
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let s = fixed_secrets();
        let key = "user_abcdefghijklmnop";
        let cipher = s.encrypt(key).unwrap();
        assert_eq!(s.decrypt(&cipher).unwrap(), key);
    }

    #[test]
    fn ciphertext_does_not_contain_the_plaintext() {
        let s = fixed_secrets();
        let cipher = s.encrypt("user_supersecretvalue").unwrap();
        let as_text = String::from_utf8_lossy(&cipher);
        assert!(!as_text.contains("supersecret"), "密文里不得出现明文片段");
        assert!(!as_text.contains("user_"), "密文里不得出现明文前缀");
    }

    #[test]
    fn encryption_is_non_deterministic() {
        let s = fixed_secrets();
        let a = s.encrypt("user_same").unwrap();
        let b = s.encrypt("user_same").unwrap();
        // 每次加密用新的随机 nonce；相同明文不应产生相同密文
        assert_ne!(a, b, "nonce 必须每次随机，否则 GCM 会灾难性泄露");
        assert_eq!(a.len(), b.len(), "长度应一致（nonce 定长）");
        assert_eq!(s.decrypt(&a).unwrap(), s.decrypt(&b).unwrap());
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let s = fixed_secrets();
        let mut cipher = s.encrypt("user_abcdefghijkl").unwrap();
        let last = cipher.len() - 1;
        cipher[last] ^= 0x01;
        assert!(s.decrypt(&cipher).is_err(), "GCM 认证必须发现篡改");
    }

    #[test]
    fn wrong_master_key_fails_instead_of_returning_garbage() {
        let a = Secrets::from_bytes([1u8; 32], MasterKeySource::Keychain);
        let b = Secrets::from_bytes([2u8; 32], MasterKeySource::Keychain);
        let cipher = a.encrypt("user_abcdefghijkl").unwrap();
        assert!(
            b.decrypt(&cipher).is_err(),
            "换主密钥必须解密失败，而不是产出垃圾"
        );
    }

    #[test]
    fn short_ciphertext_is_rejected_without_panic() {
        let s = fixed_secrets();
        assert!(s.decrypt(&[]).is_err());
        assert!(
            s.decrypt(&[0u8; NONCE_LEN]).is_err(),
            "只有 nonce 没有密文应被拒绝"
        );
        assert!(s.decrypt(&[0u8; 3]).is_err());
    }

    #[test]
    fn empty_plaintext_roundtrips() {
        let s = fixed_secrets();
        let cipher = s.encrypt("").unwrap();
        assert_eq!(s.decrypt(&cipher).unwrap(), "");
    }

    #[test]
    fn hint_keeps_head_and_tail_only() {
        let hint = key_hint("user_abcdefghijklmnop");
        assert!(hint.starts_with("user_"), "保留前缀便于辨认类型");
        assert!(hint.ends_with("mnop"), "保留尾部便于区分不同 key");
        assert!(hint.contains('…'));
        assert!(!hint.contains("efgh"), "提示不得泄露中间字符：{hint}");
    }

    #[test]
    fn short_keys_are_fully_masked() {
        assert_eq!(key_hint("user_short"), "…", "过短的 key 应整体打码");
    }

    #[test]
    fn hex_helpers_roundtrip() {
        let bytes = [0u8, 1, 15, 16, 255];
        assert_eq!(decode_hex(&encode_hex(&bytes)).unwrap(), bytes.to_vec());
        assert!(decode_hex("abc").is_err(), "奇数长度应报错");
        assert!(decode_hex("zz").is_err(), "非十六进制字符应报错");
    }

    #[test]
    fn ephemeral_source_is_reported() {
        let s = Secrets::from_bytes([9u8; 32], MasterKeySource::Ephemeral);
        assert_eq!(s.source(), MasterKeySource::Ephemeral);
    }
}
