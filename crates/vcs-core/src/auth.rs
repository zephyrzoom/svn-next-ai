//! 认证原语：本地口令哈希、JWT 签发与校验、PAT 令牌。
//!
//! 这里只做密码学与序列化，不碰数据库。用户记录的读取、状态机校验、
//! `token_version` 比对都在 `vcs-server` 里完成——本模块保证的是
//! 「同样的输入在任何实现下得到同样的判定」。
//!
//! 两个刻意的取舍：
//!   - 口令用 Argon2id 而不是 PBKDF2/scrypt：内存硬，抗 GPU 暴力破解；
//!   - JWT 只实现 HS256：单服务端集中式系统不需要非对称签名，
//!     少一份密钥管理与算法混淆攻击面（`alg: none` 一律拒绝）。

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{hash_bytes, Error, ErrorCode, Hash, Result};

type HmacSha256 = Hmac<Sha256>;

/// JWT 有效期默认 12 小时；CLI / Web 管理端走这条。
pub const DEFAULT_TOKEN_TTL_SECONDS: i64 = 12 * 60 * 60;

/// 桌面客户端默认的 30 天免登录会话长度。
///
/// 只在桌面端这类把 token 交给系统钥匙串加密保存的客户端开放；服务器依旧无状态，
/// 禁用账号 / 改角色 / 重置口令会递增 `token_version`，长期 token 同样立即失效。
pub const REMEMBER_ME_TTL_SECONDS: i64 = 30 * 86_400;

// ---------------- 口令 ----------------

/// 生成 Argon2id PHC 字符串（自带盐与参数，可直接入库）。
pub fn hash_password(password: &str) -> Result<String> {
    // 盐由本模块自己的 CSPRNG 生成：Argon2 只负责派生，随机性来源单点可控。
    let mut salt_bytes = [0u8; 16];
    getrandom::getrandom(&mut salt_bytes)
        .map_err(|e| Error::new(ErrorCode::IoError, format!("取随机盐失败：{e}")))?;
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| Error::new(ErrorCode::IoError, format!("编码盐失败：{e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::new(ErrorCode::IoError, format!("口令哈希失败：{e}")))
}

/// 校验口令。任何解析失败都视为「不匹配」，绝不 panic。
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ---------------- JWT ----------------

/// token 里携带的最小声明集。
///
/// `tv` 是 `users.token_version`：改角色、禁用账号、重置口令时递增，
/// 从而让所有已签发但未过期的 token 立即失效——不引入黑名单表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// 用户 ID
    pub sub: String,
    /// 用户名，便于日志与审计直接可读
    pub usr: String,
    /// 全局身份：admin / user（读写权限按仓库授予，不在 token 里）
    pub role: String,
    /// token 版本号
    pub tv: i64,
    /// 签发时间（Unix 秒）
    pub iat: i64,
    /// 过期时间（Unix 秒）
    pub exp: i64,
    /// token 唯一 ID，用于审计与后续撤销
    pub jti: String,
}

impl Claims {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.exp
    }
}

#[derive(Debug, Clone)]
pub struct TokenSigner {
    secret: Vec<u8>,
    ttl_seconds: i64,
}

impl TokenSigner {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            secret: secret.into(),
            ttl_seconds: DEFAULT_TOKEN_TTL_SECONDS,
        }
    }

    pub fn with_ttl(mut self, ttl_seconds: i64) -> Self {
        self.ttl_seconds = ttl_seconds;
        self
    }

    /// 签发 token。`jti` 由调用方给出，便于把审计日志与 token 对上。
    pub fn sign(&self, sub: &str, usr: &str, role: &str, tv: i64, now: i64) -> Result<String> {
        self.sign_with_ttl(sub, usr, role, tv, now, self.ttl_seconds)
    }

    /// 按本次登录的选择指定有效期（例如 30 天免登录），不必替换全局 signer。
    pub fn sign_with_ttl(
        &self,
        sub: &str,
        usr: &str,
        role: &str,
        tv: i64,
        now: i64,
        ttl_seconds: i64,
    ) -> Result<String> {
        let claims = Claims {
            sub: sub.to_string(),
            usr: usr.to_string(),
            role: role.to_string(),
            tv,
            iat: now,
            exp: now + ttl_seconds,
            jti: random_token(16),
        };
        self.sign_claims(&claims)
    }

    pub fn sign_claims(&self, claims: &Claims) -> Result<String> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(claims)
                .map_err(|e| Error::new(ErrorCode::IoError, format!("claims 序列化失败：{e}")))?,
        );
        let signing_input = format!("{header}.{payload}");
        let signature = self.mac(signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{signature}"))
    }

    /// 校验签名、算法与有效期。三段中任意一段不合规都返回 `Unauthenticated`。
    pub fn verify(&self, token: &str, now: i64) -> Result<Claims> {
        let mut parts = token.trim().split('.');
        let (header, payload, signature) = match (parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => {
                (h, p, s)
            }
            _ => return Err(unauth("token 结构不合法")),
        };
        if parts.next().is_some() {
            return Err(unauth("token 结构不合法"));
        }

        let expected = self.mac(format!("{header}.{payload}").as_bytes())?;
        // 定长比较，避免签名被逐字节探测。
        if !constant_time_eq(expected.as_bytes(), signature.as_bytes()) {
            return Err(unauth("token 签名不匹配"));
        }

        // 只接受 HS256：显式拒绝 alg=none 等降级攻击。
        let header_raw = URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| unauth("token 头部不是合法 base64url"))?;
        let header_json: serde_json::Value =
            serde_json::from_slice(&header_raw).map_err(|_| unauth("token 头部不是合法 JSON"))?;
        if header_json.get("alg").and_then(|v| v.as_str()) != Some("HS256") {
            return Err(unauth("不支持的签名算法"));
        }

        let payload_raw = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| unauth("token 载荷不是合法 base64url"))?;
        let claims: Claims =
            serde_json::from_slice(&payload_raw).map_err(|_| unauth("token 载荷缺少必需字段"))?;

        if claims.is_expired(now) {
            return Err(Error::new(
                ErrorCode::TokenExpired,
                "token 已过期，请重新登录",
            ));
        }
        Ok(claims)
    }

    fn mac(&self, input: &[u8]) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|_| Error::new(ErrorCode::IoError, "HMAC 密钥长度不合法"))?;
        mac.update(input);
        Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn unauth(msg: &str) -> Error {
    Error::new(ErrorCode::Unauthenticated, msg)
}

// ---------------- 服务端敏感配置加密 ----------------

/// 域分隔前缀：同一个 `server.secret` 同时用于 JWT、keystream 与 MAC，
/// 加前缀避免不同用途产生可利用的关联。
const SEAL_STREAM_DOMAIN: &[u8] = b"vcs/secret-box/stream/v1";
const SEAL_MAC_DOMAIN: &[u8] = b"vcs/secret-box/mac/v1";

/// 生成定长 keystream：HMAC-SHA256(key, domain || nonce || counter) 分块。
fn seal_keystream(key: &[u8], nonce: &[u8], len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut counter: u32 = 0;
    while out.len() < len {
        let mut mac = HmacSha256::new_from_slice(key)
            .map_err(|_| Error::new(ErrorCode::IoError, "加密密钥长度不合法"))?;
        mac.update(SEAL_STREAM_DOMAIN);
        mac.update(nonce);
        mac.update(&counter.to_be_bytes());
        out.extend_from_slice(&mac.finalize().into_bytes());
        counter = counter
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorCode::IoError, "加密数据过长"))?;
    }
    out.truncate(len);
    Ok(out)
}

fn seal_mac(key: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| Error::new(ErrorCode::IoError, "加密密钥长度不合法"))?;
    mac.update(SEAL_MAC_DOMAIN);
    mac.update(nonce);
    mac.update(ciphertext);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 加密服务端保存的小段敏感数据（LDAP 服务账号 bind 密码）。
///
/// 格式是 base64url(`nonce[16] || mac[32] || ciphertext`)：先加密后认证，
/// 解密时先验 MAC 再解 XOR，**密文被篡改不会解出「看起来能用」的明文**。
/// 这不是通用加密库，只用于本机配置文件里的一小段口令；密钥来自 0600 的
/// `server.secret`，与 JWT 签名密钥同源但做了域分隔。
pub fn seal_secret(key: &[u8], plaintext: &[u8]) -> Result<String> {
    let mut nonce = [0u8; 16];
    getrandom::getrandom(&mut nonce)
        .map_err(|e| Error::new(ErrorCode::IoError, format!("取加密随机数失败：{e}")))?;
    let stream = seal_keystream(key, &nonce, plaintext.len())?;
    let ciphertext: Vec<u8> = plaintext
        .iter()
        .zip(stream.iter())
        .map(|(p, k)| p ^ k)
        .collect();
    let mac = seal_mac(key, &nonce, &ciphertext)?;
    let mut blob = Vec::with_capacity(nonce.len() + mac.len() + ciphertext.len());
    blob.extend_from_slice(&nonce);
    blob.extend_from_slice(&mac);
    blob.extend_from_slice(&ciphertext);
    Ok(URL_SAFE_NO_PAD.encode(blob))
}

/// 解密 `seal_secret` 的产物；密文、MAC 或密钥不对一律报错，绝不返回部分明文。
pub fn open_secret(key: &[u8], encoded: &str) -> Result<Vec<u8>> {
    let blob = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| Error::new(ErrorCode::IoError, "敏感配置不是合法 base64url"))?;
    if blob.len() < 48 {
        return Err(Error::new(ErrorCode::IoError, "敏感配置长度不合法"));
    }
    let (nonce, rest) = blob.split_at(16);
    let (mac, ciphertext) = rest.split_at(32);
    let expected = seal_mac(key, nonce, ciphertext)?;
    if !constant_time_eq(&expected, mac) {
        return Err(Error::new(
            ErrorCode::IoError,
            "敏感配置校验失败：密钥不匹配或内容被篡改",
        ));
    }
    let stream = seal_keystream(key, nonce, ciphertext.len())?;
    Ok(ciphertext
        .iter()
        .zip(stream.iter())
        .map(|(c, k)| c ^ k)
        .collect())
}

// ---------------- 随机令牌（PAT） ----------------

/// 生成 32 字节随机串（base64url）。用于 PAT 明文等「只展示一次」的凭证。
///
/// 直接取操作系统 CSPRNG：令牌不可预测性是唯一的安全边界，
/// 拿时间戳或哈希扩散冒充随机数会在审计里被一票否决。
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // 取不到熵就直接失败，绝不退化成可预测的令牌。
    getrandom::getrandom(&mut buf).expect("操作系统 CSPRNG 不可用");
    URL_SAFE_NO_PAD.encode(&buf)
}

/// 令牌只入库其摘要，明文仅在生成时展示一次。
pub fn token_digest(token: &str) -> Hash {
    hash_bytes(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let phc = hash_password("Abcdef1!").unwrap();
        assert!(phc.starts_with("$argon2id$"), "应为 Argon2id: {phc}");
        assert!(verify_password("Abcdef1!", &phc));
        assert!(!verify_password("Abcdef1?", &phc));
        assert!(!verify_password("Abcdef1!", "not-a-phc-string"));
    }

    #[test]
    fn same_password_gets_different_salt() {
        let a = hash_password("Abcdef1!").unwrap();
        let b = hash_password("Abcdef1!").unwrap();
        assert_ne!(a, b);
        assert!(verify_password("Abcdef1!", &a));
        assert!(verify_password("Abcdef1!", &b));
    }

    #[test]
    fn jwt_roundtrip() {
        let signer = TokenSigner::new(b"secret-key-for-tests".to_vec());
        let token = signer.sign("u1", "alice", "admin", 3, 1_000).unwrap();
        let claims = signer.verify(&token, 1_100).unwrap();
        assert_eq!(claims.sub, "u1");
        assert_eq!(claims.usr, "alice");
        assert_eq!(claims.role, "admin");
        assert_eq!(claims.tv, 3);
        assert_eq!(claims.exp, 1_000 + DEFAULT_TOKEN_TTL_SECONDS);
    }

    #[test]
    fn jwt_rejects_tampering_and_expiry() {
        let signer = TokenSigner::new(b"k".to_vec()).with_ttl(60);
        let token = signer.sign("u1", "alice", "admin", 0, 1_000).unwrap();

        assert_eq!(
            signer.verify(&token, 1_061).unwrap_err().code,
            ErrorCode::TokenExpired
        );

        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        parts[2] = URL_SAFE_NO_PAD.encode([9u8; 32]);
        assert_eq!(
            signer.verify(&parts.join("."), 1_010).unwrap_err().code,
            ErrorCode::Unauthenticated
        );

        let other = TokenSigner::new(b"different".to_vec()).with_ttl(60);
        assert_eq!(
            other.verify(&token, 1_010).unwrap_err().code,
            ErrorCode::Unauthenticated
        );
    }

    #[test]
    fn jwt_rejects_alg_none() {
        let signer = TokenSigner::new(b"k".to_vec());
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            br#"{"sub":"u1","usr":"a","role":"admin","tv":0,"iat":0,"exp":9999999999,"jti":"x"}"#,
        );
        let forged = format!("{header}.{payload}.");
        assert!(signer.verify(&forged, 0).is_err());
    }

    #[test]
    fn random_tokens_are_distinct() {
        let a = random_token(32);
        let b = random_token(32);
        assert_ne!(a, b);
        assert_eq!(a.len(), 43); // 32 字节 base64url 无填充
        assert_ne!(token_digest(&a), token_digest(&b));
    }

    #[test]
    fn secret_box_roundtrip_and_tamper_detection() {
        let key = b"server-secret-from-0600-file";
        let sealed = seal_secret(key, "bind-password-中文".as_bytes()).unwrap();
        assert!(!sealed.contains("bind-password"));
        assert_eq!(
            open_secret(key, &sealed).unwrap(),
            "bind-password-中文".as_bytes()
        );

        // 换个密文字节：必须先被 MAC 拦下，不能解出「差不多对」的口令。
        let mut blob = URL_SAFE_NO_PAD.decode(&sealed).unwrap();
        *blob.last_mut().unwrap() ^= 0x01;
        let tampered = URL_SAFE_NO_PAD.encode(blob);
        assert!(open_secret(key, &tampered).is_err());
        // 换密钥同样失败。
        assert!(open_secret(b"another-key", &sealed).is_err());
        assert!(open_secret(key, "not-base64!!").is_err());
    }

    #[test]
    fn secret_box_uses_a_fresh_nonce() {
        let key = b"k";
        assert_ne!(
            seal_secret(key, b"same").unwrap(),
            seal_secret(key, b"same").unwrap()
        );
    }
}
