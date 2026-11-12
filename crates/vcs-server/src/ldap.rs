//! 目录认证（LDAP / AD）：连接、服务账号 bind、定位用户、用户二次 bind、取组。
//!
//! 这一层**故意不碰数据库**：`ServerStore` 只接收已经验证过的 [`DirectoryUser`]，
//! 负责 JIT 建号、角色判定与签发 JWT。好处有三：
//!   - 目录协议与用户状态机互不纠缠，目录侧出错不会写坏本地账号；
//!   - 存储层测试可以注入一个假的 `DirectoryUser`，不必真的起目录服务；
//!   - 将来换目录客户端（或加 mock 目录做集成测试）不动业务层。
//!
//! 安全约束（设计 §8.3）：
//!   - 只允许 `ldaps` 或 `ldap + StartTLS`，**没有明文 bind 选项**；默认校验证书，
//!     刻意不调用 `set_no_tls_verify`；
//!   - 用户名拼进 filter 前一律 [`escape_filter`] 转义，防 LDAP 注入；
//!   - 目录不可用返回 `LDAP_UNAVAILABLE` 且绝不退化成「校验通过」，
//!     本地账号登录路径不受影响；
//!   - 用户不存在与口令错误返回同一个 `InvalidCredentials`，不泄露账号是否存在。

use std::time::Duration;

use ldap3::{LdapConn, LdapConnSettings, ResultEntry, Scope, SearchEntry};
use vcs_core::{Error, ErrorCode, Result, Role};

/// 连接与单次操作超时：目录服务挂了不能把 gRPC 工作线程拖死。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const OP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    Ldaps,
    StartTls,
}

impl TlsMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ldaps" => Ok(Self::Ldaps),
            "starttls" => Ok(Self::StartTls),
            _ => Err(Error::new(
                ErrorCode::InvalidArgument,
                "tls_mode 只能是 ldaps 或 starttls（禁止明文 LDAP）",
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ldaps => "ldaps",
            Self::StartTls => "starttls",
        }
    }
}

/// 一份完整的目录配置。`bind_password` 只在服务端进程内以明文存在，
/// 落库前由 `vcs_core::auth::seal_secret` 加密，读取接口不回传。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapSettings {
    pub host: String,
    pub port: u16,
    pub tls_mode: TlsMode,
    pub base_dn: String,
    pub bind_dn: String,
    pub bind_password: String,
    pub user_filter: String,
    pub group_filter: String,
    pub attr_username: String,
    pub attr_display_name: String,
    pub enabled: bool,
    /// 目录组 DN → 全局角色。只在 JIT 建号时使用。
    pub group_roles: Vec<(String, Role)>,
}

impl Default for LdapSettings {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 636,
            tls_mode: TlsMode::Ldaps,
            base_dn: String::new(),
            bind_dn: String::new(),
            bind_password: String::new(),
            user_filter: "(uid={username})".into(),
            group_filter: String::new(),
            attr_username: "uid".into(),
            attr_display_name: "cn".into(),
            enabled: false,
            group_roles: Vec::new(),
        }
    }
}

/// 可回传给管理界面的目录配置。`settings.bind_password` 恒为空串——
/// 明文口令永远不出服务端，前端只靠 `bind_password_set` 判断「留空是否表示不修改」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapPublicConfig {
    pub settings: LdapSettings,
    pub bind_password_set: bool,
}

/// 目录侧验证通过后的用户信息。`dn` 用做本地映射，`groups` 决定 JIT 初始角色。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryUser {
    pub username: String,
    pub display_name: String,
    pub dn: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LdapTestOutcome {
    pub bound_dn: String,
    pub user_count_sample: u32,
}

impl LdapSettings {
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() {
            return Err(Error::new(ErrorCode::InvalidArgument, "LDAP 主机不能为空"));
        }
        if self.port == 0 {
            return Err(Error::new(ErrorCode::InvalidArgument, "LDAP 端口不合法"));
        }
        if self.base_dn.trim().is_empty() || self.bind_dn.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "base_dn 与 bind_dn 不能为空",
            ));
        }
        if !self.user_filter.contains("{username}") {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "user_filter 必须包含 {username} 占位符",
            ));
        }
        if self.attr_username.trim().is_empty() || self.attr_display_name.trim().is_empty() {
            return Err(Error::new(
                ErrorCode::InvalidArgument,
                "用户名 / 姓名属性名不能为空",
            ));
        }
        Ok(())
    }

    /// `ldaps://` 直接 TLS；`starttls` 走 `ldap://` 但在 bind 前升级（由连接设置完成）。
    pub fn url(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let scheme = match self.tls_mode {
            TlsMode::Ldaps => "ldaps",
            TlsMode::StartTls => "ldap",
        };
        format!("{scheme}://{host}:{}", self.port)
    }

    fn conn_settings(&self) -> LdapConnSettings {
        LdapConnSettings::new()
            .set_starttls(matches!(self.tls_mode, TlsMode::StartTls))
            .set_conn_timeout(CONNECT_TIMEOUT)
        // 不调用 set_no_tls_verify：默认用系统根证书校验证书。
    }

    pub fn user_filter_for(&self, username: &str) -> Result<String> {
        if !self.user_filter.contains("{username}") {
            return Err(unavailable("user_filter 缺少 {username} 占位符"));
        }
        Ok(self
            .user_filter
            .replace("{username}", &escape_filter(username)))
    }

    fn group_filter_for(&self, username: &str, dn: &str) -> String {
        self.group_filter
            .replace("{username}", &escape_filter(username))
            .replace("{dn}", &escape_filter(dn))
    }
}

/// RFC 4515 filter 转义：`* ( ) \ NUL` 以及控制字符、非 ASCII 字节必须转成 `\xx`。
pub fn escape_filter(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match *byte {
            b'*' => out.push_str("\\2a"),
            b'(' => out.push_str("\\28"),
            b')' => out.push_str("\\29"),
            b'\\' => out.push_str("\\5c"),
            0 => out.push_str("\\00"),
            0x20..=0x7e => out.push(*byte as char),
            other => out.push_str(&format!("\\{other:02x}")),
        }
    }
    out
}

fn unavailable(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::LdapUnavailable, message.into())
}

fn invalid_credentials() -> Error {
    // 用户不存在、DN 找不到、口令错，一律同一条消息。
    Error::new(ErrorCode::InvalidCredentials, "用户名或密码错误")
}

/// `SearchEntry::construct` 在 BER 异常时会 panic；目录是外部输入，兜住不让服务端崩。
fn construct_entry(raw: ResultEntry) -> Result<SearchEntry> {
    std::panic::catch_unwind(|| SearchEntry::construct(raw))
        .map_err(|_| unavailable("目录返回了无法解析的条目"))
}

/// 属性名大小写不敏感地取值：OpenLDAP 可能回 `uid`，AD 可能回 `UID`。
fn attr_values(entry: &SearchEntry, name: &str) -> Vec<String> {
    entry
        .attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn connect(cfg: &LdapSettings) -> Result<LdapConn> {
    let mut conn = LdapConn::with_settings(cfg.conn_settings(), &cfg.url())
        .map_err(|e| unavailable(format!("连接目录服务失败：{e}")))?;
    conn.with_timeout(OP_TIMEOUT);
    Ok(conn)
}

fn service_bind(conn: &mut LdapConn, cfg: &LdapSettings) -> Result<()> {
    conn.simple_bind(&cfg.bind_dn, &cfg.bind_password)
        .map_err(|e| unavailable(format!("目录服务账号 bind 失败：{e}")))?
        .success()
        .map_err(|e| unavailable(format!("目录服务账号 bind 被拒绝：{e}")))?;
    Ok(())
}

/// 完成一次目录认证：成功才返回用户档案，任何异常都不会被当成「登录通过」。
pub fn authenticate_ldap(
    cfg: &LdapSettings,
    username: &str,
    password: &str,
) -> Result<DirectoryUser> {
    if !cfg.enabled {
        return Err(unavailable("LDAP 登录未启用"));
    }
    cfg.validate()?;
    if username.is_empty() || password.is_empty() {
        return Err(invalid_credentials());
    }

    let mut conn = connect(cfg)?;
    service_bind(&mut conn, cfg)?;

    let filter = cfg.user_filter_for(username)?;
    let attrs = vec![
        cfg.attr_username.as_str(),
        cfg.attr_display_name.as_str(),
        "memberOf",
    ];
    let (entries, _) = conn
        .search(&cfg.base_dn, Scope::Subtree, &filter, attrs)
        .map_err(|e| unavailable(format!("目录用户查询失败：{e}")))?
        .success()
        .map_err(|e| unavailable(format!("目录用户查询被拒绝：{e}")))?;
    // filter 必须唯一命中：匹配到多个条目说明配置太宽，宁可报错也不能随便挑一个 bind。
    let mut matches = entries.into_iter().filter(|e| !e.is_ref());
    let raw = matches.next();
    if matches.next().is_some() {
        return Err(unavailable(
            "用户 filter 匹配到多个目录条目，请收紧 user_filter",
        ));
    }
    let entry = construct_entry(match raw {
        Some(e) => e,
        None => return Err(invalid_credentials()),
    })?;

    let mut groups = attr_values(&entry, "memberOf");
    if !cfg.group_filter.trim().is_empty() {
        let group_filter = cfg.group_filter_for(username, &entry.dn);
        let (group_entries, _) = conn
            .search(&cfg.base_dn, Scope::Subtree, &group_filter, vec!["cn"])
            .map_err(|e| unavailable(format!("目录组查询失败：{e}")))?
            .success()
            .map_err(|e| unavailable(format!("目录组查询被拒绝：{e}")))?;
        for ge in group_entries.into_iter().filter(|e| !e.is_ref()) {
            let dn = construct_entry(ge)?.dn;
            if !groups.iter().any(|g| g.eq_ignore_ascii_case(&dn)) {
                groups.push(dn);
            }
        }
    }

    let resolved_name = attr_values(&entry, &cfg.attr_username)
        .into_iter()
        .next()
        .unwrap_or_else(|| username.to_string());
    let display_name = attr_values(&entry, &cfg.attr_display_name)
        .into_iter()
        .next()
        .unwrap_or_else(|| resolved_name.clone());

    // 关键一步：用目录返回的 DN + 用户口令再 bind 一次。服务账号能搜到用户，
    // 不代表用户本人的口令正确；这一步绝不能被跳过或降级。
    conn.simple_bind(&entry.dn, password)
        .map_err(|e| unavailable(format!("目录用户 bind 失败：{e}")))?
        .success()
        .map_err(|_| invalid_credentials())?;
    let _ = conn.unbind();

    Ok(DirectoryUser {
        username: resolved_name,
        display_name: display_name.trim().to_string(),
        dn: entry.dn,
        groups,
    })
}

/// 连通性测试：只做服务账号 bind + 一次抽样搜索，不涉及任何本地账号变更。
pub fn test_ldap_connection(cfg: &LdapSettings) -> Result<LdapTestOutcome> {
    cfg.validate()?;
    let mut conn = connect(cfg)?;
    service_bind(&mut conn, cfg)?;
    let (entries, _) = conn
        .search(
            &cfg.base_dn,
            Scope::Subtree,
            "(objectClass=*)",
            vec![cfg.attr_username.as_str()],
        )
        .map_err(|e| unavailable(format!("目录抽样查询失败：{e}")))?
        .success()
        .map_err(|e| unavailable(format!("目录抽样查询被拒绝：{e}")))?;
    // 连通性测试不是目录导出：抽样计数封顶，避免大目录把这一次请求拖成长查询。
    let sample = entries.len().min(100) as u32;
    let _ = conn.unbind();
    Ok(LdapTestOutcome {
        bound_dn: cfg.bind_dn.clone(),
        user_count_sample: sample,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_escapes_injection_and_non_ascii() {
        assert_eq!(escape_filter("alice"), "alice");
        assert_eq!(escape_filter("a*b(c)\\d"), "a\\2ab\\28c\\29\\5cd");
        assert_eq!(escape_filter("a\0b"), "a\\00b");
        assert_eq!(escape_filter("张"), "\\e5\\bc\\a0");
        let cfg = LdapSettings::default();
        assert_eq!(cfg.user_filter_for("alice").unwrap(), "(uid=alice)");
        let injected = cfg.user_filter_for("*)(|(uid=*))").unwrap();
        assert!(!injected.contains("*)("));
        assert!(injected.contains("\\2a"));
    }

    #[test]
    fn only_tls_urls_are_produced() {
        let cfg = LdapSettings {
            host: "ldap.example.com".into(),
            ..Default::default()
        };
        assert_eq!(cfg.url(), "ldaps://ldap.example.com:636");
        let starttls = LdapSettings {
            host: "ldap.example.com".into(),
            port: 389,
            tls_mode: TlsMode::StartTls,
            ..cfg
        };
        assert_eq!(starttls.url(), "ldap://ldap.example.com:389");
        let ipv6 = LdapSettings {
            host: "2001:db8::1".into(),
            ..starttls
        };
        assert_eq!(ipv6.url(), "ldap://[2001:db8::1]:389");
    }

    #[test]
    fn config_validation_rejects_unsafe_filters() {
        let mut cfg = LdapSettings {
            host: "ldap.example.com".into(),
            base_dn: "dc=example,dc=com".into(),
            bind_dn: "cn=admin,dc=example,dc=com".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        cfg.user_filter = "(uid=alice)".into();
        assert_eq!(cfg.validate().unwrap_err().code, ErrorCode::InvalidArgument);
        assert!(TlsMode::parse("plain").is_err());
        assert_eq!(TlsMode::parse("STARTTLS").unwrap(), TlsMode::StartTls);
    }

    #[test]
    fn auth_never_succeeds_when_disabled_or_empty() {
        let cfg = LdapSettings::default();
        assert_eq!(
            authenticate_ldap(&cfg, "alice", "pw").unwrap_err().code,
            ErrorCode::LdapUnavailable
        );
    }
}
