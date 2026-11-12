//! `vcs serve` 内置的 Web 管理后台。
//!
//! 与 gRPC 共用同一个 `Arc<Mutex<ServerStore>>`，不另开进程、不复制数据。
//! 静态页面用 `include_str!` 编进二进制，部署时依然只有一个 `vcs` 文件。
//!
//! 鉴权：管理员用本地账号或 LDAP 登录换取现有 JWT，之后每个 `/api/*` 请求都必须
//! 带 `Authorization: Bearer <token>`，服务端校验 token 且要求全局 admin 角色。
//! 仓库级操作以 `Session::system()`（仅服务端内部可构造）执行，这样管理后台能管
//! 任意仓库，而桌面端 / gRPC 客户端依旧受成员门约束。

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::{json, Value};
use vcs_core::{
    EffectiveQuota, Error, ErrorCode, RegistrationStatus, RepoCreationStatus, RepoMemberRole,
    RepoStatus, Result as CoreResult, Role,
};
use vcs_server::{
    authenticate_ldap, ActorKind, AdminOverview, AdminUser, GcJobStatus, LdapPublicConfig,
    LdapSettings, LdapTestOutcome, MemberInfo, PurgeCandidate, PurgeEstimate, PurgeOutcome,
    PurgedPathInfo, RegistrationInfo, RepoInfo, RepoRequestInfo, RetentionApplyOutcome,
    RetentionPolicyInfo, RetentionPolicyInput, ServerStore, Session, TlsMode, UsageInfo, UserInfo,
};

use crate::Store;

const INDEX_HTML: &str = include_str!("admin_assets/index.html");
const APP_JS: &str = include_str!("admin_assets/app.js");
const STYLES_CSS: &str = include_str!("admin_assets/styles.css");

/// HTTP 管理端的状态：与 gRPC 共享同一把存储锁。
#[derive(Clone)]
pub struct AdminState {
    store: Store,
}

/// 构造管理端路由。`GET /` 是单页界面，`/api/*` 是 JSON 接口。
pub fn app(store: Store) -> Router {
    let state = AdminState { store };
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/styles.css", get(styles))
        .route("/favicon.ico", get(favicon))
        .route("/api/login", post(login))
        .route("/api/session", get(session_info))
        .route("/api/overview", get(overview))
        .route("/api/registrations", get(registrations))
        .route("/api/registrations/:id/approve", post(approve_registration))
        .route("/api/registrations/:id/reject", post(reject_registration))
        .route("/api/repo-requests", get(repo_requests))
        .route("/api/repo-requests/:id/approve", post(approve_repo_request))
        .route("/api/repo-requests/:id/reject", post(reject_repo_request))
        .route("/api/users", get(users).post(create_user))
        .route("/api/users/search", get(search_users))
        .route("/api/users/:id/status", post(set_user_status))
        .route("/api/users/:id/role", post(set_user_role))
        .route("/api/repos", get(repos).post(create_repo))
        .route("/api/repos/:id/status", post(set_repo_status))
        .route("/api/repos/:id/members", get(members).post(add_member))
        .route(
            "/api/repos/:id/members/:username",
            put(set_member_role).delete(remove_member),
        )
        .route("/api/repos/:id/quota", get(repo_quota).put(set_repo_quota))
        .route(
            "/api/quota-defaults",
            get(quota_defaults).put(set_quota_defaults),
        )
        .route("/api/ldap", get(ldap_config).put(save_ldap_config))
        .route("/api/ldap/test", post(test_ldap))
        .route("/api/repos/:id/candidates", get(purge_candidates))
        .route("/api/repos/:id/estimate", post(estimate_purge))
        .route("/api/repos/:id/purge", post(purge))
        .route("/api/repos/:id/purged", get(purged))
        .route("/api/repos/:id/purged/:purge_id/revert", post(revert_purge))
        .route("/api/repos/:id/policies", get(policies).post(upsert_policy))
        .route("/api/repos/:id/policies/:policy_id", delete(delete_policy))
        .route("/api/repos/:id/retention", post(apply_retention))
        .route("/api/repos/:id/gc", post(run_gc))
        .route("/api/repos/:id/gc/:job_id", get(gc_status))
        .with_state(state)
}

/// 绑定并运行 HTTP 管理端，直到进程退出。
#[allow(dead_code)]
pub async fn serve(
    store: Store,
    listen: SocketAddr,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    serve_with_listener(store, listener).await
}

/// 用已绑定好的 listener 运行 HTTP 管理端。
///
/// `vcs serve` 会在拉起 gRPC 之前先完成绑定，这样端口被占用时能立刻失败，
/// 不会出现「gRPC 已经就绪、管理端却悄悄没起来」的半残状态。
pub async fn serve_with_listener(
    store: Store,
    listener: tokio::net::TcpListener,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    axum::serve(listener, app(store)).await?;
    Ok(())
}

// ---------------- 静态资源 ----------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn script() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        APP_JS,
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLES_CSS,
    )
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

// ---------------- 鉴权与统一响应 ----------------

fn failure(status: StatusCode, message: impl Into<String>, code: Option<String>) -> Response {
    (
        status,
        Json(json!({ "ok": false, "message": message.into(), "code": code })),
    )
        .into_response()
}

fn success(data: Value) -> Response {
    Json(json!({ "ok": true, "data": data })).into_response()
}

/// 所有管理接口共用：取 token → 校验 → 必须是全局 admin。
async fn admin_session(
    state: &AdminState,
    headers: &HeaderMap,
) -> std::result::Result<Session, Response> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .unwrap_or(raw)
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(failure(StatusCode::UNAUTHORIZED, "请先登录管理端", None));
    }
    let store = state.store.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        guard.authenticate(&token)
    })
    .await;
    match checked {
        Ok(Ok(session)) if session.role == Role::Admin && session.permits("admin") => Ok(session),
        Ok(Ok(_)) => Err(failure(
            StatusCode::FORBIDDEN,
            "仅管理员可访问 Web 管理端",
            None,
        )),
        Ok(Err(err)) => Err(failure(
            StatusCode::UNAUTHORIZED,
            err.message,
            Some(format!("{:?}", err.code)),
        )),
        Err(join) => Err(failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("鉴权任务异常：{join}"),
            None,
        )),
    }
}

/// 把同步的存储调用挪到 blocking 线程池，并把业务错误统一成 JSON。
async fn run<F>(state: &AdminState, f: F) -> Response
where
    F: FnOnce(&ServerStore) -> CoreResult<Value> + Send + 'static,
{
    let store = state.store.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    })
    .await;
    match outcome {
        Ok(Ok(data)) => success(data),
        Ok(Err(err)) => failure(
            StatusCode::BAD_REQUEST,
            err.message,
            Some(format!("{:?}", err.code)),
        ),
        Err(join) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("管理操作异常：{join}"),
            None,
        ),
    }
}

/// 管理后台以系统身份执行仓库级操作：绕过成员门，但保留真实 user_id 供审计。
fn elevated(session: &Session) -> Session {
    let mut actor = session.clone();
    actor.kind = ActorKind::System;
    actor
}

// ---------------- 请求体小工具 ----------------

fn s(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn u(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_role(text: &str) -> Role {
    match text.trim().to_ascii_lowercase().as_str() {
        "admin" => Role::Admin,
        "writer" => Role::Writer,
        _ => Role::Readonly,
    }
}

fn parse_member_role(text: &str) -> RepoMemberRole {
    match text.trim().to_ascii_lowercase().as_str() {
        "owner" => RepoMemberRole::Owner,
        "maintainer" => RepoMemberRole::Maintainer,
        "reader" => RepoMemberRole::Reader,
        _ => RepoMemberRole::Writer,
    }
}

fn parse_reg_status(text: &str) -> Option<RegistrationStatus> {
    match text.to_ascii_lowercase().as_str() {
        "pending" => Some(RegistrationStatus::Pending),
        "approved" => Some(RegistrationStatus::Approved),
        "rejected" => Some(RegistrationStatus::Rejected),
        "expired" => Some(RegistrationStatus::Expired),
        _ => None,
    }
}

fn parse_request_status(text: &str) -> Option<RepoCreationStatus> {
    match text.to_ascii_lowercase().as_str() {
        "pending" => Some(RepoCreationStatus::Pending),
        "approved" => Some(RepoCreationStatus::Approved),
        "rejected" => Some(RepoCreationStatus::Rejected),
        "expired" => Some(RepoCreationStatus::Expired),
        _ => None,
    }
}

fn parse_repo_status(text: &str) -> RepoStatus {
    match text.to_ascii_lowercase().as_str() {
        "disabled" => RepoStatus::Disabled,
        "archived" => RepoStatus::Archived,
        _ => RepoStatus::Active,
    }
}

// ---------------- 响应转换 ----------------

fn reg_json(r: &RegistrationInfo) -> Value {
    json!({
        "request_id": r.request_id, "username": r.username,
        "display_name": r.display_name, "reason": r.reason,
        "state": format!("{:?}", r.state), "submitted_at": r.submitted_at,
        "decided_at": r.decided_at, "decided_by": r.decided_by,
        "decision_note": r.decision_note, "expires_at": r.expires_at,
    })
}

fn repo_request_json(r: &RepoRequestInfo) -> Value {
    json!({
        "request_id": r.request_id, "requester_user_id": r.requester_user_id,
        "repo_name": r.repo_name, "reason": r.reason,
        "requested_quota_bytes": r.requested_quota_bytes,
        "state": format!("{:?}", r.state), "submitted_at": r.submitted_at,
        "decided_at": r.decided_at, "decided_by": r.decided_by,
        "decision_note": r.decision_note, "expires_at": r.expires_at,
        "approved_repo_id": r.approved_repo_id,
    })
}

fn repo_json(r: &RepoInfo) -> Value {
    json!({
        "repo_id": r.repo_id, "name": r.name, "head_rev": r.head_rev,
        "status": format!("{:?}", r.status), "owner_user_id": r.owner_user_id,
    })
}

fn user_json(u: &UserInfo) -> Value {
    json!({
        "user_id": u.user_id, "username": u.username,
        "display_name": u.display_name, "role": format!("{:?}", u.role),
        "status": format!("{:?}", u.status),
    })
}

fn admin_user_json(u: &AdminUser) -> Value {
    json!({
        "user_id": u.user_id, "username": u.username,
        "display_name": u.display_name, "source": u.source,
        "status": u.status, "role": u.role,
        "created_at": u.created_at, "last_login_at": u.last_login_at,
    })
}

fn member_json(m: &MemberInfo) -> Value {
    json!({
        "user_id": m.user_id, "username": m.username,
        "display_name": m.display_name, "role": format!("{:?}", m.role),
    })
}

fn quota_json(q: &EffectiveQuota) -> Value {
    json!({
        "soft_bytes": q.soft_bytes, "hard_bytes": q.hard_bytes,
        "source": format!("{:?}", q.source),
    })
}

fn usage_json(u: &UsageInfo) -> Value {
    json!({
        "logical_bytes": u.logical_bytes, "used_bytes": u.used_bytes,
        "object_count": u.object_count, "file_count": u.file_count,
        "quota": quota_json(&u.quota),
    })
}

fn overview_json(o: &AdminOverview) -> Value {
    json!({
        "users": o.users, "active_users": o.active_users,
        "repos": o.repos, "active_repos": o.active_repos,
        "pending_registrations": o.pending_registrations,
        "pending_repo_requests": o.pending_repo_requests,
        "objects": o.objects, "object_bytes": o.object_bytes,
        "purged_pending": o.purged_pending,
    })
}

fn candidate_json(c: &PurgeCandidate) -> Value {
    json!({
        "path": c.path, "logical_bytes": c.logical_bytes,
        "unique_bytes": c.unique_bytes, "first_seen_rev": c.first_seen_rev,
        "last_modified_rev": c.last_modified_rev,
    })
}

fn estimate_json(e: &PurgeEstimate) -> Value {
    json!({
        "paths": e.paths, "logical_bytes": e.logical_bytes,
        "reclaimable_bytes": e.reclaimable_bytes, "shared_bytes": e.shared_bytes,
        "affected_revisions": e.affected_revisions, "shared_paths": e.shared_paths,
    })
}

fn purge_outcome_json(p: &PurgeOutcome) -> Value {
    json!({
        "purge_id": p.purge_id, "rev": p.rev, "gc_job_id": p.gc_job_id,
        "deleted": p.deleted, "estimate": estimate_json(&p.estimate),
    })
}

fn purged_json(p: &PurgedPathInfo) -> Value {
    json!({
        "purge_id": p.purge_id, "path": p.path, "state": p.state,
        "reason": p.reason, "purged_by": p.purged_by,
        "created_at": p.created_at, "grace_until": p.grace_until,
        "activated_at": p.activated_at,
    })
}

fn policy_json(p: &RetentionPolicyInfo) -> Value {
    json!({
        "policy_id": p.policy_id, "path_pattern": p.path_pattern,
        "keep_months": p.keep_months, "max_bytes": p.max_bytes,
        "max_age_days": p.max_age_days, "enabled": p.enabled,
        "dry_run": p.dry_run, "created_at": p.created_at,
    })
}

fn retention_apply_json(a: &RetentionApplyOutcome) -> Value {
    json!({
        "candidates": a.candidates, "reclaimable_bytes": a.reclaimable_bytes,
        "new_rev": a.new_rev,
    })
}

fn gc_json(g: &GcJobStatus) -> Value {
    json!({
        "job_id": g.job_id, "state": g.state,
        "progress_percent": g.progress_percent, "marked_objects": g.marked_objects,
        "swept_objects": g.swept_objects, "reclaimed_bytes": g.reclaimed_bytes,
        "error": g.error,
    })
}

fn ldap_json(c: &LdapPublicConfig) -> Value {
    let cfg = &c.settings;
    json!({
        "host": cfg.host, "port": cfg.port, "tls_mode": cfg.tls_mode.as_str(),
        "base_dn": cfg.base_dn, "bind_dn": cfg.bind_dn,
        "bind_password": "", "bind_password_set": c.bind_password_set,
        "user_filter": cfg.user_filter, "group_filter": cfg.group_filter,
        "attr_username": cfg.attr_username, "attr_display_name": cfg.attr_display_name,
        "enabled": cfg.enabled,
        "group_roles": cfg.group_roles.iter().map(|(dn, role)| {
            format!("{dn}={}", match role {
                Role::Admin => "admin", Role::Writer => "writer", Role::Readonly => "readonly",
            })
        }).collect::<Vec<_>>().join("\n"),
    })
}

fn ldap_test_json(t: &LdapTestOutcome) -> Value {
    json!({ "bound_dn": t.bound_dn, "user_count_sample": t.user_count_sample })
}

/// 把界面表单还原成完整的目录配置；`bind_password` 留空由调用方沿用旧值。
fn ldap_from_body(v: &Value) -> CoreResult<LdapSettings> {
    let mut cfg = LdapSettings {
        host: s(v, "host"),
        port: u(v, "port") as u16,
        tls_mode: TlsMode::parse(&s(v, "tls_mode"))?,
        base_dn: s(v, "base_dn"),
        bind_dn: s(v, "bind_dn"),
        bind_password: s(v, "bind_password"),
        user_filter: s(v, "user_filter"),
        group_filter: s(v, "group_filter"),
        attr_username: s(v, "attr_username"),
        attr_display_name: s(v, "attr_display_name"),
        enabled: b(v, "enabled"),
        group_roles: Vec::new(),
    };
    for line in s(v, "group_roles").lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (dn, role) = line.rsplit_once('=').ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidArgument,
                "组映射每行应为 <组DN>=<admin|writer|readonly>",
            )
        })?;
        cfg.group_roles
            .push((dn.trim().to_string(), parse_role(role)));
    }
    Ok(cfg)
}

fn ldap_with_password(store: &ServerStore, body: &Value) -> CoreResult<LdapSettings> {
    let mut cfg = ldap_from_body(body)?;
    if cfg.bind_password.is_empty() {
        if let Some(previous) = store.ldap_config()? {
            cfg.bind_password = previous.bind_password;
        }
    }
    Ok(cfg)
}

// ---------------- 登录 / 会话 ----------------

async fn login(State(state): State<AdminState>, Json(body): Json<Value>) -> Response {
    let username = s(&body, "username");
    let password = s(&body, "password");
    let source = s(&body, "source");
    run(&state, move |store| {
        let (token, user) = if source.eq_ignore_ascii_case("LDAP") {
            let cfg = store.ldap_config()?.filter(|c| c.enabled).ok_or_else(|| {
                Error::new(ErrorCode::LdapUnavailable, "LDAP 登录尚未配置或未启用")
            })?;
            let profile = authenticate_ldap(&cfg, &username, &password)?;
            store.login_ldap(&profile)?
        } else {
            store.login(&username, &password)?
        };
        if user.role != Role::Admin {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "仅管理员可登录 Web 管理端",
            ));
        }
        Ok(json!({
            "token": token,
            "user": user_json(&user),
        }))
    })
    .await
}

async fn session_info(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    success(json!({
        "user_id": session.user_id,
        "username": session.username,
        "role": format!("{:?}", session.role),
    }))
}

// ---------------- 概览 / 审批 ----------------

async fn overview(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        Ok(overview_json(&store.admin_overview(&session)?))
    })
    .await
}

async fn registrations(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let filter = q.get("state").and_then(|v| parse_reg_status(v));
    run(&state, move |store| {
        let items = store.list_registrations(&session, filter, 200)?;
        Ok(json!({ "items": items.iter().map(reg_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn approve_registration(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let role = parse_role(&s(&body, "role"));
    run(&state, move |store| {
        let out = store.approve_registration(&session, &id, role)?;
        Ok(json!({
            "user_id": out.user_id,
            "setup_token": out.setup_token,
            "setup_token_expires_at": out.setup_token_expires_at,
        }))
    })
    .await
}

async fn reject_registration(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let reason = s(&body, "reason");
    run(&state, move |store| {
        store.reject_registration(&session, &id, &reason)?;
        Ok(json!({}))
    })
    .await
}

async fn repo_requests(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let filter = q.get("state").and_then(|v| parse_request_status(v));
    run(&state, move |store| {
        let items = store.list_repo_requests(&session, filter, 200)?;
        Ok(json!({ "items": items.iter().map(repo_request_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn approve_repo_request(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let soft = u(&body, "soft_bytes");
    let hard = u(&body, "hard_bytes");
    let note = s(&body, "note");
    run(&state, move |store| {
        let repo = store.approve_repo_request(&session, &id, soft, hard, &note)?;
        Ok(json!({ "repo": repo_json(&repo) }))
    })
    .await
}

async fn reject_repo_request(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let reason = s(&body, "reason");
    run(&state, move |store| {
        store.reject_repo_request(&session, &id, &reason)?;
        Ok(json!({}))
    })
    .await
}

// ---------------- 用户 ----------------

async fn users(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.admin_list_users(&session, 500)?;
        Ok(json!({ "items": items.iter().map(admin_user_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn create_user(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let username = s(&body, "username");
    let display_name = s(&body, "display_name");
    let password = s(&body, "password");
    let role = parse_role(&s(&body, "role"));
    run(&state, move |store| {
        Ok(json!({ "user": user_json(&store.create_user(
            &session, &username, &display_name, &password, role,
        )?) }))
    })
    .await
}

async fn search_users(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let _session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let query = q.get("q").cloned().unwrap_or_default();
    run(&state, move |store| {
        let items = store.search_users(&query, 20)?;
        Ok(json!({ "items": items.iter().map(|u| json!({
            "user_id": u.user_id, "username": u.username, "display_name": u.display_name,
        })).collect::<Vec<_>>() }))
    })
    .await
}

async fn set_user_status(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let active = b(&body, "active");
    run(&state, move |store| {
        store.admin_set_user_status(&session, &id, active)?;
        Ok(json!({}))
    })
    .await
}

async fn set_user_role(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let role = parse_role(&s(&body, "role"));
    run(&state, move |store| {
        store.admin_set_user_role(&session, &id, role)?;
        Ok(json!({}))
    })
    .await
}

// ---------------- 仓库 / 成员 / 配额 ----------------

async fn repos(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.admin_list_repos(&session, None)?;
        Ok(json!({ "items": items.iter().map(repo_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn create_repo(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let name = s(&body, "name");
    let soft = u(&body, "soft_bytes");
    let hard = u(&body, "hard_bytes");
    run(&state, move |store| {
        Ok(json!({ "repo": repo_json(&store.create_repo_direct(&session, &name, soft, hard)?) }))
    })
    .await
}

async fn set_repo_status(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let status = parse_repo_status(&s(&body, "status"));
    run(&state, move |store| {
        Ok(json!({ "repo": repo_json(&store.set_repo_status(&elevated(&session), &id, status)?) }))
    })
    .await
}

async fn members(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.list_members(&elevated(&session), &id)?;
        Ok(json!({ "items": items.iter().map(member_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn add_member(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let username = s(&body, "username");
    let role = parse_member_role(&s(&body, "role"));
    run(&state, move |store| {
        store.add_member(&elevated(&session), &id, &username, role)?;
        Ok(json!({}))
    })
    .await
}

async fn set_member_role(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((id, username)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let role = parse_member_role(&s(&body, "role"));
    run(&state, move |store| {
        let member = store.set_member_role(&elevated(&session), &id, &username, role)?;
        Ok(json!({ "member": member_json(&member) }))
    })
    .await
}

async fn remove_member(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((id, username)): Path<(String, String)>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        store.remove_member(&elevated(&session), &id, &username)?;
        Ok(json!({}))
    })
    .await
}

async fn repo_quota(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        store.require_admin_action(&session, "查看仓库配额")?;
        let (soft, hard) = store.repo_quota_override(&id)?;
        let (default_soft, default_hard) = store.quota_defaults()?;
        Ok(json!({
            "soft_bytes": soft, "hard_bytes": hard,
            "effective": quota_json(&store.effective_quota(&id)?),
            "usage": usage_json(&store.usage(&id)?),
            "defaults": { "soft_bytes": default_soft, "hard_bytes": default_hard },
        }))
    })
    .await
}

async fn set_repo_quota(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let soft = u(&body, "soft_bytes");
    let hard = u(&body, "hard_bytes");
    let inherit = b(&body, "inherit");
    run(&state, move |store| {
        let effective = store.set_repo_quota(&elevated(&session), &id, soft, hard, inherit)?;
        Ok(json!({ "effective": quota_json(&effective) }))
    })
    .await
}

async fn quota_defaults(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        store.require_admin_action(&session, "查看全局配额")?;
        let (soft, hard) = store.quota_defaults()?;
        Ok(json!({ "soft_bytes": soft, "hard_bytes": hard }))
    })
    .await
}

async fn set_quota_defaults(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let soft = u(&body, "soft_bytes");
    let hard = u(&body, "hard_bytes");
    run(&state, move |store| {
        let effective = store.set_default_quota(&session, soft, hard)?;
        Ok(json!({ "effective": quota_json(&effective) }))
    })
    .await
}

// ---------------- LDAP 目录设置 ----------------

async fn ldap_config(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let cfg = store.ldap_config_public(Some(&session))?;
        Ok(match cfg {
            Some(public) => ldap_json(&public),
            None => json!({
                "host": "", "port": 636, "tls_mode": "ldaps",
                "base_dn": "", "bind_dn": "", "bind_password": "",
                "bind_password_set": false,
                "user_filter": "(uid={username})", "group_filter": "",
                "attr_username": "uid", "attr_display_name": "cn",
                "enabled": false, "group_roles": "",
            }),
        })
    })
    .await
}

async fn save_ldap_config(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let cfg = ldap_with_password(store, &body)?;
        store.set_ldap_config(Some(&session), &cfg)?;
        Ok(json!({}))
    })
    .await
}

async fn test_ldap(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let cfg = ldap_with_password(store, &body)?;
        Ok(ldap_test_json(
            &store.test_ldap_config(Some(&session), &cfg)?,
        ))
    })
    .await
}

// ---------------- 空间治理 ----------------

async fn purge_candidates(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.list_purge_candidates(&elevated(&session), &id, 100)?;
        Ok(json!({ "items": items.iter().map(candidate_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn estimate_purge(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let paths = strings(&body, "paths");
    run(&state, move |store| {
        Ok(estimate_json(&store.estimate_purge(
            &elevated(&session),
            &id,
            &paths,
        )?))
    })
    .await
}

async fn purge(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let paths = strings(&body, "paths");
    let immediate = b(&body, "immediate");
    let reason = s(&body, "reason");
    run(&state, move |store| {
        Ok(purge_outcome_json(&store.purge(
            &elevated(&session),
            &id,
            &paths,
            immediate,
            &reason,
        )?))
    })
    .await
}

async fn purged(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.list_purged_paths(&elevated(&session), &id)?;
        Ok(json!({ "items": items.iter().map(purged_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn revert_purge(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((id, purge_id)): Path<(String, String)>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let rev = store.revert_purge(&elevated(&session), &id, &purge_id)?;
        Ok(json!({ "rev": rev }))
    })
    .await
}

async fn policies(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        let items = store.list_retention_policies(&elevated(&session), &id)?;
        Ok(json!({ "items": items.iter().map(policy_json).collect::<Vec<_>>() }))
    })
    .await
}

async fn upsert_policy(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let policy_id = s(&body, "policy_id");
    let input = RetentionPolicyInput {
        policy_id: if policy_id.is_empty() {
            None
        } else {
            Some(policy_id)
        },
        path_pattern: s(&body, "path_pattern"),
        keep_months: u(&body, "keep_months") as u32,
        max_bytes: u(&body, "max_bytes"),
        max_age_days: u(&body, "max_age_days") as u32,
        enabled: b(&body, "enabled"),
        dry_run: b(&body, "dry_run"),
    };
    run(&state, move |store| {
        Ok(policy_json(&store.upsert_retention_policy(
            &elevated(&session),
            &id,
            input,
        )?))
    })
    .await
}

async fn delete_policy(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((id, policy_id)): Path<(String, String)>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        store.delete_retention_policy(&elevated(&session), &id, &policy_id)?;
        Ok(json!({}))
    })
    .await
}

async fn apply_retention(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let dry_run = b(&body, "dry_run");
    let immediate = b(&body, "immediate");
    run(&state, move |store| {
        Ok(retention_apply_json(&store.apply_retention(
            &elevated(&session),
            &id,
            dry_run,
            immediate,
        )?))
    })
    .await
}

async fn run_gc(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        Ok(gc_json(&store.run_gc(&elevated(&session), &id)?))
    })
    .await
}

async fn gc_status(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path((id, job_id)): Path<(String, String)>,
) -> Response {
    let session = match admin_session(&state, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    run(&state, move |store| {
        Ok(gc_json(&store.gc_status(
            &elevated(&session),
            &id,
            &job_id,
        )?))
    })
    .await
}
