//! Web 管理后台专用的全库视图与账号治理。
//!
//! 桌面客户端看到的仓库列表按成员资格过滤（`list_repos`），而管理后台需要
//! 「全库视角」：所有仓库、所有账号、待办统计。这些方法都要求调用者是全局
//! admin，并且只由 `vcs serve` 内置的 HTTP 管理端调用，不暴露给 gRPC 客户端。
//!
//! 账号状态 / 角色变更会同时递增 `token_version`，让已签发的 JWT 立刻失效。

use super::*;
use rusqlite::{params, OptionalExtension};

/// 管理后台的用户行。状态与角色保留数据库文本，前端按文本渲染。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminUser {
    pub user_id: String,
    pub username: String,
    pub display_name: String,
    pub source: String,
    pub status: String,
    pub role: String,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

/// 概览页的统计数字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminOverview {
    pub users: i64,
    pub active_users: i64,
    pub repos: i64,
    pub active_repos: i64,
    pub pending_registrations: i64,
    pub pending_repo_requests: i64,
    pub pending_quota_requests: i64,
    pub objects: i64,
    pub object_bytes: i64,
    pub purged_pending: i64,
}

impl ServerStore {
    fn admin_count(&self, sql: &str) -> Result<i64> {
        self.conn
            .query_row(sql, [], |r| r.get(0))
            .map_err(db("统计管理概览失败"))
    }

    /// 概览数字。只读，不触碰对象池。
    pub fn admin_overview(&self, actor: &Session) -> Result<AdminOverview> {
        self.require_admin_action(actor, "查看管理概览")?;
        let (objects, object_bytes): (i64, i64) = self
            .conn
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(stored_size), 0) FROM objects WHERE pending_delete = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(db("统计对象池失败"))?;
        Ok(AdminOverview {
            users: self.admin_count("SELECT COUNT(*) FROM users")?,
            active_users: self.admin_count("SELECT COUNT(*) FROM users WHERE status = 'active'")?,
            repos: self.admin_count("SELECT COUNT(*) FROM repos")?,
            active_repos: self.admin_count("SELECT COUNT(*) FROM repos WHERE status = 'active'")?,
            pending_registrations: self.admin_count(
                "SELECT COUNT(*) FROM registration_requests WHERE status = 'pending'",
            )?,
            pending_repo_requests: self.admin_count(
                "SELECT COUNT(*) FROM repo_creation_requests WHERE status = 'pending'",
            )?,
            pending_quota_requests: self
                .admin_count("SELECT COUNT(*) FROM repo_quota_requests WHERE status = 'pending'")?,
            objects,
            object_bytes,
            purged_pending: self
                .admin_count("SELECT COUNT(*) FROM purged_paths WHERE state = 'pending'")?,
        })
    }

    /// 全库用户列表（管理视角，不按成员资格过滤）。
    pub fn admin_list_users(&self, actor: &Session, limit: usize) -> Result<Vec<AdminUser>> {
        self.require_admin_action(actor, "查看用户列表")?;
        let mut stmt = self
            .conn
            .prepare(
                "SELECT user_id, username, display_name, source, status, role,
                        created_at, last_login_at
                 FROM users ORDER BY created_at DESC LIMIT ?1",
            )
            .map_err(db("查询用户列表失败"))?;
        let rows = stmt
            .query_map(params![limit.clamp(1, 500) as i64], |r| {
                Ok(AdminUser {
                    user_id: r.get(0)?,
                    username: r.get(1)?,
                    display_name: r.get(2)?,
                    source: r.get(3)?,
                    status: r.get(4)?,
                    role: r.get(5)?,
                    created_at: r.get(6)?,
                    last_login_at: r.get(7)?,
                })
            })
            .map_err(db("查询用户列表失败"))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db("读取用户列表失败"))?);
        }
        Ok(out)
    }

    /// 全库仓库列表。`filter` 为空时返回所有状态，便于管理端停用 / 归档仓库。
    pub fn admin_list_repos(
        &self,
        actor: &Session,
        filter: Option<RepoStatus>,
    ) -> Result<Vec<RepoInfo>> {
        self.require_admin_action(actor, "查看全库仓库列表")?;
        let sql = match filter {
            Some(_) => {
                "SELECT repo_id, name, head_rev, status, owner_user_id FROM repos
                 WHERE status = ?1 ORDER BY name"
            }
            None => {
                "SELECT repo_id, name, head_rev, status, owner_user_id FROM repos ORDER BY name"
            }
        };
        let mut stmt = self.conn.prepare(sql).map_err(db("查询全库仓库失败"))?;
        let mapper =
            |r: &rusqlite::Row<'_>| -> rusqlite::Result<(String, String, i64, String, String)> {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            };
        let rows: Vec<(String, String, i64, String, String)> = match filter {
            Some(status) => stmt
                .query_map(params![repo_status_to_str(status)], mapper)
                .map_err(db("查询全库仓库失败"))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db("读取全库仓库失败"))?,
            None => stmt
                .query_map([], mapper)
                .map_err(db("查询全库仓库失败"))?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db("读取全库仓库失败"))?,
        };
        rows.into_iter()
            .map(|(repo_id, name, head_rev, status, owner_user_id)| {
                Ok(RepoInfo {
                    repo_id,
                    name,
                    head_rev: head_rev as u64,
                    status: parse_repo_status(&status)?,
                    owner_user_id,
                })
            })
            .collect()
    }

    /// 启用 / 禁用账号。禁止停用自己，也禁止停用最后一个可用管理员。
    pub fn admin_set_user_status(
        &self,
        actor: &Session,
        user_id: &str,
        active: bool,
    ) -> Result<()> {
        self.require_admin_action(actor, "启用或禁用账号")?;
        if actor.user_id == user_id {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "不能停用自己的账号",
            ));
        }
        let role: Option<String> = self
            .conn
            .query_row(
                "SELECT role FROM users WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询账号失败"))?;
        let role = role.ok_or_else(|| Error::new(ErrorCode::UserNotFound, "用户不存在"))?;
        if !active && role == "admin" {
            let admins = self.admin_count(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status = 'active'",
            )?;
            if admins <= 1 {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "不能停用最后一个可用管理员",
                ));
            }
        }
        let status = if active { "active" } else { "disabled" };
        self.conn
            .execute(
                "UPDATE users SET status = ?1, token_version = token_version + 1
                 WHERE user_id = ?2",
                params![status, user_id],
            )
            .map_err(db("更新账号状态失败"))?;
        self.audit(
            "",
            &actor.user_id,
            "admin.user_status",
            user_id,
            Some(status),
        )?;
        Ok(())
    }

    /// 调整全局角色。禁止改自己，禁止把最后一个可用管理员降级。
    pub fn admin_set_user_role(&self, actor: &Session, user_id: &str, role: Role) -> Result<()> {
        self.require_admin_action(actor, "调整全局角色")?;
        if actor.user_id == user_id {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "不能修改自己的全局角色",
            ));
        }
        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT role FROM users WHERE user_id = ?1",
                params![user_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db("查询账号失败"))?;
        let current = current.ok_or_else(|| Error::new(ErrorCode::UserNotFound, "用户不存在"))?;
        if current == "admin" && role != Role::Admin {
            let admins = self.admin_count(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status = 'active'",
            )?;
            if admins <= 1 {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "不能把最后一个可用管理员降级",
                ));
            }
        }
        let next = role_to_str(role);
        self.conn
            .execute(
                "UPDATE users SET role = ?1, token_version = token_version + 1
                 WHERE user_id = ?2",
                params![next, user_id],
            )
            .map_err(db("更新全局角色失败"))?;
        self.audit("", &actor.user_id, "admin.user_role", user_id, Some(next))?;
        Ok(())
    }
}

fn repo_status_to_str(status: RepoStatus) -> &'static str {
    match status {
        RepoStatus::Active => "active",
        RepoStatus::Disabled => "disabled",
        RepoStatus::Archived => "archived",
    }
}
