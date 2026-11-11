//! svn-next CLI —— M1/M2 验收用客户端。
//!
//! 这一版故意直接打开服务端存储目录（本地直连），目标是先把**版本语义**跑对：
//! 检出、提交、离线状态比对、离线 diff、日志。等语义稳定后再套上 gRPC 与 Electron，
//! 避免「传输层 bug」和「语义 bug」混在一起排查。
//!
//! 已具备的能力都对应 proto 里的正式接口，命令名与 RPC 一一对应：
//!   init-admin → BootstrapAdmin       login → Login
//!   repo-create → CreateRepoDirect    import / commit → BeginCommit + PushObjects + FinalizeCommit
//!   checkout → Checkout               update → GetChanges
//!   log → GetLog                      status / diff / revert → 纯本地，不访问服务端

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use vcs_core::{hash_bytes, ChangeKind, ObjectStore, WorkingCopy};
use vcs_server::{PendingChange, ServerStore, Session};

#[derive(Parser)]
#[command(name = "vcs", version, about = "svn-next 命令行客户端（M1）")]
struct Cli {
    /// 服务端存储目录
    #[arg(long, global = true, env = "VCS_STORE", default_value = ".vcs-server")]
    store: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 引导管理员账号（仅限空库，执行一次）
    InitAdmin {
        #[arg(long)]
        username: String,
        /// 姓名
        #[arg(long)]
        name: String,
        /// 留空则交互输入（不回显）
        #[arg(long)]
        password: Option<String>,
    },
    /// 登录并缓存 token
    Login {
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: Option<String>,
        /// 用已有 PAT 登录（CI 场景，避免把密码写进脚本）
        #[arg(long)]
        pat: Option<String>,
    },
    /// 清除本地缓存的 token
    Logout,
    /// 管理员直通创建仓库
    RepoCreate {
        name: String,
        /// 单仓库软上限（字节，0 表示继承全局默认）
        #[arg(long, default_value_t = 0)]
        soft: u64,
        /// 单仓库硬上限（字节，0 表示继承全局默认）
        #[arg(long, default_value_t = 0)]
        hard: u64,
    },
    /// 把本地目录导入为下一个版本
    Import {
        repo: String,
        dir: PathBuf,
        #[arg(short, long, default_value = "")]
        message: String,
    },
    /// 查看提交历史
    Log {
        repo: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
    },
    /// 检出到本地目录
    Checkout {
        repo: String,
        dir: PathBuf,
        /// 指定版本，默认取 head
        #[arg(short, long)]
        rev: Option<u64>,
    },
    /// 离线查看工作副本状态
    Status {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// 忽略 mtime 快路径，对所有条目重算哈希
        #[arg(long)]
        full: bool,
    },
    /// 离线查看工作副本差异
    Diff {
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// 把工作副本的本地变更提交为下一个版本（两阶段：BeginCommit → Push → Finalize）
    Commit {
        #[arg(default_value = ".")]
        dir: PathBuf,
        #[arg(short, long)]
        message: String,
    },
    /// 把服务端的新版本合并到工作副本
    Update {
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// 更新到指定版本，默认取 head
        #[arg(short, long)]
        rev: Option<u64>,
    },
    /// 撤销本地改动（从基线恢复），不带路径表示全部
    Revert {
        paths: Vec<String>,
        #[arg(short, long, default_value = ".")]
        dir: PathBuf,
    },
    /// 管理员创建本地账号
    UserCreate {
        #[arg(long)]
        username: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        password: Option<String>,
        /// 全局角色：admin / writer / readonly
        #[arg(long, default_value = "writer")]
        role: String,
    },
    /// 添加仓库成员
    MemberAdd {
        repo: String,
        #[arg(long)]
        username: String,
        /// owner / maintainer / writer / reader
        #[arg(long, default_value = "writer")]
        role: String,
    },
    /// 列出仓库成员
    MemberList { repo: String },
    /// 仓库配额与用量
    Info { repo: String },
    /// 输出某个版本的文件内容
    Cat {
        repo: String,
        path: String,
        #[arg(short, long)]
        rev: Option<u64>,
    },
    /// 预估清理这些目录能释放多少空间（dry-run，不改任何数据）
    Estimate { repo: String, paths: Vec<String> },
    /// 清理目录：删除提交 + 路径裁剪 + GC
    Purge {
        repo: String,
        paths: Vec<String>,
        /// 跳过回收站宽限期，立即释放（磁盘已满时用这个）
        #[arg(long)]
        immediate: bool,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// 手动执行一次空间回收（共享对象池是全局的，扫的是全库）
    Gc { repo: String },
    /// 查询 GC 任务状态
    GcStatus { repo: String, job_id: String },
    /// 执行一次后台维护（转正到期清理 + 全库 GC）
    Maintenance,
    /// 列出保留策略
    RetentionList { repo: String },
    /// 新建 / 更新保留策略
    RetentionSet {
        repo: String,
        /// 顶层目录名的正则，例如 ^20\d{4}$
        #[arg(long)]
        pattern: String,
        /// 保留最近 N 个目录（按名称倒序）
        #[arg(long, default_value_t = 0)]
        keep_months: u32,
        /// 最多保留多少字节
        #[arg(long, default_value_t = 0)]
        max_bytes: u64,
        /// 超过 N 天未修改就清理
        #[arg(long, default_value_t = 0)]
        max_age_days: u32,
        /// 更新已有策略
        #[arg(long, default_value = "")]
        id: String,
        /// 停用该策略
        #[arg(long)]
        disabled: bool,
        /// 策略默认只预览不执行；加这个开关才会真的删数据
        #[arg(long)]
        execute: bool,
    },
    /// 删除保留策略
    RetentionRm { repo: String, policy_id: String },
    /// 评估并执行保留策略
    RetentionApply {
        repo: String,
        /// 只预览候选，不改动数据
        #[arg(long)]
        dry_run: bool,
        /// 执行时跳过回收站宽限期
        #[arg(long)]
        immediate: bool,
    },
    /// 查看回收站（已执行的清理记录）
    Purged { repo: String },
    /// 撤销一次清理，把内容恢复到最新版本
    Unpurge { repo: String, purge_id: String },
    /// 列出可清理目录候选（按独占大小排序）
    Candidates {
        repo: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// 自助注册（免登录，不收集密码）
    Register {
        #[arg(long)]
        username: String,
        /// 姓名
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// 查看注册申请（仅管理员）
    RegisterList {
        /// pending / approved / rejected / expired，留空为全部
        #[arg(long, default_value = "")]
        state: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 审批通过注册申请，输出一次性设置密码 token（仅管理员）
    RegisterApprove {
        request_id: String,
        /// readonly / writer / admin
        #[arg(long, default_value = "readonly")]
        role: String,
    },
    /// 驳回注册申请（仅管理员，必须写原因）
    RegisterReject {
        request_id: String,
        #[arg(long)]
        reason: String,
    },
    /// 用一次性 token 设置初始密码（免登录）
    SetPassword {
        #[arg(long)]
        token: String,
        #[arg(long)]
        password: Option<String>,
    },
    /// 提交仓库创建申请
    RepoRequest {
        name: String,
        #[arg(long, default_value = "")]
        reason: String,
        /// 期望配额（字节，0 表示由管理员核定或继承全局默认）
        #[arg(long, default_value_t = 0)]
        quota: u64,
    },
    /// 查看仓库创建申请（管理员看全部，普通用户只看自己的）
    RepoRequestList {
        #[arg(long, default_value = "")]
        state: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 审批仓库申请（仅管理员）
    RepoApprove {
        request_id: String,
        /// 核定硬上限（0 表示继承全局默认）
        #[arg(long, default_value_t = 0)]
        hard: u64,
        /// 核定软上限（0 表示按硬上限 85% 推导）
        #[arg(long, default_value_t = 0)]
        soft: u64,
        #[arg(long, default_value = "")]
        note: String,
    },
    /// 驳回仓库申请（仅管理员，必须写原因）
    RepoReject {
        request_id: String,
        #[arg(long)]
        reason: String,
    },
    /// 创建访问令牌（明文只显示一次）
    PatCreate {
        #[arg(long)]
        name: String,
        /// 逗号分隔：read / write / admin，留空为 read
        #[arg(long, default_value = "")]
        scopes: String,
        /// 有效天数，0 表示不过期
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// 列出我参与的仓库
    RepoList,
    /// 移除仓库成员（owner / maintainer）
    MemberRemove {
        repo: String,
        #[arg(long)]
        username: String,
    },
    /// 调整成员角色（owner / maintainer）
    MemberSetRole {
        repo: String,
        #[arg(long)]
        username: String,
        /// owner / maintainer / writer / reader
        #[arg(long)]
        role: String,
    },
    /// 调整单仓库配额（仅管理员）
    RepoQuota {
        repo: String,
        /// 硬上限（字节）
        #[arg(long, default_value_t = 0)]
        hard: u64,
        /// 软上限（字节，0 表示按硬上限 85% 推导）
        #[arg(long, default_value_t = 0)]
        soft: u64,
        /// 清零并恢复继承全局默认
        #[arg(long)]
        inherit: bool,
    },
    /// 变更仓库状态（仅管理员）：active / disabled / archived
    RepoStatus {
        repo: String,
        #[arg(long)]
        status: String,
    },
    /// 列出自己的访问令牌
    PatList,
    /// 撤销访问令牌
    PatRevoke { token_id: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("错误：{err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let store_root = cli.store.clone();

    match cli.cmd {
        Cmd::InitAdmin {
            username,
            name,
            password,
        } => {
            let store = ServerStore::open(&store_root)?;
            let password = match password {
                Some(p) => p,
                None => prompt("密码：")?,
            };
            let user = store.bootstrap_admin(&username, &name, &password)?;
            println!("已创建管理员：{}（{}）", user.username, user.display_name);
            println!("下一步：vcs login --username {}", user.username);
        }

        Cmd::Login {
            username,
            password,
            pat,
        } => {
            let store = ServerStore::open(&store_root)?;
            if let Some(pat) = pat {
                // PAT 已经在服务端签发过，这里只校验有效并缓存，不再走密码流程。
                let session = store.authenticate(&pat)?;
                save_token(&store_root, &pat, &session.username)?;
                println!(
                    "已使用访问令牌登录：{}（{}，角色 {}）",
                    session.username,
                    scope_label(&session),
                    role_label(session.role)
                );
                return Ok(());
            }
            let password = match password {
                Some(p) => p,
                None => prompt("密码：")?,
            };
            let (token, user) = store.login(&username, &password)?;
            save_token(&store_root, &token, &user.username)?;
            println!(
                "登录成功：{}（{}，角色 {}）",
                user.display_name,
                user.username,
                role_label(user.role)
            );
        }

        Cmd::Logout => {
            let removed = clear_token(&store_root)?;
            println!(
                "{}",
                if removed {
                    "已退出登录"
                } else {
                    "本地没有缓存的登录状态"
                }
            );
        }

        Cmd::RepoCreate { name, soft, hard } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let repo = store.create_repo_direct(&session, &name, soft, hard)?;
            println!("仓库已创建：{}（{}）", repo.name, repo.repo_id);
            println!("你已被登记为 owner，可直接 import / checkout。");
        }

        Cmd::Import { repo, dir, message } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let outcome = store.import_dir(&session, &repo, &dir, &message)?;
            if outcome.no_changes {
                println!("没有变更：r{} 之后工作内容一致，未生成新版本", outcome.rev);
            } else {
                println!(
                    "已提交 r{}：新增 {}，修改 {}，删除 {}，未变 {}",
                    outcome.rev,
                    outcome.added,
                    outcome.modified,
                    outcome.deleted,
                    outcome.unchanged
                );
                println!(
                    "本次新增对象占用 {} 字节（已去重的对象不重复计费）",
                    outcome.new_object_bytes
                );
            }
        }

        Cmd::Log { repo, limit } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let repo_info = store.find_repo(&repo)?;
            let mut entries = store.log(&session, &repo, limit)?;
            if entries.is_empty() {
                println!("仓库还没有任何提交");
            }
            for entry in entries.iter_mut() {
                entry.changes = store.revision_changes(&repo_info.repo_id, entry.rev)?;
                println!(
                    "r{:<4} {}  {:<10} {} 项变更",
                    entry.rev,
                    format_time(entry.committed_at),
                    entry.author,
                    entry.change_count
                );
                if !entry.message.is_empty() {
                    for line in entry.message.lines() {
                        println!("      {line}");
                    }
                }
                for change in &entry.changes {
                    println!("      {} {}", change_marker(change.kind), change.path);
                }
            }
        }

        Cmd::Checkout { repo, dir, rev } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let (repo_info, rev, files) = store.files_at(&session, &repo, rev)?;
            let mut wc = WorkingCopy::create(&dir)?;
            let written = wc.checkout(&repo_info.repo_id, rev, &files, store.objects())?;
            println!(
                "已检出 {} r{} 到 {}（{} 个文件）",
                repo_info.name,
                rev,
                dir.display(),
                written
            );
        }

        Cmd::Commit { dir, message } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let mut wc = WorkingCopy::open(&dir)?;
            let repo_id = wc
                .get_meta("repo_id")?
                .ok_or("这个工作副本没有关联仓库，请先 checkout")?;

            // 冲突未解决一律拒绝：宁可挡住，也不猜用户想留哪一边。
            if wc.has_conflicts()? {
                for (path, kind) in wc.conflicts()? {
                    println!("C       {path}（{kind} 冲突）");
                }
                return Err("存在未解决的冲突，请先处理（vcs revert 或手工合并）后重试".into());
            }

            let changes = wc.local_changes()?;
            if changes.is_empty() {
                println!("工作副本没有变更（r{}）", wc.base_rev()?);
                return Ok(());
            }

            // 采集内容：边读边校验，防止「扫描后又被改」导致提交的内容与本地不一致。
            let mut payload: Vec<([u8; 32], Vec<u8>)> = Vec::new();
            let mut estimated = 0u64;
            for change in &changes {
                let Some(hash) = change.hash else { continue };
                let data = std::fs::read(wc.root().join(&change.path))?;
                if hash_bytes(&data) != hash {
                    return Err(format!("{} 在扫描后又被修改，请重新提交", change.path).into());
                }
                if !store.objects().exists(&hash) {
                    // 只统计对象池里还没有的内容，去重后不重复计费。
                    estimated += data.len() as u64;
                }
                payload.push((hash, data));
            }

            let pending: Vec<PendingChange> = changes
                .iter()
                .map(|c| PendingChange {
                    path: c.path.clone(),
                    kind: c.kind,
                    entry_kind: c.entry_kind,
                    hash: c.hash,
                    size: c.size,
                })
                .collect();

            let base_rev = wc.base_rev()?;
            let commit =
                store.begin_commit(&session, &repo_id, base_rev, &pending, &message, estimated)?;

            for batch in batch_payload(&payload) {
                store.push_objects(&session, &commit.session_id, batch)?;
            }

            let outcome = match store.finalize_commit(&session, &commit.session_id) {
                Ok(outcome) => outcome,
                Err(err) => {
                    // 失败就显式放弃会话，别把半截上传留给 2 小时 TTL。
                    let _ = store.abort_commit(&session, &commit.session_id);
                    return Err(Box::new(err));
                }
            };

            // 本地收尾：基线刷新，否则 status 会一直认为这些文件「待提交」。
            wc.mark_committed(outcome.rev, &changes, store.objects())?;

            println!(
                "已提交 r{}：新增 {}，修改 {}，删除 {}",
                outcome.rev, outcome.added, outcome.modified, outcome.deleted
            );
            println!(
                "本次新增对象占用 {} 字节（已去重的对象不重复计费）",
                outcome.new_object_bytes
            );
        }

        Cmd::Update { dir, rev } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let mut wc = WorkingCopy::open(&dir)?;
            let repo_id = wc
                .get_meta("repo_id")?
                .ok_or("这个工作副本没有关联仓库，请先 checkout")?;

            let info = store.find_repo(&repo_id)?;
            // 混合版本：取最旧的条目版本，而不是 head，否则 revert 掉的差异永远拉不回来。
            let from_rev = wc.oldest_rev()?;
            let target = rev.unwrap_or(info.head_rev);

            if target == from_rev {
                println!("已是最新版本 r{target}");
                return Ok(());
            }
            if target < from_rev {
                return Err(format!(
                    "工作副本已包含 r{from_rev} 的内容，不支持回退到更早的 r{target}"
                )
                .into());
            }

            let changes = store.get_changes(&session, &repo_id, from_rev, target)?;
            let remote: Vec<vcs_core::RemoteChange> = changes
                .iter()
                .map(|c| vcs_core::RemoteChange {
                    path: c.path.clone(),
                    kind: c.kind,
                    entry_kind: c.entry_kind,
                    hash: c.hash,
                    size: c.size,
                })
                .collect();

            let outcome = wc.apply_update(&repo_id, target, &remote, store.objects())?;
            println!(
                "已更新到 r{target}：更新 {}，新增 {}，删除 {}",
                outcome.updated, outcome.added, outcome.removed
            );
            if outcome.conflicts > 0 || outcome.tree_conflicts > 0 {
                println!(
                    "发现 {} 处内容冲突、{} 处树冲突：本地文件保持你的内容未动",
                    outcome.conflicts, outcome.tree_conflicts
                );
                println!("下一步：vcs status 查看，vcs diff 比对，vcs revert <路径> 放弃本地改动");
            }
        }

        Cmd::Revert { paths, dir } => {
            let mut wc = WorkingCopy::open(&dir)?;
            let restored = wc.revert(&paths)?;
            println!("已回退 {restored} 个条目到基线内容");
            if paths.is_empty() && wc.has_conflicts()? {
                // add/add 冲突指向的是未受控文件，revert 不删它——留给用户自己处理。
                for (path, kind) in wc.conflicts()? {
                    println!("仍待处理：{path}（{kind} 冲突）");
                }
            }
            if paths.is_empty() {
                println!("未纳入版本控制的文件不会被删除");
            }
        }

        Cmd::Status { dir, full } => {
            let wc = WorkingCopy::open(&dir)?;
            let entries = wc.status_with(full)?;
            if entries.is_empty() {
                println!("工作副本干净（r{}）", wc.base_rev()?);
                return Ok(());
            }
            let total = entries.len();
            for entry in &entries {
                println!("{}       {}", entry.state.marker(), entry.path);
            }
            println!("共 {total} 项变化");
        }

        Cmd::Diff { dir } => {
            let wc = WorkingCopy::open(&dir)?;
            let diffs = wc.diff()?;
            if diffs.is_empty() {
                println!("没有差异");
                return Ok(());
            }
            for (path, text) in diffs {
                println!("Index: {path}");
                println!("===================================================================");
                print!("{text}");
            }
        }

        Cmd::UserCreate {
            username,
            name,
            password,
            role,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let password = match password {
                Some(p) => p,
                None => prompt("初始密码：")?,
            };
            let user = store.create_user(
                &session,
                &username,
                &name,
                &password,
                parse_global_role(&role)?,
            )?;
            println!(
                "已创建账号：{}（{}，角色 {}）",
                user.username,
                user.display_name,
                role_label(user.role)
            );
        }

        Cmd::MemberAdd {
            repo,
            username,
            role,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.add_member(&session, &repo, &username, parse_member_role(&role)?)?;
            println!("已把 {username} 加入 {repo}，角色 {role}");
        }

        Cmd::MemberList { repo } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let members = store.list_members(&session, &repo)?;
            println!("用户名       姓名       角色");
            for m in members {
                println!(
                    "{:<12} {:<10} {}",
                    m.username,
                    m.display_name,
                    member_role_label(m.role)
                );
            }
        }

        Cmd::Info { repo } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let info = store.find_repo(&repo)?;
            store.member_role(&info.repo_id, &session.user_id)?;
            let usage = store.usage(&info.repo_id)?;
            println!("仓库       : {}（{}）", info.name, info.repo_id);
            println!("head       : r{}", info.head_rev);
            println!("文件数     : {}", usage.file_count);
            println!(
                "逻辑大小   : {} 字节（当前版本所有文件之和）",
                usage.logical_bytes
            );
            println!(
                "对象池占用 : {} 字节（{} 个对象，全库共享）",
                usage.used_bytes, usage.object_count
            );
            println!(
                "生效配额   : 软 {} / 硬 {}（来源：{}）",
                usage.quota.soft_bytes,
                usage.quota.hard_bytes,
                quota_source_label(usage.quota.source)
            );
        }

        Cmd::Cat { repo, path, rev } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let data = store.read_file(&session, &repo, rev, &path)?;
            use std::io::Write;
            std::io::stdout().write_all(&data)?;
        }

        Cmd::Estimate { repo, paths } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let estimate = store.estimate_purge(&session, &repo, &paths)?;
            println!("清理目标   : {}", estimate.paths.join(", "));
            println!(
                "逻辑大小   : {}（这些路径当前内容之和）",
                human_bytes_exact(estimate.logical_bytes)
            );
            println!(
                "可释放     : {}（仅这些路径独占、删掉就能回收的部分）",
                human_bytes_exact(estimate.reclaimable_bytes)
            );
            println!(
                "不会释放   : {}（被其它目录或其它仓库共享）",
                human_bytes_exact(estimate.shared_bytes)
            );
            println!("影响版本数 : {}", estimate.affected_revisions);
            if !estimate.shared_paths.is_empty() {
                println!("共享引用示例：");
                for path in &estimate.shared_paths {
                    println!("    {path}");
                }
            }
            println!(
                "这是 dry-run，没有改动任何数据。确认后执行：vcs purge {repo} {} --immediate",
                estimate.paths.join(" ")
            );
        }

        Cmd::Purge {
            repo,
            paths,
            immediate,
            reason,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let outcome = store.purge(&session, &repo, &paths, immediate, &reason)?;
            println!(
                "已删除 {} 个路径并生成 r{}（清理标记 {}）",
                outcome.deleted, outcome.rev, outcome.purge_id
            );
            println!(
                "预估可释放 {}，其中共享 {} 不会被回收",
                human_bytes_exact(outcome.estimate.reclaimable_bytes),
                human_bytes_exact(outcome.estimate.shared_bytes)
            );
            if immediate {
                let status = store.gc_status(&session, &repo, &outcome.gc_job_id)?;
                println!(
                    "GC {}：{}，清扫 {} 个对象，实际释放 {}",
                    status.job_id,
                    status.state,
                    status.swept_objects,
                    human_bytes_exact(status.reclaimed_bytes)
                );
            } else {
                println!(
                    "已进入回收站，7 天后由 GC 释放；需要立刻腾空间请加 --immediate。任务：{}",
                    outcome.gc_job_id
                );
            }
        }

        Cmd::Gc { repo } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let status = store.run_gc(&session, &repo)?;
            println!(
                "GC {}：{}，标记 {} 个、清扫 {} 个对象，释放 {}",
                status.job_id,
                status.state,
                status.marked_objects,
                status.swept_objects,
                human_bytes(status.reclaimed_bytes)
            );
        }

        Cmd::GcStatus { repo, job_id } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let status = store.gc_status(&session, &repo, &job_id)?;
            println!("任务   : {}", status.job_id);
            println!("状态   : {}（{}%）", status.state, status.progress_percent);
            println!("已标记 : {}", status.marked_objects);
            println!("已清扫 : {}", status.swept_objects);
            println!("已释放 : {}", human_bytes(status.reclaimed_bytes));
            if let Some(err) = status.error {
                println!("错误   : {err}");
            }
        }

        Cmd::Maintenance => {
            let store = ServerStore::open(&store_root)?;
            let report = store.run_maintenance()?;
            println!(
                "维护完成：保留策略清理 {} 个仓库（{} 个目录），转正 {} 条清理记录，\
                 清扫 {} 个对象，释放 {}（共 {} 个仓库）",
                report.retention_repos,
                report.retention_paths,
                report.promoted_purges,
                report.swept_objects,
                human_bytes_exact(report.reclaimed_bytes),
                report.repo_count
            );
            // 维护任务是系统级的（不属于任何仓库），因此不进 gc-status 的仓库维度查询。
            println!("任务号：{}（系统级任务，不归属任何仓库）", report.gc_job_id);
        }

        Cmd::RetentionList { repo } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let policies = store.list_retention_policies(&session, &repo)?;
            if policies.is_empty() {
                println!("没有配置保留策略");
                return Ok(());
            }
            println!("策略号              模式                参数                          状态");
            for policy in policies {
                let mut rules = Vec::new();
                if policy.keep_months > 0 {
                    rules.push(format!("保留 {} 个", policy.keep_months));
                }
                if policy.max_bytes > 0 {
                    rules.push(format!("上限 {}", human_bytes(policy.max_bytes)));
                }
                if policy.max_age_days > 0 {
                    rules.push(format!("{} 天未改即清理", policy.max_age_days));
                }
                let state = match (policy.enabled, policy.dry_run) {
                    (false, _) => "已停用",
                    (true, true) => "仅预览",
                    (true, false) => "已启用",
                };
                println!(
                    "{:<18} {:<20} {:<28} {}",
                    policy.policy_id,
                    policy.path_pattern,
                    rules.join("、"),
                    state
                );
            }
        }

        Cmd::RetentionSet {
            repo,
            pattern,
            keep_months,
            max_bytes,
            max_age_days,
            id,
            disabled,
            execute,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let policy = store.upsert_retention_policy(
                &session,
                &repo,
                vcs_server::RetentionPolicyInput {
                    policy_id: if id.is_empty() { None } else { Some(id) },
                    path_pattern: pattern,
                    keep_months,
                    max_bytes,
                    max_age_days,
                    enabled: !disabled,
                    // 默认 dry_run：删数据这件事必须显式要求。
                    dry_run: !execute,
                },
            )?;
            println!(
                "已保存策略 {}（{}）{}",
                policy.policy_id,
                policy.path_pattern,
                if policy.dry_run {
                    "：仅预览，不会真的删除"
                } else {
                    "：已启用"
                }
            );
        }

        Cmd::RetentionRm { repo, policy_id } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.delete_retention_policy(&session, &repo, &policy_id)?;
            println!("已删除策略 {policy_id}");
        }

        Cmd::RetentionApply {
            repo,
            dry_run,
            immediate,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let outcome = store.apply_retention(&session, &repo, dry_run, immediate)?;
            if outcome.candidates.is_empty() {
                println!("所有目录都满足保留策略，没有需要清理的内容");
                return Ok(());
            }
            println!("候选目录：{}", outcome.candidates.join("、"));
            println!("预计释放：{}", human_bytes_exact(outcome.reclaimable_bytes));
            if dry_run {
                println!("这是预览，没有改动任何数据。确认后执行：vcs retention-apply {repo} --immediate");
            } else {
                println!(
                    "已生成删除提交 r{}（清理记录可在 vcs purged 查看）",
                    outcome.new_rev
                );
            }
        }

        Cmd::Purged { repo } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let entries = store.list_purged_paths(&session, &repo)?;
            if entries.is_empty() {
                println!("回收站是空的：还没有执行过清理");
                return Ok(());
            }
            println!("路径              状态       清理时间              执行者");
            for entry in entries {
                println!(
                    "{:<16} {:<10} {:<20} {}",
                    entry.path,
                    match entry.state.as_str() {
                        "pending" => "宽限期",
                        "active" => "已生效",
                        "reverted" => "已撤销",
                        other => other,
                    },
                    format_time(entry.created_at),
                    entry.purged_by
                );
                if let Some(reason) = entry.reason {
                    if !reason.is_empty() {
                        println!("      {reason}");
                    }
                }
                if entry.state != "reverted" {
                    println!("      撤销：vcs unpurge {repo} {}", entry.purge_id);
                }
            }
        }

        Cmd::Unpurge { repo, purge_id } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let rev = store.revert_purge(&session, &repo, &purge_id)?;
            println!("已撤销清理，内容恢复到 r{rev}");
            println!("GC 不再跳过这些路径；如果内容此前已被回收，命令会在执行前就失败。");
        }

        Cmd::Candidates { repo, limit } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let candidates = store.list_purge_candidates(&session, &repo, limit)?;
            if candidates.is_empty() {
                println!("没有可清理的目录");
                return Ok(());
            }
            println!("目录              逻辑大小        独占可释放      版本范围");
            for c in candidates {
                println!(
                    "{:<16} {:>12} {:>16} r{}..r{}",
                    c.path,
                    human_bytes(c.logical_bytes),
                    human_bytes(c.unique_bytes),
                    c.first_seen_rev,
                    c.last_modified_rev
                );
            }
        }

        Cmd::Register {
            username,
            name,
            reason,
        } => {
            let store = ServerStore::open(&store_root)?;
            let info = store.register(&username, &name, &reason)?;
            println!(
                "已提交注册申请：{}（申请号 {}）",
                info.username, info.request_id
            );
            println!("状态：待审批，有效期至 {}", format_time(info.expires_at));
            println!("审批通过后管理员会给你一个一次性链接，用它设置密码才能登录。");
        }

        Cmd::RegisterList { state, limit } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let filter = parse_reg_filter(&state)?;
            let list = store.list_registrations(&session, filter, limit)?;
            if list.is_empty() {
                println!("没有符合条件的注册申请");
                return Ok(());
            }
            println!("申请号              用户名        姓名        状态        提交时间");
            for r in list {
                println!(
                    "{:<18} {:<12} {:<10} {:<10} {}",
                    r.request_id,
                    r.username,
                    r.display_name,
                    reg_state_label(r.state),
                    format_time(r.submitted_at)
                );
                if let Some(note) = r.decision_note {
                    println!("    审批意见：{note}");
                }
            }
        }

        Cmd::RegisterApprove { request_id, role } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let approved =
                store.approve_registration(&session, &request_id, parse_global_role(&role)?)?;
            println!("已审批通过，账号 user_id = {}", approved.user_id);
            println!(
                "一次性设置密码令牌（只显示这一次，{} 前有效）：",
                format_time(approved.setup_token_expires_at)
            );
            println!("  {}", approved.setup_token);
            println!("请把它转交申请人，由对方执行：");
            println!(
                "  vcs set-password --token {} --password '<新密码>'",
                approved.setup_token
            );
        }

        Cmd::RegisterReject { request_id, reason } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.reject_registration(&session, &request_id, &reason)?;
            println!("已驳回申请 {request_id}");
        }

        Cmd::SetPassword { token, password } => {
            let store = ServerStore::open(&store_root)?;
            let password = match password {
                Some(p) => p,
                None => prompt("新密码：")?,
            };
            store.set_password(&token, &password)?;
            println!("密码已设置，现在可以登录了：vcs login --username <用户名>");
        }

        Cmd::RepoRequest {
            name,
            reason,
            quota,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let info = store.request_repo(&session, &name, &reason, quota)?;
            println!(
                "已提交仓库创建申请：{}（申请号 {}）",
                info.repo_name, info.request_id
            );
            println!(
                "状态：待审批，有效期至 {}；审批通过后你会自动成为 owner",
                format_time(info.expires_at)
            );
            println!("查询进度：vcs repo-request-list");
        }

        Cmd::RepoRequestList { state, limit } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let filter = parse_creation_filter(&state)?;
            let list = store.list_repo_requests(&session, filter, limit)?;
            if list.is_empty() {
                println!("没有符合条件的仓库申请");
                return Ok(());
            }
            println!("申请号              仓库名            状态        提交时间");
            for r in list {
                println!(
                    "{:<18} {:<16} {:<10} {}",
                    r.request_id,
                    r.repo_name,
                    creation_state_label(r.state),
                    format_time(r.submitted_at)
                );
                if let Some(repo_id) = r.approved_repo_id {
                    println!("    已建仓库：{repo_id}");
                }
                if let Some(note) = r.decision_note {
                    println!("    审批意见：{note}");
                }
            }
        }

        Cmd::RepoApprove {
            request_id,
            hard,
            soft,
            note,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let repo = store.approve_repo_request(&session, &request_id, soft, hard, &note)?;
            println!("仓库已创建：{}（{}）", repo.name, repo.repo_id);
            println!("申请人已成为 owner，可直接 import / checkout。");
        }

        Cmd::RepoReject { request_id, reason } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.reject_repo_request(&session, &request_id, &reason)?;
            println!("已驳回仓库申请 {request_id}");
        }

        Cmd::RepoList => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let repos = store.list_repos(&session, None)?;
            if repos.is_empty() {
                println!("你还没有参与任何仓库。可以申请一个：vcs repo-request <仓库名>");
                return Ok(());
            }
            println!("仓库名            状态        head    仓库号");
            for repo in repos {
                println!(
                    "{:<16} {:<10} r{:<6} {}",
                    repo.name,
                    repo_status_label(repo.status),
                    repo.head_rev,
                    repo.repo_id
                );
            }
        }

        Cmd::MemberRemove { repo, username } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.remove_member(&session, &repo, &username)?;
            println!("已把 {username} 移出 {repo}，其未完成的提交会话已作废");
        }

        Cmd::MemberSetRole {
            repo,
            username,
            role,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let member =
                store.set_member_role(&session, &repo, &username, parse_member_role(&role)?)?;
            println!(
                "已把 {} 的角色改为 {}",
                member.username,
                member_role_label(member.role)
            );
            if !member.role.can_write() {
                println!("该成员已降为只读，手上的提交会话已作废");
            }
        }

        Cmd::RepoQuota {
            repo,
            hard,
            soft,
            inherit,
        } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let effective = store.set_repo_quota(&session, &repo, soft, hard, inherit)?;
            println!(
                "生效配额：软 {} / 硬 {}（来源：{}）",
                human_bytes(effective.soft_bytes),
                human_bytes(effective.hard_bytes),
                quota_source_label(effective.source)
            );
        }

        Cmd::RepoStatus { repo, status } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let repo = store.set_repo_status(&session, &repo, parse_repo_status(&status)?)?;
            println!(
                "仓库 {} 状态已改为 {}",
                repo.name,
                repo_status_label(repo.status)
            );
            if !repo.status.is_usable() {
                println!("该仓库已不可读写，所有在途提交已作废");
            }
        }

        Cmd::PatCreate { name, scopes, days } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let scopes: Vec<String> = scopes
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            let expires_at = if days > 0 {
                Some(now_secs() + days * 86_400)
            } else {
                None
            };
            let (token, token_id) = store.create_pat(&session, &name, &scopes, expires_at)?;
            println!("已创建访问令牌 {token_id}");
            println!("明文只显示这一次，请立即保存：");
            println!("  {token}");
            match expires_at {
                Some(ts) => println!("有效期至 {}", format_time(ts)),
                None => println!("永不过期"),
            }
            println!("在 CI 里使用：vcs login --username <用户名> --pat '<令牌>'");
        }

        Cmd::PatList => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            let tokens = store.list_pats(&session)?;
            if tokens.is_empty() {
                println!("还没有访问令牌");
                return Ok(());
            }
            println!("令牌号              名称              权限            状态      最后使用");
            for t in tokens {
                let state = if t.revoked_at.is_some() {
                    "已撤销"
                } else if t.expires_at.is_some_and(|e| e <= now_secs()) {
                    "已过期"
                } else {
                    "有效"
                };
                println!(
                    "{:<18} {:<16} {:<14} {:<9} {}",
                    t.token_id,
                    t.name,
                    t.scopes.join(","),
                    state,
                    t.last_used_at
                        .map(format_time)
                        .unwrap_or_else(|| "-".into())
                );
            }
        }

        Cmd::PatRevoke { token_id } => {
            let store = ServerStore::open(&store_root)?;
            let session = require_session(&store, &store_root)?;
            store.revoke_pat(&session, &token_id)?;
            println!("已撤销令牌 {token_id}，立即生效");
        }
    }
    Ok(())
}

// ---------------- 提交辅助 ----------------

/// 单批推送的条目上限与字节上限。
///
/// 分批的意义不只是省内存：`push_objects` 是幂等的，中断后重跑同一批不会产生副作用，
/// 所以「大提交失败」可以从头重来而不必担心半截状态。
const PUSH_BATCH_ITEMS: usize = 32;
const PUSH_BATCH_BYTES: usize = 32 * 1024 * 1024;

fn batch_payload(payload: &[(vcs_core::Hash, Vec<u8>)]) -> Vec<&[(vcs_core::Hash, Vec<u8>)]> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (idx, (_, data)) in payload.iter().enumerate() {
        bytes += data.len();
        let full = idx + 1 - start >= PUSH_BATCH_ITEMS || bytes >= PUSH_BATCH_BYTES;
        if full {
            batches.push(&payload[start..=idx]);
            start = idx + 1;
            bytes = 0;
        }
    }
    if start < payload.len() {
        batches.push(&payload[start..]);
    }
    batches
}

// ---------------- 登录态缓存 ----------------

#[derive(Serialize, Deserialize, Default)]
struct CredFile {
    #[serde(default)]
    stores: BTreeMap<String, CredEntry>,
}

#[derive(Serialize, Deserialize)]
struct CredEntry {
    token: String,
    username: String,
}

fn cred_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)?;
    Some(home.join(".vcs").join("credentials.json"))
}

fn store_key(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string()
}

fn read_creds() -> CredFile {
    cred_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

fn save_token(root: &Path, token: &str, username: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = cred_path().ok_or("无法定位用户主目录，无法缓存登录状态")?;
    let mut creds = read_creds();
    creds.stores.insert(
        store_key(root),
        CredEntry {
            token: token.to_string(),
            username: username.to_string(),
        },
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn clear_token(root: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(path) = cred_path() else {
        return Ok(false);
    };
    let mut creds = read_creds();
    let removed = creds.stores.remove(&store_key(root)).is_some();
    if removed {
        std::fs::write(&path, serde_json::to_vec_pretty(&creds)?)?;
    }
    Ok(removed)
}

fn require_session(
    store: &ServerStore,
    root: &Path,
) -> Result<Session, Box<dyn std::error::Error>> {
    let key = store_key(root);
    let entry = read_creds()
        .stores
        .remove(&key)
        .ok_or("尚未登录，请先执行：vcs login --username <用户名>")?;
    // token 由 ServerStore 校验签名、有效期与 token_version，CLI 自己不做信任判断。
    Ok(store.authenticate(&entry.token)?)
}

// ---------------- 输出辅助 ----------------

fn prompt(label: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value = rpassword::prompt_password(label)?;
    if value.is_empty() {
        return Err("密码不能为空".into());
    }
    Ok(value)
}

fn role_label(role: vcs_core::Role) -> &'static str {
    match role {
        vcs_core::Role::Admin => "admin",
        vcs_core::Role::Writer => "writer",
        vcs_core::Role::Readonly => "readonly",
    }
}

fn parse_global_role(value: &str) -> Result<vcs_core::Role, Box<dyn std::error::Error>> {
    match value {
        "admin" => Ok(vcs_core::Role::Admin),
        "writer" => Ok(vcs_core::Role::Writer),
        "readonly" => Ok(vcs_core::Role::Readonly),
        other => Err(format!("未知全局角色：{other}（可选 admin / writer / readonly）").into()),
    }
}

fn parse_member_role(value: &str) -> Result<vcs_core::RepoMemberRole, Box<dyn std::error::Error>> {
    match value {
        "owner" => Ok(vcs_core::RepoMemberRole::Owner),
        "maintainer" => Ok(vcs_core::RepoMemberRole::Maintainer),
        "writer" => Ok(vcs_core::RepoMemberRole::Writer),
        "reader" => Ok(vcs_core::RepoMemberRole::Reader),
        other => Err(
            format!("未知成员角色：{other}（可选 owner / maintainer / writer / reader）").into(),
        ),
    }
}

fn repo_status_label(status: vcs_core::RepoStatus) -> &'static str {
    match status {
        vcs_core::RepoStatus::Active => "active",
        vcs_core::RepoStatus::Disabled => "disabled",
        vcs_core::RepoStatus::Archived => "archived",
    }
}

fn parse_repo_status(value: &str) -> Result<vcs_core::RepoStatus, Box<dyn std::error::Error>> {
    match value.trim() {
        "active" => Ok(vcs_core::RepoStatus::Active),
        "disabled" => Ok(vcs_core::RepoStatus::Disabled),
        "archived" => Ok(vcs_core::RepoStatus::Archived),
        other => Err(format!("未知状态：{other}（可选 active / disabled / archived）").into()),
    }
}

fn member_role_label(role: vcs_core::RepoMemberRole) -> &'static str {
    match role {
        vcs_core::RepoMemberRole::Owner => "owner",
        vcs_core::RepoMemberRole::Maintainer => "maintainer",
        vcs_core::RepoMemberRole::Writer => "writer",
        vcs_core::RepoMemberRole::Reader => "reader",
    }
}

fn quota_source_label(source: vcs_core::QuotaSource) -> &'static str {
    match source {
        vcs_core::QuotaSource::RepoOverride => "单仓库覆盖",
        vcs_core::QuotaSource::GlobalDefault => "全局默认",
        vcs_core::QuotaSource::Fallback => "系统兜底",
    }
}

/// 容量展示：用户在「磁盘满」的时刻只关心 GB/MB 量级，字节数反而看不清。
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 带原始字节数的容量展示：压缩后 46 B 这种反直觉的数字，用户需要看到精确值才敢确认。
fn human_bytes_exact(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{}（{bytes} 字节）", human_bytes(bytes))
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 会话的能力范围展示：JWT 是「本人全部权限」，PAT 才列出具体 scope。
fn scope_label(session: &Session) -> String {
    match &session.scopes {
        None => "全部权限".to_string(),
        Some(list) => format!("scope {}", list.join(",")),
    }
}

fn reg_state_label(state: vcs_core::RegistrationStatus) -> &'static str {
    match state {
        vcs_core::RegistrationStatus::Pending => "待审批",
        vcs_core::RegistrationStatus::Approved => "已通过",
        vcs_core::RegistrationStatus::Rejected => "已驳回",
        vcs_core::RegistrationStatus::Expired => "已过期",
    }
}

fn parse_reg_filter(
    value: &str,
) -> Result<Option<vcs_core::RegistrationStatus>, Box<dyn std::error::Error>> {
    Ok(match value.trim() {
        "" => None,
        "pending" => Some(vcs_core::RegistrationStatus::Pending),
        "approved" => Some(vcs_core::RegistrationStatus::Approved),
        "rejected" => Some(vcs_core::RegistrationStatus::Rejected),
        "expired" => Some(vcs_core::RegistrationStatus::Expired),
        other => {
            return Err(format!(
                "未知状态：{other}（可选 pending / approved / rejected / expired）"
            )
            .into())
        }
    })
}

fn creation_state_label(state: vcs_core::RepoCreationStatus) -> &'static str {
    match state {
        vcs_core::RepoCreationStatus::Pending => "待审批",
        vcs_core::RepoCreationStatus::Approved => "已通过",
        vcs_core::RepoCreationStatus::Rejected => "已驳回",
        vcs_core::RepoCreationStatus::Expired => "已过期",
    }
}

fn parse_creation_filter(
    value: &str,
) -> Result<Option<vcs_core::RepoCreationStatus>, Box<dyn std::error::Error>> {
    Ok(match value.trim() {
        "" => None,
        "pending" => Some(vcs_core::RepoCreationStatus::Pending),
        "approved" => Some(vcs_core::RepoCreationStatus::Approved),
        "rejected" => Some(vcs_core::RepoCreationStatus::Rejected),
        "expired" => Some(vcs_core::RepoCreationStatus::Expired),
        other => {
            return Err(format!(
                "未知状态：{other}（可选 pending / approved / rejected / expired）"
            )
            .into())
        }
    })
}

fn change_marker(kind: ChangeKind) -> char {
    match kind {
        ChangeKind::Add => 'A',
        ChangeKind::Modify => 'M',
        ChangeKind::Delete => 'D',
        ChangeKind::Rename => 'R',
        ChangeKind::Copy => 'C',
    }
}

fn format_time(ts: i64) -> String {
    // 不引 chrono：只做「UTC 秒 → YYYY-MM-DD HH:MM:SS」的最小换算。
    let secs = ts.max(0) as u64;
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Howard Hinnant 的 days→civil 算法，避免为格式化时间引入日期库。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_zero() {
        assert_eq!(format_time(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn formats_known_timestamp() {
        // 2026-11-10 14:51:00 UTC
        assert_eq!(format_time(1_794_322_260), "2026-11-10 14:51:00");
        // 跨闰年边界：2028-02-29 00:00:00 UTC
        assert_eq!(format_time(1_835_395_200), "2028-02-29 00:00:00");
    }
}
