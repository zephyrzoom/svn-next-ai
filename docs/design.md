# svn-next 设计方案

| 项目 | 内容 |
| --- | --- |
| 版本 | v0.1（M0 契约先行） |
| 日期 | 2026-11-10 |
| 状态 | 待评审 |
| 工作区 | `svn-next-ai` |

## 1. 目标与范围

参考 SVN 的核心语义，用现代技术栈重新设计实现一个集中式版本控制系统：客户端 Electron，服务端 Rust。不追求 SVN 全量功能，只实现检出、提交等核心闭环，但要求工程上可运维、容量可控。

### 1.1 保留的 SVN 语义

- 单一权威服务端，客户端不保存完整历史。
- 全局单调递增版本号 `rN`，一次提交产生一个版本。
- 路径级权限控制。
- 瘦工作副本：只缓存当前基线内容，历史留在服务端。
- 混合版本工作副本：每个条目自带 `base_rev`。
- 锁机制（`needs-lock` 语义）。
- 删除是可回滚的历史事件，不是数据擦除。

### 1.2 明确不做（Out of scope）

- **不兼容 SVN 网络协议，不提供 `svn dump` 或历史仓库导入能力**（2026-11-10 确认移除）。`import` 从本地目录导入首个 revision 属于内容初始化，不是历史迁移，二者不要混淆。
- 不实现 WebDAV / DeltaV、svndiff、`.svn` 目录格式。
- 不实现 `blame`、`externals`、服务端 hook、完整 property / revprop 体系。
- 不引入 Git 式 DAG、本地全历史、分支合并。
- **不提供分支与标签**：没有 `branch` / `tag` / `ref` 概念，也不采用「用 `/branches`、`/tags` 目录约定模拟分支」的做法。仓库只有一条线性主干，版本定位只用全局 `rN`；`checkout -r <rev>` 是按版本号检出历史快照，不是切换分支。用户自建的普通数据目录（如 `202610`、`202611`）属于业务数据，照常支持。
- 不实现独立的 LFS 服务；大文件用 CAS 分块去重与容量治理解决。
- 不提供按月份分区隔离存储布局（`PARTITIONED`）：全仓库统一共享 CAS，不拆分对象池。

### 1.3 MVP 功能集

| 类别 | 命令 |
| --- | --- |
| 仓库 | `RequestRepo` 申请 / 管理员审批、`ListRepos` / `info`、`import`（本地目录导入首个版本）、owner 自助成员管理与仓库状态 |
| 成员 | owner 增删成员 / 调整角色 / 按用户名搜索；仅成员可检出、提交、清理 |
| 读取 | `checkout`（支持 depth / 稀疏路径）、`log`、`status`、`diff` |
| 写入 | `add`、`delete`、`rename`、`commit`、`update`、`revert` |
| 冲突 | 文本三方合并、冲突元数据、客户端三栏解决 |
| 容量 | 空间面板、释放量预估、一键清理、配额预检、保留策略；admin 配置全局默认与单仓库配额 |
| 用户体系 | LDAP / 本地登录、自助注册、管理员审批、PAT 管理 |

## 2. 总体架构

```
Electron 客户端                          Rust 服务端
┌───────────────────────────┐          ┌───────────────────────────┐
│ Renderer  React + TS      │          │ API / Auth / ACL           │
│ Preload   类型化 IPC       │  gRPC    │ RepoService                │
│ Main      napi-rs addon   │ ◄──────► │ CAS + 元数据 DB             │
│ .vcs/     wc.db+pristine  │ HTTP/2   │ GC / Quota / Retention     │
└───────────────────────────┘   TLS    └───────────────────────────┘
              ▲                                    ▲
              └──────── vcs-core (Rust) ──────────┘
```

**关键决策：所有版本控制逻辑只实现一次，放在 Rust crate `vcs-core` 中。** 服务端直接调用，Electron 主进程经 napi-rs 原生模块调用同一份代码；TypeScript 只负责界面。这样工作副本引擎、增量算法、三方合并不会出现「JS 版与 Rust 版语义不一致」这类最难排查的问题。

Electron 渲染进程不直接发网络请求：所有仓库操作经 `contextBridge` 暴露的类型化 IPC 到主进程，由 Rust core 内的 gRPC 客户端发出。避免浏览器环境无法原生使用 gRPC 的问题，也便于统一 token 与重试。

## 3. 技术选型

| 层 | 选型 | 理由 |
| --- | --- | --- |
| 传输 | tonic gRPC（HTTP/2）+ rustls，双向流式；预留 HTTPS/1.1 + JSON/chunked 兼容通道 | 大对象传输；企业代理/防火墙兜底；传输层与业务逻辑解耦 |
| 服务端 | tokio + tonic + axum（管理/健康）+ sqlx + ldap3 + argon2 + jsonwebtoken + tracing | 异步 IO；内建认证，不依赖外部 IdP |
| 存储（服务端） | **统一的本地共享 CAS**（`SHARED_CAS`）：`repo/objects/`，blake3 + zstd + FastCDC 分块 | 全仓库单一对象池，不分区隔离；不引入 S3 / MinIO 等外部存储；跨版本、跨路径去重，增量只传缺失块 |
| 元数据 | SQLite（单机） | 强事务，唯一写入点；多机方案不在当前范围 |
| 客户端核心 | Rust `vcs-core` + **napi-rs 原生模块**，SQLite 管 `wc.db` | 已确认不采用 sidecar；与服务端同一份语义，离线操作快 |
| 桌面 | Electron + Vite + React + TS，虚拟列表 + Monaco diff | 只做 UI，不碰版本算法 |
| 认证 | 内建 `AuthService`：LDAP（LDAPS / StartTLS）+ 本地账号（自助注册 + 管理员审批）；JWT + PAT，token 存系统钥匙串 | 不依赖外部 IdP；审批与禁用可通过 `token_version` 即时吊销凭证 |

本机检测到 Rust 1.95、Node 22，**未安装 protoc**。M1 生成 gRPC 代码时使用 `protoc-bin-vendored` 提供编译器，避免要求开发者预装 protoc；CI 中同样使用该方式做 proto 语法校验。

## 4. 数据模型

### 4.1 服务端对象模型

- **Blob**：文件内容经 FastCDC 分块、zstd 压缩后，以 blake3 哈希为地址存入 CAS。
- **Tree**：不可变目录快照，`name → { kind, hash, size, props }`，Merkle 结构。每个 revision 的 `root_tree` 是**完整快照**，不是增量 delta 链。
- **Revision**：`{ rev rN, root_tree, parent_rev, author, message, committed_at }`，`revisions` 表是唯一写入点。

读 `path@rev` 即从 `root_tree` 逐级下钻，复杂度 O(目录深度)；`checkout` / `update` 退化为两次 tree 的 diff。「完整快照」这个选择在容量治理上带来一个关键好处：切断旧版本引用后做一次可达性标记，老数据自然成为孤儿，无需重放 delta 链即可裁剪。

磁盘布局（M1 已落地为服务端存储根目录，见 §11.2）：

```
<server-root>/
├── server.db                   # SQLite：repos / revisions / users / quota / audit …
├── server.secret                # JWT HS256 签名密钥，0600，首次启动生成
├── objects/                    # CAS：不可变内容块（全仓库共享一个对象池）
│   ├── ab/12e9c4…              # 前两级哈希做目录分片
│   └── 9f/03c1a7…
└── tmp/                        # 写入暂存区，仅用于原子 rename
```

对象文件首字节是压缩标记（`0` 原样 / `1` zstd 帧），因此「小对象不压缩、大对象压缩」不会引入第二个文件扩展名或额外元数据表。

### 4.1.1 树对象的规范编码

树对象必须**逐字节可复现**：同样的目录内容在任何平台、任何写入顺序下都要产生相同字节，否则 blake3 地址漂移，去重与「同一内容只存一份」的承诺就失效。因此不采用 serde/bincode 之类可能随版本改变布局的格式，而是手写定长小端编码：

```
"VCSTREE1"            8 字节魔数（布局变更必须升版本）
count                 u32 LE
重复 count 次，按 name 的字节序排序：
  kind                u8   （1=file, 2=dir, 3=symlink）
  size                u64 LE
  hash                32 字节
  name_len            u32 LE
  name                UTF-8 字节，禁止包含 '/'、'.'、'..' 或为空
```

排序是编码的一部分而不是调用方的责任——写入顺序不影响地址，这一点由单测 `encoding_is_order_independent` 钉住。解码遇到魔数不符、截断、尾部多余字节一律报 `TreeConflict`，不静默截断。

### 4.2 元数据 schema

见 `schema/server.sql`。核心表：

| 表 | 职责 |
| --- | --- |
| `repos` / `revisions` / `revision_changes` | 版本与变更元数据，**永久保留**；`repos` 另含 owner、状态与核定配额 |
| `repo_creation_requests` | 仓库创建申请与审批记录（未审批不分配 `repo_id`、不创建 CAS 目录） |
| `repo_members` | 仓库成员与角色；成员资格是使用仓库的第一道门 |
| `objects` / `trees` | CAS 对象索引、大小、`first_seen_rev`，支撑可达性计算 |
| `purged_paths` | 路径级裁剪标记，GC 可达性遍历时跳过 |
| `gc_jobs` | 异步 GC 任务状态与进度 |
| `quota_usage` / `retention_policies` | 容量占用缓存与保留策略 |
| `system_settings` | 全局默认配额等服务端级配置（KV，`default_quota_*`） |
| `upload_sessions` | 两阶段提交会话与断点续传 |
| `users` / `registration_requests` / `pat_tokens` | 用户、注册审批、个人访问令牌 |
| `ldap_config` / `ldap_group_roles` | LDAP 连接配置与组 → 角色映射 |
| `acl` / `locks` / `audit_log` | 权限、锁、审计 |

**元数据与内容字节分离**是本设计的基本原则：数据库里只存 32 字节哈希指针，真正的字节全在 CAS。

### 4.3 工作副本 `.vcs/`

见 `schema/wc.sql`。`wc.db`（SQLite）记录 `path, kind, base_rev, base_hash, state, mtime, size, conflict_id, lock_token, depth`；`pristine/` 按哈希缓存基线内容。

- 每个条目自带 `base_rev`，天然支持混合版本工作副本。
- `status` 先比 `mtime/size`，仅可疑文件重算哈希。**`mtime` 必须存纳秒**：秒级精度会让「同一秒内、字节数不变的改写」被快路径吞掉（改一行金额但长度不变，`status` 报干净，用户以为提交成功）。对时间戳精度不足的文件系统（FAT 等），用 `status --full` 强制全量哈希。
- `diff` / `revert` 完全离线，从 `pristine` 恢复；检出时同步写 `pristine`，所以断网也能看差异。
- 冲突不写标记文件，而是把 base/mine/theirs 三个哈希记入 `conflicts` 表，供 GUI 三栏解决。
- `wc_purged_paths` 缓存服务端裁剪标记，检出旧版本时给出友好提示而非悬空哈希错误。

## 5. 通信协议

完整契约见 `proto/repo.proto`（`package vcs.repo.v1`）。协议中不存在 `branch` / `tag` / `ref` 字段，`rev` 是唯一版本定位方式；`CheckoutRequest.rev` 表示按全局版本号检出历史快照，不是切换分支。服务分组：

| 组 | RPC |
| --- | --- |
| 元数据 | `GetHead`、`GetRevision`、`GetTree`、`GetLog` |
| 检出更新 | `Checkout`（流式）、`Update`、`FetchObjects`（双向流） |
| 提交 | `BeginCommit`、`PushObjects`、`FinalizeCommit`、`AbortCommit` |
| 锁 | `Lock`、`Unlock` |
| 容量治理 | `GetQuota`、`ListPurgeCandidates`、`EstimatePurge`、`Purge`、`GetGcStatus` |
| 用户体系 | 独立 `AuthService`：`Login`、`Register`、`List/Approve/RejectRegistration`、`RefreshToken`/`Logout`、`WhoAmI`、PAT 管理、LDAP 配置与连通性测试、用户启停与角色 |
| 仓库管理 | 独立 `RepoAdminService`：`RequestRepo`、`List/Approve/RejectRepoCreation`、`CreateRepoDirect`（管理员直通）、`ListRepos`/`GetRepo`、`SetRepoStatus`；成员管理 `ListMembers`/`AddMember`/`RemoveMember`/`SetMemberRole`/`SearchUsers`（owner 自助） |
| 配额配置 | `Get/UpdateQuotaDefaults`（全局默认，仅 admin）、`Get/SetRepoQuota`（单仓库覆盖，仅 admin；owner 只读查看生效值） |

**两阶段提交**：对象先落 CAS（此时是孤儿，GC 可回收），只有 `FinalizeCommit` 才在单事务里分配版本号。因此断网或中断不会产生半个版本，支持断点续传，重复哈希可秒级跳过。

```
BeginCommit(base_rev, changeset, estimated_new_bytes)
  ← session_id + 缺失对象哈希清单 + quota_hint
PushObjects(stream) → 分块 + zstd
FinalizeCommit(session_id)
  ← rN+1 + 新 root_tree   或   OUT_OF_DATE / QUOTA_EXCEEDED
回写 wc.db / pristine，更新各条目 base_rev
```

统一错误码：`OUT_OF_DATE`、`PATH_CONFLICT`、`TREE_CONFLICT`、`PERMISSION_DENIED`、`QUOTA_EXCEEDED`、`CONTENT_PURGED`、`LOCKED`、`OBJECT_MISSING`、`INVALID_SESSION`。`QUOTA_EXCEEDED` 的响应中直接携带可清理目录清单，供客户端渲染一键入口。

用户体系另有 `UNAUTHENTICATED`、`INVALID_CREDENTIALS`、`ACCOUNT_PENDING`、`ACCOUNT_DISABLED`、`REGISTRATION_EXISTS`、`SETUP_TOKEN_INVALID`、`LDAP_UNAVAILABLE`、`TOKEN_EXPIRED`、`PASSWORD_POLICY_VIOLATION` 等错误码；成员门另有 `REPO_NOT_MEMBER`、`REPO_MEMBER_FORBIDDEN`、`REPO_OWNER_PROTECTED`、`USER_NOT_FOUND` 等，配额配置另有 `QUOTA_CONFIG_INVALID`。`Register` 是唯一的免认证写接口（`Login` 也算免认证入口），其余 RPC 均校验 JWT / PAT 与成员资格；`SETUP_TOKEN_INVALID` 编号保留但已不再使用。

## 6. 核心流程

- **checkout**：`GetHead` → 取目标 rev 的 `root_tree`（按 depth 与稀疏路径）→ 按哈希批量拉缺失对象 → 写工作文件 + `wc.db` + `pristine`，全程可断点续传。
- **status / diff**：纯本地，`imara-diff` 做行级 diff。
- **commit**：`Finalize` 内事务用 `SELECT head FOR UPDATE`（SQLite `BEGIN IMMEDIATE`）保证单写者；`head != base_rev` 返回 `OUT_OF_DATE`，否则 copy-on-write 构建新 `root_tree`、写 revision、一次提交。冲突路径上不会出现「半合并」状态。
- **update**：服务端算 tree diff，客户端做 base/mine/theirs 三方合并；树冲突（add/add、delete/modify 等）返回结构化错误，MVP 不自动解决。
- **revert**：从 `pristine` 恢复，不联网。

## 7. 一致性、并发与冲突

- 版本号递增与 revision 写入在同一事务内，读走 MVCC 快照。
- 对象写入幂等：临时文件 + 原子 rename，同名哈希重复写结果一致。
- 提交中断只留孤儿对象，由 GC 清扫，不影响版本链。
- 树冲突显式检测并返回结构化错误，不自动猜测用户意图。
- **已确认采用严格 `out-of-date` 拒绝**：`FinalizeCommit` 时 `head != base_rev` 直接返回 `OUT_OF_DATE`，服务端不自动合并非重叠路径；用户先 `update` 再提交。策略由 `vcs_core::CommitPolicy::StrictOutOfDate` 显式锁定。

## 8. 用户体系与认证

认证由**内建 `AuthService`** 提供，不以外部 OIDC 为主入口（外部 IdP 可作为后续可选扩展）。两类身份来源共存：

| 来源 | 凭证 | 开通方式 |
| --- | --- | --- |
| LDAP / AD | 企业目录，服务端不存用户密码 | 首次登录成功自动创建本地映射（JIT），组映射为角色 |
| 本地账号 | Argon2id 哈希 + 每用户随机 salt | **用户自助注册（当场设密码）→ 管理员审批 → 直接可登录** |

**用户字段固定为三项**：`username`（用户名，全局唯一）、`password`（仅本地账号，只存 Argon2id 哈希）、`display_name`（姓名）。不收集邮箱、手机号、头像等任何其它资料；LDAP 侧也只映射用户名与姓名。密码强度规则见 8.4。

### 8.1 状态机

```
自助注册（自带密码）→ PENDING ──管理员 approve──→ ACTIVE ──禁用──→ DISABLED
              │
              ├─ reject ──→ REJECTED
              └─ 超期 ────→ EXPIRED
空库首个注册 ──自动批准为 admin────────→ ACTIVE（注册时的密码立即可用）
LDAP 首次登录成功 ─────────────────────→ ACTIVE（免审批；首个目录用户自动 admin）
```

- `PENDING` / `REJECTED` / `EXPIRED` / `DISABLED` 一律不可登录，返回 `ERROR_CODE_ACCOUNT_PENDING` 等明确错误码，客户端给友好提示而不是反复报「密码错误」。
- 审批通过时分配角色（默认只读，管理员可指定）。**密码在注册时就由本人设置并写库**：审批只是放行，不再有「先开通、后激活」两段式，也就没有一次性链接需要转交。
- **首位用户引导（本轮定稿）**：全库第一个账号自动成为 `admin`，无需审批、无需预置 bootstrap 管理员。注册与 LDAP 首次登录共用 `first_user_slot()`：在 `BEGIN IMMEDIATE` 写事务内判定「用户表为空且没有引导审计记录」，谁先提交谁拿走名额，跨 CLI / gRPC 进程互斥。引导记录落 `audit_log`（`user.first_admin.register` / `user.first_admin.ldap`），即便删掉首用户也不会让第二个人再抢注。
- 首位注册者同样在提交时设好密码：申请单直接 `approved`，注册完即可登录，不需要任何激活步骤。
- `init-admin` 降级为「可选的预置管理员 + 首位账号救援」：只在没有可登录管理员时可用；同名本地账号没有可用口令（旧流程遗留）时重置口令并提权，避免全库锁死。

### 8.2 自助注册与审批

- `Register` 为**免认证**接口，提交 `username`、`display_name`（姓名）、`password` 与申请理由；用户名唯一性、姓名长度、密码策略（8.4）都在服务端复核一次，不合规的申请直接拒绝且不落任何数据。不收集邮箱等其它资料。
- **空库首个 `Register` 自动批准为 `admin`**，响应里带 `auto_approved` / `granted_role`；第二个起仍回到 PENDING 审批链路。判定与 LDAP JIT 共用同一个事务名额（见 8.1）。
- 管理员用 `ListRegistrations` / `ApproveRegistration` / `RejectRegistration` 处理，拒绝必须填写原因；全部写入 `audit_log`。
- 审批通过后申请人直接用注册时的密码登录；未审批前密码不生效，登录返回 `ACCOUNT_PENDING` 而不是「密码错误」。旧流程遗留的无口令申请在审批时会被拒绝，提示对方重新申请。
- 防刷：按 IP / 用户名限流，可选验证码；申请默认 7 天有效，超期自动 `EXPIRED`。

### 8.3 LDAP 登录（已接入）

流程：服务账号 bind → 按 `user_filter` 搜索用户 DN → 查组 → 用该 DN 与用户密码再次 bind 校验 → JIT 建号 / 更新映射 → 签发 JWT。

- **分层**：目录协议在 `vcs-server/src/ldap.rs`（只管连接与 bind，不碰数据库），业务在 `ServerStore::login_ldap()`（只接收已验证的 `DirectoryUser`）。gRPC 把同步 bind 放进 `spawn_blocking`，因为 `ldap3` 的同步接口会自建 tokio runtime，不能在 async 线程里直接调用。这样集成测试可以注入假 `DirectoryUser`，不必真的起目录服务。
- 依赖 `ldap3 0.11`（`sync` + `tls-rustls`，纯 Rust TLS，跨平台一致）；默认用系统根证书校验，**从不调用 `set_no_tls_verify`**；连接与单次操作超时各 5 秒。连接池暂未做，每次登录一条连接。
- 用户名在拼入 filter 前按 RFC 4515 转义（`* ( ) \ NUL` 与非 ASCII 字节），防 LDAP 注入；`group_filter` 另外支持 `{dn}` 占位符。
- 必须 LDAPS 或 StartTLS，禁止明文 bind；服务账号只读、最小权限。
- `ldap_config.bind_password_enc` 用 `vcs_core::auth::seal_secret` 加密（HMAC-SHA256 keystream + encrypt-then-MAC，密钥取自 `server.secret` 并做域分隔），读取接口 `LdapPublicConfig` 恒不回传明文；`UpdateLdapConfig` 的 `bind_password` 留空表示沿用已存密码；`TestLdapConnection` 供管理员验证配置。
- **降级策略**：LDAP 不可用时，本地管理员与本地账号仍能登录，避免全员锁死；LDAP 登录返回 `ERROR_CODE_LDAP_UNAVAILABLE`，绝不退化成「校验通过」。用户不存在与口令错误统一返回 `INVALID_CREDENTIALS`，不泄露账号是否存在。
- **首次登录免审批（已定稿）**：LDAP 用户不走自助注册审批，bind 成功即 JIT 创建 `ACTIVE` 本地映射并写审计。**首个目录用户自动成为 `admin`**（与注册共用引导名额）；之后的组 → 角色映射由 `ldap_group_roles` 配置，命中多条取最高、未命中 `readonly`，且**只在 JIT 建号时生效**——已有账号角色以本地为准，避免目录侧组调整把引导管理员悄悄降级。
- 目录侧只读取用户名（`attr_username`）与姓名（`attr_display_name`），不再读取邮箱属性；LDAP 账号不受 8.4 的本地密码策略约束，密码由目录服务自行管理。
- 仍待补：目录连接池、嵌套组解析、真机目录集成测试。当前自动化覆盖 filter 转义、配置校验、故障降级，以及注入 `DirectoryUser` 的 JIT / 首位规则 / gRPC 端到端首用户注册。

### 8.4 Token、凭证与密码策略

- 登录成功签发 HS256 JWT 放 gRPC metadata。默认有效期 12 小时；桌面端登录页勾选「30 天内免登录」时，`LoginRequest.remember_me=true`，服务端签发 30 天有效期的 token（`TokenSigner::sign_with_ttl` 按次指定，不改全局 signer）。CLI / Web 管理端 / 公共机器仍走 12 小时。`refresh_token` 字段保留但轮换 RPC 尚未实现，到期重新登录。
- `users.token_version`：管理员禁用账号或改角色时递增，JWT 携带该版本号，校验时比对，实现**即时失效**，不必等 token 自然过期。
- PAT 供 CLI/CI：创建时明文只返回一次，库里只存哈希；支持 scopes、过期时间与随时撤销。
- 存储：Argon2id + 每用户随机 salt；登录失败计数与临时锁定。注册时算好的哈希直接写 `users.password_hash`，服务端不保存任何明文口令。
- 客户端 token 存系统钥匙串（Electron `safeStorage`），不落明文文件；无加密能力时宁可不存，界面把「30 天免登录」置灰。
- **30 天免登录的边界**：`settings.json` 只存密文 token 与 `sessionExpiresAt`，启动时先做本地到期判断，再用 `WhoAmI` 向服务端确认 token 仍有效，成功才直接进主界面。没有服务端会话表，撤销依旧只能靠 `token_version`（停用 / 改角色 / 重置口令）；不勾记住我时只保留内存会话，不落盘。

**本地账号密码强度规则（硬约束）**：

| 条件 | 要求 |
| --- | --- |
| 长度 | ≥ 8 个字符（按 Unicode 标量值计） |
| 数字 | 至少 1 个 ASCII 数字 `0-9` |
| 大写字母 | 至少 1 个 `A-Z` |
| 小写字母 | 至少 1 个 `a-z` |
| 特殊字符 | 至少 1 个 ASCII 标点字符（对应 Rust `char::is_ascii_punctuation()`）；空格、字母、数字与非 ASCII 字符均不计入 |

- 校验实现放在 `vcs-core` 的 `validate_password()`（返回全部未满足项）与 `password_is_valid()`，服务端、CLI 与 Electron 主进程（经 napi-rs）复用同一份逻辑，不出现「前端宽松、后端严格」或反之。
- **服务端在 `Register` 及任何后续改密接口必须强制再校验一次**；客户端校验只用于实时提示，不作为安全边界。不满足返回 `ERROR_CODE_PASSWORD_POLICY_VIOLATION`，`Status.message` 列出具体缺失项。
- 该规则只约束本地账号；LDAP 用户的密码由目录服务管理，不在此校验。

### 8.5 权限分层

权限分两层，**成员资格是能否使用仓库的第一道门**：

| 层 | 载体 | 决定什么 |
| --- | --- | --- |
| 成员门（仓库级） | `repo_members` | 该用户是否为本仓库成员、属于哪个成员角色；**非成员一律不能检出、提交、更新或清理** |
| 路径门（可选细粒度） | `acl` | 成员在具体路径前缀上能做哪些操作；未配置时以成员角色为准 |

- `acl.subject` 统一为 `user:<id>` / `group:<ldap_group_dn>` / `role:<name>`；LDAP 组由 `ldap_group_roles` 映射，用于批量路径授权，但**不能替代成员资格**。
- 全局 `admin` 通过 `RepoAdminService` 执行审批、仓库状态变更与审计；若要"使用"某仓库（检出 / 提交），同样需要被 owner 加为成员，不自动获得全仓库访问权。
- 用户注册与仓库创建审批接口要求全局 `admin`；成员管理接口由 owner 自助调用（见 8.9）。
- 成员资格与路径 ACL 在 `BeginCommit` 与 `FinalizeCommit` **两次**校验，防止会话期间被移出成员列表仍完成提交。

### 8.6 其他安全要求

- 路径统一规范化防穿越；符号链接与大小写不敏感文件系统（macOS / Windows）单独检测。
- 对象大小与请求频率限流；登录、注册、审批、禁用、PAT 操作全量写审计日志；服务端 hook 一律不开放。
- 提交前可选密钥/隐私扫描，把误提交挡在入库之前。

### 8.7 客户端交互

客户端启动先显示**登录页**：用「本地账号 / LDAP 账号」两个 tab 切换账号类型（切到哪个按哪个登录，LDAP tab 不显示注册入口），只填用户名、密码并勾「30 天内免登录」；成功后才切到主界面（工作副本 / 仓库 / 账号），已记住的会话在启动时静默恢复。注册入口在登录页：表单只有用户名、姓名、密码、申请理由四项，密码按 8.4 由服务端复核。
- **服务器地址不出现在登录表单里**：默认地址由 `desktop/build-config.json`（或 `VCS_DEFAULT_TARGET`）在 `npm run build` 时生成 `dist/renderer/config.js` 注入安装包；页面只显示当前生效地址，点「设置」才能改成 `host:port` 覆盖默认值，覆盖值存 `userData/settings.json`，「恢复默认」清除。换地址时本机旧凭证一并作废。
- 登录后的业务请求若返回认证类错误，界面自动退回登录页。管理员审批台在服务端 Web 管理端，桌面端不再提供。仓库列表只显示自己作为成员的仓库。

### 8.8 仓库创建与审批

仓库不再由运维随手创建：**任何 `ACTIVE` 用户都可提交创建申请，必须经管理员审批后才能使用**。未审批的申请只是一张单据，不分配 `repo_id`、不创建 CAS 目录、不能执行任何 `RepoService` 操作。

```
用户 RequestRepo → PENDING ──管理员 approve──→ 创建 repos(active) + CAS 目录 + owner 成员记录 → 可用
                      ├─ reject ──→ REJECTED（附原因）
                      └─ 超期 ──→ EXPIRED
管理员可 SetRepoStatus 将已激活仓库置为 DISABLED / ARCHIVED
```

- **申请单** `repo_creation_requests`：申请人、仓库名、期望 `repo_id`、用途说明、申请配额、状态、审批人与决定、有效期。仓库名与 `repo_id` 全局唯一，命名限定小写字母 / 数字 / 连字符。
- **审批动作**：`ApproveRepoCreation` 时管理员可核定 `quota_bytes`（可驳回申请值），也可**留空继承全局默认**；通过后在**单事务**内插入 `repos`（状态 `active`、记录 `owner_user_id` 与来源申请）、写入 `repo_members` 的 owner 行、更新申请状态与审计日志。
- **初始化顺序**：先幂等创建 `repo/objects/` 目录（残留的孤儿目录由 GC 清理），再提交数据库事务；目录创建失败则事务不提交，申请保持 `PENDING` 可重试。绝不允许出现「有 `repos` 行但无目录」或「有目录但无 `repos` 行」的可使用状态。
- **owner 权限**：申请人自动成为该仓库 owner（`repo_members` 中 `role=owner`），可自行增删成员、调整成员角色与管理配额内空间；**其他用户默认无权限，必须被 owner 加入成员才能使用仓库**（见 8.9）。
- **管理员直通**：全局 `admin` 可用 `CreateRepoDirect` 直接创建已激活仓库，供首次部署、迁移与自动化测试使用，同样写审计日志；该入口按推荐默认保留（见第 15 章收口说明）。
- **RPC 校验**：`RepoService` 所有方法依次校验 `repos.status == active` → 调用者在 `repo_members` 中 → 成员角色允许该操作 → 路径级 `acl`；分别返回 `REPO_DISABLED` / `REPO_ARCHIVED`、`REPO_NOT_MEMBER`、`PERMISSION_DENIED`。`PENDING` 申请没有 `repo_id`，携带该 ID 的请求返回 `REPO_NOT_FOUND`，不向非申请人泄漏审批状态。
- **容量联动**：审批时若核定配额则写入 `repos`，留空则写 0 表示继承全局默认；最终由 9.6 的生效规则解析后供 `BeginCommit` 预检使用。

`init` 语义随之调整：申请审批通过生成空仓库（r0），owner 再用 `import` 从本地目录导入内容产生首个 revision，不再由 `init` 一步创建可用仓库。

### 8.9 仓库成员管理

**创建者（owner）自行管理成员，无需全局管理员介入；只有成员才能使用仓库。**

`repo_members` 以 (`repo_id`, `user_id`) 为唯一键，记录成员角色、`added_by` 与 `added_at`。owner 也作为一行存在，`repos.owner_user_id` 只是审批来源的冗余索引，两者在同一事务内写入。

| 角色 | 能力 |
| --- | --- |
| `owner` | 全部操作；增删成员、改成员角色、管理配额内空间；唯一、不可移除、不可降级（MVP 不做转让） |
| `maintainer` | 可增删 `writer` / `reader` 成员，不能改 owner 或增删 maintainer；可读写与清理 |
| `writer` | 检出、提交、更新、路径级清理 |
| `reader` | 仅检出与查看历史 |

- owner 用 `ListMembers` / `AddMember` / `RemoveMember` / `SetMemberRole` 管理；`SearchUsers` 按用户名查找可添加的 `ACTIVE` 用户，只返回用户名与姓名，不返回任何敏感信息。`maintainer` 只能增删低阶成员，越权返回 `REPO_MEMBER_FORBIDDEN`。
- 成员变更即时生效：移除成员时同步把该用户未完成的 `upload_sessions` 置为 `aborted`，`FinalizeCommit` 仍会二次校验成员资格兜底；所有成员变更写入 `audit_log`。
- `ListRepos` 只返回调用者作为成员且状态 `active` 的仓库；非成员调用 `RepoService` 返回 `REPO_NOT_MEMBER` 并提示联系 owner。
- 全局 `admin` 的管理通道（审批、状态变更、审计）不受成员门限制，但使用仓库仍需被 owner 加为成员。

## 9. 容量治理与空间回收

### 9.1 问题定义

典型场景：用户按年月创建 `202610`、`202611`、`202612`，创建 `202701` 时磁盘不足，希望自己删掉旧月份就能立刻腾出空间继续工作。

在朴素设计下这做不到：删除只是新增一个不含该路径的 revision，旧 tree 仍引用 blob，GC 认为它「可达」，磁盘一字节都不会少，下一次提交照样失败。因此需要的不是「删除功能」，而是一套**空间回收闭环**。

### 9.2 默认策略修正

原定的「历史永久保留」修正为：

> **revision 元数据永久保留，内容字节按路径 / 保留策略可裁剪。**

`log`、作者、时间、变更路径列表不丢；释放掉的正是占空间大头的内容字节。

### 9.3 独占大小与释放量预估

CAS 去重下，「目录有多大」与「删掉它能释放多少」是两回事。定义：

- **逻辑大小 logical_bytes**：该目录下所有文件字节之和（含被共享的对象）。
- **独占大小 unique_bytes**：仅被该目录可达、删除后可真正回收的对象字节之和。

空间面板按目录/月份展示 `unique_bytes`。任何清理操作先 `EstimatePurge` 做 dry-run，返回 `reclaimable_bytes` 与 `affected_revisions`，客户端展示「清理 202610 预计释放 12.4 GB，影响 3 个历史版本」，确认后才执行。用户无需理解 GC。

### 9.4 路径级裁剪（`purged_paths`）

**不能用「推进 `min_retained_rev` 砍掉整段历史」做主力**：那会连带抹掉 `202611`、`202612` 的历史，用户只删了一个月份却丢了全部历史。

正确做法：

1. 删除提交移除 HEAD 中的路径。
2. 在 `purged_paths` 写入该路径模式，状态 `ACTIVE`（或先 `PENDING` 进入宽限期）。
3. GC 做可达性遍历时，从所有 revision 的 `root_tree` 出发，但**跳过 `purged_paths` 匹配的条目**。
4. 只有被该路径独占的 blob 变为孤儿并回收；被其他月份共享的对象自动保留。

不重写历史、不改版本号、不影响其他目录；`unique_bytes` 与最终 `reclaimed_bytes` 天然对齐。旧 tree 中残留的悬空哈希不删除（tree 对象很小），检出旧版本遇到它返回 `CONTENT_PURGED`，由客户端给出「该内容已按容量策略清理」的提示。

按时间裁剪整段历史（`min_retained_rev`）仅作为可选保留策略，不是本场景主力。

### 9.5 异步 GC

标记-清扫可能很慢，一律异步执行：

- `Purge` 返回 `gc_job_id`，后台任务从 `gc_jobs` 表取状态。
- 标记阶段：遍历所有保留 revision 的 tree（跳过 purged 路径），生成可达哈希集合。
- 清扫阶段：删除 `objects` 中不在集合内的记录与对应文件，累计 `reclaimed_bytes`。
- 客户端通过 `GetGcStatus` 轮询/订阅进度与已释放量。
- 清扫采用「先标记待删、延迟物理删除」两段式，避免与正在进行的 `PushObjects` 竞态。

### 9.6 配额层级、配置与提交前预检

管理员可配置**全局默认配额**（对所有仓库生效）与**单仓库配额**（覆盖全局默认）。配额只有**一条线**：超过即拒绝提交，不分软 / 硬。

| 优先级 | 来源 | 说明 |
| --- | --- | --- |
| 1 | 单仓库显式值 `repos.quota_bytes` | 非 0 时优先；审批时核定，或用 `SetRepoQuota` 事后调整 |
| 2 | 全局默认值 `system_settings` 的 `default_quota_bytes` | 仓库值为 0 时继承；管理员用 `Get/UpdateQuotaDefaults` 配置 |
| 3 | 系统内置兜底 `FALLBACK_QUOTA_BYTES`（100 GiB） | 两者都未配置时启用，保证任何情况下都有上限，避免空间无限膨胀 |

- **继承语义**：`repos.quota_bytes = 0` 表示「继承全局默认」；`SetRepoQuota(inherit=true)` 可清零恢复继承。改全局默认后，所有继承默认值的仓库**立即**按新阈值预检；改单仓库值只影响该仓库。
- **合法性**：只有「0（继承）」与「> 0（显式上限）」两种取值，不再存在软硬比例约束，审批人只需填一个数字。
- **只读可见**：owner 与成员可查看本仓库的生效配额及其来源（`QuotaSource`），但只有全局 `admin` 能修改；审批时也可选择「不核定」，留空即继承全局默认。
- **审计**：全局默认与单仓库配额的每次变更写入 `audit_log`，记录改前 / 改后值、执行者与仓库。

**提交前预检**：把容量检查前移到 `BeginCommit`：

- 请求携带 `estimated_new_bytes`，服务端用**生效后的** `limit_bytes` 计算 `used + new > limit_bytes` 时返回 `QUOTA_EXCEEDED`。
- 错误响应附带 `ListPurgeCandidates` 结果（最老月份 + 各自 `reclaimable_bytes`）以及当前生效配额与来源，避免用户不知道该找谁调额度。
- 客户端直接渲染「清理 202610 可释放 12.4 GB → [一键清理] → [重试提交]」。
- 用量条按上限百分比变色（85% 起黄、95% 起红），只提示不拦截。

好处是避免「写到一半磁盘满、留下一堆孤儿对象」的脏状态。

### 9.7 保留策略自动化

`retention_policies` 支持按目录模式（如 `^20\d{4}$`）配置：保留最近 N 个月 / 最多 N GB / 超过 X 天。到期自动执行「逻辑删除 → 路径裁剪 → 异步 GC」，支持 `dry_run` 与通知，避免用户忘记手动清理。

### 9.8 回收站与立即释放

删除默认进入 `PENDING` 宽限期（7 天）以防误删，宽限期内不物理回收。但磁盘已满的场景下必须提供 `immediate=true` 的「立即释放」选项，否则用户删完仍写不进去。UI 在配额不足错误中默认勾选立即释放。

### 9.9 存储布局：统一共享 CAS

已确认**全仓库只使用单一共享 CAS**（`SHARED_CAS`），不提供按月份分区隔离（`PARTITIONED`）布局：`objects/` 是唯一对象池，所有目录、所有月份、所有历史版本共用同一份内容并按哈希全局去重。删除 `202610` 这类月份目录后，空间释放完全依赖 9.4 的路径级裁剪与 9.5 的异步 GC。

收益是按月归档中大量重复的模板、附件只存一份。代价有两点，方案里明确补齐：

| 代价 | 缓解措施 |
| --- | --- |
| 引用会跨目录「外溢」，删一个目录能释放多少不再等于它的逻辑大小 | `EstimatePurge` 在 GC 标记阶段构建反向引用集合，精确计算 `unique_bytes` 并缓存进 `quota_usage`；`shared_paths` 列出被别处共享、不会被回收的部分，使 dry-run 预估与实际释放量对齐 |
| GC 需扫全库可达性，不能靠删分区 O(1) 完成 | 异步任务 + `GetGcStatus` 进度可查；按 `first_seen_rev` 分批做增量标记；`objects.pending_delete` 两段式清扫避免与并发上传竞态；低峰执行，不阻塞提交 |

`v_cas_usage` 提供全库单行容量统计供 `GetQuota` 使用，`quota_usage` 按路径缓存独占大小供空间面板展示。schema 中不再保留 `storage_layout`、`objects.bucket`、`v_bucket_usage` 等布局选择或分桶字段。

容量治理主线始终是 9.4～9.8 节的裁剪与 GC；当前范围既不引入外部对象存储，也不引入分区布局。

### 9.10 用户交互闭环

```
创建 202701 → BeginCommit → QUOTA_EXCEEDED
   ↓ 响应携带候选：202610 可释放 12.4 GB
空间面板 / 一键清理 → EstimatePurge(dry-run) → 确认（默认立即释放）
   ↓
删除提交 + 路径裁剪 + 异步 GC → GetGcStatus 显示进度
   ↓
重试提交 202701 → 成功
```

全程不需要用户理解 revision、CAS、GC，也不需要登录服务端敲命令。

### 9.11 权限与审计

清理同样要过 8.5 的两道门：调用者必须是仓库成员（`writer` / `maintainer` / `owner`），再由路径级 ACL 判断是否对自己的目录持有 `purge` 权限；非成员返回 `REPO_NOT_MEMBER`。成员即可自助完成，无需全局管理员介入。所有 `Purge`、GC、配额变更与成员变更写入 `audit_log`，记录执行者、路径、释放量、原因。

### 9.12 与备份、加密擦除的关系

主库 GC 后，离线备份与灾备快照中仍可能残留已清理数据。若只是空间问题可接受；若涉及敏感数据，按第 10 章采用加密擦除，让所有副本中的密文同时失效。

## 10. 删除语义与数据擦除

| 层次 | 行为 |
| --- | --- |
| 逻辑删除（默认） | 新增 revision，旧版本仍可检出；不释放空间 |
| 容量清理（第 9 章） | 路径级裁剪 + GC，释放内容字节，元数据保留 |
| 历史重写 `purge --path` | 离线重放生成新仓库，跳过目标路径，版本号与工作副本全部失效 |
| 加密擦除 | 每个敏感对象独立 DEK 加密，删除密钥即不可读；不动版本号，备份中密文自动失效 |
| 删除整个仓库 | 连同 `metadata.db`、`objects/`、全部备份一起销毁 |

建议：MVP 只实现逻辑删除 + 普通 GC，文档明确「删除是可回滚的历史事件，不是数据擦除」；但第一天就预留 `ObjectCodec` 可插拔加密层、对象引用可重建、`purge` 管理员权限与审计，避免事后补救成本极高。

## 11. 工程结构

```
svn-next-ai/
├── docs/design.md               本方案
├── proto/repo.proto             gRPC 契约（唯一协议真源）
├── schema/
│   ├── server.sql               服务端元数据 DDL（22 个表 / 视图）
│   └── wc.sql                   工作副本 wc.db DDL（7 个表）
├── crates/
│   ├── vcs-core/                共享核心
│   │   ├── hash.rs              blake3 地址、hex、objects/ab/cdef… 分片路径
│   │   ├── tree.rs              树对象规范编码、自底向上组装、扁平化、全树遍历
│   │   ├── store.rs             FileObjectStore：tmp→原子 rename、zstd、幂等写入
│   │   ├── auth.rs              Argon2id、HS256 JWT、PAT/CSPRNG
│   │   ├── wc.rs                工作副本：检出、离线 status / diff、pristine 缓存
│   │   └── lib.rs               领域类型、ObjectStore 契约、错误码、配额解析
│   ├── vcs-proto/               build.rs 用 tonic-build + protoc-bin-vendored 生成 gRPC
│   ├── vcs-server/              lib（ServerStore 业务层），无独立 bin
│   ├── vcs-grpc/                网络层 lib：三个 service 的实现 + serve()，无独立 bin
│   ├── vcs-napi/                Node 原生模块：把 vcs-core 的工作副本语义交给 Electron
│   └── vcs-cli/                 唯一二进制 `vcs`：客户端命令 + `serve` + `server info`
├── desktop/                     Electron 客户端
│   ├── src/main/client.ts       gRPC 传输层（@grpc/grpc-js + proto-loader）
│   ├── src/main/wc.ts           工作副本 → napi 的类型化包装（不实现任何语义）
│   ├── src/main/flow.ts         闭环编排（检出 / 更新 / 两阶段提交 / 还原）
│   ├── src/main/index.ts        主进程 IPC：唯一发起网络与文件操作的地方
│   ├── src/main/settings.ts     连接信息（明文）与凭证（safeStorage 加密）持久化
│   ├── src/preload/index.ts     类型化 API（统一 Envelope 返回）
│   ├── src/renderer/            界面（无 Node 权限，只能经 preload 调用）
│   ├── native/vcs-napi.node     napi 构建产物（gitignore，由 npm run build:native 生成）
│   ├── electron-builder.yml     打包配置（未签名，内部试用）
│   └── scripts/
│       ├── build-native.cjs     cargo build + 改后缀，不引 @napi-rs/cli
│       ├── stage-assets.cjs     把 proto 复制进 desktop/ 以便随包分发
│       └── smoke-single-user.cjs  起真服务端的全链路冒烟
└── .github/workflows/
    ├── ci.yml                   三平台 CI + 契约校验 + CLI/桌面端冒烟
    └── release.yml              三平台未签名打包（打 tag 时发 Release）
```

M0 阶段 Rust workspace 保持零外部依赖、可离线 `cargo check`；M1 引入外部依赖并结束该状态，这是刻意的阶段边界：**契约评审与编译工具链解耦**，proto 作为纯文本可先评审通过，再生成代码。

### 11.1 vcs-proto：契约即代码

`proto/repo.proto` 是唯一真源，`vcs-proto` 只做代码生成，不含任何业务逻辑。生成物同时提供服务端 trait 与客户端桩（`repo_service_server` / `repo_service_client` 等 6 个模块，133 个消息、14 个枚举）。

配套 `crates/vcs-proto/tests/contract.rs` 把已对外承诺的内容钉死：

- 34 个错误码的数值（客户端按数值分支，编号只能新增不能复用），M1 追加 `ERROR_CODE_IO_ERROR = 35`；
- 三个 service 的客户端桩必须存在（服务被误删会编译失败）；
- 契约里不得出现 `branch` / `tag` 字段（守住「无分支与标签」）。

### 11.2 服务端存储根目录

```
<server-root>/server.db       SQLite，由 schema/server.sql 初始化（WAL + 外键开启）
<server-root>/server.secret   JWT 签名密钥，0600
<server-root>/objects/        共享 CAS
<server-root>/tmp/            原子写入暂存区，启动时清理 .part 残留
```

M1 只有本地直连形态（CLI 直接打开这个目录），目的是先把版本语义跑对；M3 起多了 gRPC 网络入口。两者（以及运维工具）打开的是同一个存储根目录。

原先是三个独立二进制 `vcs` / `vcs-grpc` / `vcs-server`，部署时要分发三份、还容易把 `--root` 与 `--store` 指到不同目录。现合并为**唯一可执行文件 `vcs`**，用子命令区分：

```bash
vcs --store /srv/vcs serve --listen 127.0.0.1:50051   # 常驻 gRPC 服务，默认只绑本机
vcs --store /srv/vcs server info                      # 一次性运维：看布局、对象数、字节数
vcs --store /srv/vcs import reports ./src -m "初始导入"  # 本地直连的客户端命令
```

`--store` 是全局参数（环境变量 `VCS_STORE`），所有子命令共用，从根上消除了目录错配。业务层仍在 `vcs-server` lib、网络层仍在 `vcs-grpc` lib，只是不再各自带 bin；`vcs serve` 在 `vcs-cli` 里建 tokio runtime 后调 `vcs_grpc::serve`。对外暴露公网请在前面加 TLS 网关（gRPC over TLS 或反向代理），本阶段不做内置 TLS。

### 11.3 CLI 命令（M1 + M2 + M3）

| 命令 | 对应 gRPC | 说明 |
| --- | --- | --- |
| `vcs serve` | 承载三个 service | 常驻服务；`--listen` / `--maintenance-interval` 只对该子命令生效 |
| `vcs server info` | 无（本地运维） | 一次性打印存储布局、对象数、压缩后大小、是否已初始化 |
| `vcs init-admin` | `BootstrapAdmin` | 可选：没有可登录管理员时预置 / 救援管理员；已有管理员则拒绝 |
| `vcs login` / `logout` | `Login` | token 缓存于 `~/.vcs/credentials.json`，0600；`--ldap` 走目录 bind |
| `vcs repo-create` | `CreateRepoDirect` | 管理员直通建库，写审计 |
| `vcs user-create` | `ApproveRegistration` 建号段 | 管理员建 ACTIVE 账号 |
| `vcs member-add` / `member-list` | `AddRepoMember` / `ListRepoMembers` | 按角色矩阵校验 |
| `vcs import` | `BeginCommit` + `PushObjects` + `FinalizeCommit` | 全量目录导入，自动识别增删改 |
| `vcs commit` | `BeginCommit` + `PushObjects` + `FinalizeCommit` | **M2**：提交工作副本；冲突未解决直接拒绝；失败自动 `AbortCommit` |
| `vcs update` | `GetChanges` | **M2**：把服务端新版本合并进工作副本，产出冲突元数据 |
| `vcs revert` | 无（纯本地） | **M2**：从基线恢复，清冲突；未受控文件不删 |
| `vcs checkout` | `Checkout` | 可指定 `-r` 检出历史版本 |
| `vcs log` | `GetLog` | 含每版本变更清单 |
| `vcs status` / `diff` | 无（纯本地） | 完全离线，不访问服务端 |
| `vcs cat` | `GetFile` | 输出指定版本文件内容；内容已裁剪时返回 `CONTENT_PURGED` |
| `vcs info` | `GetQuota` | 文件数、逻辑大小、对象池占用、生效配额与来源 |
| `vcs estimate` | `EstimatePurge` | **M2**：dry-run，给出可释放 / 共享 / 影响版本数 |
| `vcs purge` | `Purge` | **M2**：删除提交 + 路径裁剪标记 + GC（`--immediate` 立即释放） |
| `vcs gc` / `gc-status` | `GetGcStatus` | **M2**：手动全库回收 / 查任务进度 |
| `vcs purged` / `unpurge` | `ListPurgedPaths` / `RevertPurge` | **M4**：回收站列表与撤销清理（内容已回收则明确失败） |
| `vcs retention-list` / `retention-set` / `retention-rm` | `List/Upsert/DeleteRetentionPolicy` | **M4**：按目录正则配置保留策略（默认仅预览） |
| `vcs retention-apply` | `ApplyRetention` | **M4**：评估并清理；`--dry-run` 只报告 |
| `vcs maintenance` | 无（系统任务，无 RPC） | **M4**：手工触发「转正到期清理 + 全库 GC」，与 `vcs serve` 定时任务同一入口 |
| `vcs candidates` | `ListPurgeCandidates` | **M2**：按独占大小排序的可清理目录 |
| `vcs register` | `Register` | **M3**：免登录自助注册（用户名/姓名/密码/理由，密码留空则交互输入）；**空库首个账号自动批准为 admin，注册完即可登录** |
| `vcs ldap-config` / `ldap-show` / `ldap-test` | 本机运维（无 RPC） | 加密保存目录配置 / 回显（不含密码）/ 测试服务账号 bind |
| `get` / `update` / `test` LDAP 配置 RPC | `GetLdapConfig` / `UpdateLdapConfig` / `TestLdapConnection` | 仅管理员；桌面端「目录设置」走这三个接口 |
| `vcs register-list` | `ListRegistrations` | **M3**：管理员查看申请（可按状态过滤） |
| `vcs register-approve` | `ApproveRegistration` | **M3**：通过审批并激活账号（密码由申请人注册时设置） |
| `vcs register-reject` | `RejectRegistration` | **M3**：驳回，必须写原因 |
| `vcs repo-request` / `repo-request-list` | `RequestRepo` / `ListRepoCreations` | **M3**：任何 ACTIVE 用户可申请建库；普通用户只看自己的申请 |
| `vcs repo-approve` / `repo-reject` | `ApproveRepoCreation` / `RejectRepoCreation` | **M3**：管理员审批，单事务建库 + owner + r0 |
| `vcs pat-create` / `pat-list` / `pat-revoke` | `CreatePat` / `ListPats` / `RevokePat` | **M3**：明文只返回一次，撤销即时生效 |
| `vcs login --pat` | `Login`（凭证校验段） | **M3**：CI 用令牌登录，不必把密码写进脚本 |
| `vcs repo-list` | `ListRepos` | **M3**：只列出自己参与的仓库 |
| `vcs member-remove` / `member-set-role` | `RemoveMember` / `SetMemberRole` | **M3**：按角色矩阵校验，owner 受保护；变更立刻作废在途提交 |
| `vcs repo-quota` | `SetRepoQuota` | **M3**：管理员核定单仓库配额，`--inherit` 恢复继承 |
| `vcs repo-status` | `SetRepoStatus` | **M3**：active / disabled / archived；非 active 时读写全拒 |


## 12. 里程碑与验收

| 阶段 | 产出 | 验收 | 状态 |
| --- | --- | --- | --- |
| M0 | proto 契约、server/wc schema、workspace 与 Electron 骨架、CI | 契约评审通过；`cargo check` 通过；proto 语法校验通过 | ✅ 完成 |
| M1 | 只读闭环：管理员直通创建仓库 + `import`（本地目录）、`checkout`、`log`、离线 `status/diff`；**bootstrap 管理员 + 本地登录 + JWT 校验** | CLI 可用管理员账号完成检出与状态比对 | ✅ 完成（另加：成员门、两级配额预检、gRPC 代码生成、13 个服务端集成测试） |
| M2 | 写闭环：`commit` / `update` / `revert` / 冲突元数据；**GC 基础**（可达性标记、独占大小、dry-run） | 真实仓库跑通提交更新闭环；`EstimatePurge` 预估值与实际释放量误差可解释 | ✅ 完成（COW 树变更、两阶段提交、冲突三哈希、路径裁剪 + 标记清扫 GC；93 个测试） |
| M3 | 工程化：**LDAP 登录 + 自助注册审批 + 仓库创建审批 + 成员门与 owner 成员管理 + 全局/单仓库配额配置 + PAT**、组角色映射、ACL 与锁、配额预检、一键 `Purge`、大文件分块续传、GC、备份 | 注册与仓库申请两条审批链路均跑通；非成员无法检出/提交；改全局默认后继承仓库立即按新阈值预检、单仓库覆盖优先 | 进行中：**自助注册（含首位用户自动管理员，注册即设密码）+ 审批 + LDAP 登录 / JIT 建号 / 组角色映射 / 三个配置 RPC + 仓库申请审批 + PAT（含 scope 收窄）+ gRPC 服务层 + 成员/配额/仓库状态治理已落地**；ACL、锁、目录连接池待做 |
| M4 | Electron GUI：登录/注册、新建仓库向导与审批台（用户 + 仓库）、仓库设置页成员管理、admin 配额配置页、空间面板、检出向导、变更列表、diff、提交、历史、三栏冲突、打包自动更新 | 三平台安装包可用 | 进行中：**提交 / 更新 / 冲突三栏合并 + 注册审批 + 建库审批 + 成员管理 + 空间治理 / 回收站 / 保留策略（含自动执行）/ 配额设置 + 后台维护 + 打包（本地 + GitHub Actions 三平台未签名）已落地**（工作副本走 napi 复用 `vcs-core`）；代码签名与自动更新按「内部试用」定位暂不做 |
| M5 | 可选 AI 辅助：提交信息生成、变更摘要、冲突解释，服务端可插拔接口 | 不侵入核心协议 | 待开始 |

CLI 先行、GUI 后置，目的是先用最低成本验证版本语义是否正确。

## 13. 测试策略

- Rust 单元测试；`proptest` 覆盖分块与三方合并。
- 内存服务端集成测试，覆盖 `OUT_OF_DATE`、并发提交、中断续传。
- `cargo-fuzz` 打协议解析与路径规范化。
- 审批状态机单测：状态转换、仅 `ACTIVE` 可登录、审批前后密码是否可用。
- 密码策略单测：长度边界（7 / 8 位）、四类字符各自缺失、特殊字符集合（ASCII 标点计入，空格与非 ASCII 不计入）、客户端与服务端共用同一份校验函数。
- 仓库审批集成测试：`PENDING` 申请无 `repo_id` 且任何 `RepoService` 调用返回 `REPO_NOT_FOUND`；审批事务原子性（`repos` + owner 成员记录 + 申请状态 + 审计同时成功或同时回滚）；重复审批幂等；`DISABLED` / `ARCHIVED` 仓库拒绝读写。
- 成员门集成测试：非成员调用 `RepoService` 一律 `REPO_NOT_MEMBER`；`reader` 提交被拒；owner 不可移除或降级；`maintainer` 不能增删 `maintainer`；成员被移除后未完成的 `BeginCommit` 在 `FinalizeCommit` 被拒；`ListRepos` 不列出非成员仓库。
- 首位用户引导专项：空库首个注册自动批准为 admin、第二个仍 PENDING；首个 LDAP 登录为 admin、之后按组映射；两条入口共用名额（注册先 / LDAP 先各一条）；引导审计记录防止删号重抢；`init-admin` 救援未激活的首位账号；gRPC 端到端验证 `RegisterResponse` 新字段与「未配置目录时返回 `LDAP_UNAVAILABLE`」。
- LDAP 单测覆盖 filter 转义防注入、TLS URL 生成、配置校验、bind 密码加密落库（明文不落盘 / 不回传）、故障降级；真机目录集成测试（mock bind / 组查询）与连接池仍待补。
- JWT 即时失效：禁用用户或改角色后旧 token 被拒；PAT 明文只返回一次、只存哈希、撤销立即生效。
- GC 专项：构造共享对象、裁剪路径、验证「共享对象不被误删」与「独占对象全部回收」。
- 配额层级单测与集成：继承 / 覆盖 / 兜底三种来源、软阈值 85% 推导、非法配置校验；改全局默认后继承仓库的 `BeginCommit` 立即按新硬阈值拒绝，单仓库覆盖不受全局默认变化影响。
- 三平台 CI（macOS / Windows / Linux）；Electron 端 Playwright E2E。

## 14. 风险与对策

| 风险 | 对策 |
| --- | --- |
| napi-rs 三平台预编译产物 | 已确定采用原生模块；CI 三平台 matrix + prebuild 缓存 + 按架构分发 `.node` 产物，不引入 sidecar 备选 |
| 企业网络拦截 HTTP/2 / gRPC | 保留 HTTPS/1.1 + JSON/chunked 兼容传输层，与业务逻辑解耦 |
| Windows 文件锁 / 长路径 / 大小写 | 临时文件原子 rename、`\\?\` 前缀、统一路径规范化 |
| 大二进制文件 | FastCDC 分块去重，避免整文件重传 |
| GC 与并发写入竞态 | 标记待删与物理删除两段式，清扫前检查会话引用 |
| 备份中残留已清理数据 | 敏感场景启用加密擦除；归档场景在文档中明确告知 |
| SQLite 单机写并发 | `BEGIN IMMEDIATE` + 单写者；WAL 模式提升读并发 |
| LDAP 服务不可用 | 本地管理员与本地账号可独立登录；LDAP 请求返回 `LDAP_UNAVAILABLE`，绝不降级为「校验通过」 |
| LDAP 注入 / 明文 bind | 用户名拼入 filter 前按 RFC 4515 转义；强制 LDAPS/StartTLS 与证书校验；服务账号只读最小权限；连接与操作各 5 秒超时（连接池待补） |
| 首位用户名额被抢注 | `BEGIN IMMEDIATE` 事务内原子判定 + 引导审计记录，跨 CLI / gRPC 进程互斥；名额只发一次，删号也不会重发；部署方可先用 `init-admin` 预置管理员关闭这个入口 |
| 自助注册被刷 / 审批积压 | 注册接口按 IP 与用户名限流、可选验证码；申请 7 天过期；管理员审批台与待办提醒；首位账号自动批准后即由本人承担审批 |
| 仓库审批积压导致无法开通 | 申请 7 天过期 + 管理员待办提醒；全局 `admin` 可用 `CreateRepoDirect` 应急直通并写审计；审批台按提交时间排序 |
| 审批与目录创建不一致 | 先幂等创建 `objects/` 再提交事务，失败保持 `PENDING` 可重试；孤儿目录由 GC 清理；启动时扫描并修复不一致状态 |
| 弱密码 / 客户端校验被绕过 | 密码策略在 `vcs-core` 单一实现；服务端 `Register` 强制再校验并返回 `PASSWORD_POLICY_VIOLATION`；客户端校验仅作实时提示 |
| 成员被移除后仍完成提交 | 成员资格在 `BeginCommit` 与 `FinalizeCommit` 两次校验；移除时将其未完成的 `upload_sessions` 标记为 `aborted` |
| owner 误操作导致仓库无人可管 | owner 行不可移除、不可降级、不可被 maintainer 改动；MVP 不做所有权转让；变更全部写审计日志 |
| 配额继承被误解 / 两级配置冲突 | 生效优先级由 `vcs-core::resolve_effective_quota()` 单一真源决定；`GetRepoQuota` 同时返回显式值、全局默认与生效来源；`hard > 0` 且 `soft <= hard` 强校验；每次变更写审计日志 |
| 配额拒绝却已写盘 | 提交分两遍：第一遍只算地址，预检通过才落盘。单测 `rejected_import_writes_nothing_to_the_object_pool` 守住 |
| 整目录全量提交的扫描成本 | M1 一次提交读两遍盘（算地址 + 落盘），大目录下 I/O 翻倍；M2 引入 `BeginCommit` 会话与客户端侧变更列表后只推送真正变化的路径 |
| 共享 CAS 下 `unique_bytes` 算不准 | 配额预检用乐观估计（可能误拒，不会误放）；M2 的 GC 标记阶段用反向引用集合精算，并在 `EstimatePurge` 里给出可解释的误差 |

## 15. 决策记录

### 已定

| 决策 | 结论 |
| --- | --- |
| 版本控制逻辑实现位置 | 单一 Rust `vcs-core`，客户端经 napi-rs 复用 |
| 对象模型 | 不可变完整 tree 快照 + 全局递增 rN，非 Git DAG |
| 提交协议 | 两阶段 `BeginCommit` → `PushObjects` → `FinalizeCommit` |
| 删除默认语义 | 逻辑删除；历史元数据永久，内容按策略可裁剪 |
| 容量回收主力机制 | 路径级 `purged_paths` 裁剪 + 异步 GC，非整段历史裁剪 |
| **存储布局** | **统一共享 CAS（`SHARED_CAS`）**，不提供 `PARTITIONED` 分区隔离 |
| **SVN 历史导入** | **不做**。不兼容 SVN 协议，不提供 `svn dump` 导入 |
| **服务端内容存储** | **本地文件系统 CAS**（`repo/objects/`）+ 单机 SQLite；不引入 S3 / MinIO / Postgres |
| **提交冲突策略** | **严格 `out-of-date` 拒绝**，`head != base_rev` 返回 `OUT_OF_DATE`，不做服务端自动合并 |
| **客户端绑定方式** | **napi-rs 原生模块**，不采用 sidecar 进程 |
| **分支与标签** | **不提供**。无 `branch` / `tag` / `ref`，也不用目录约定模拟；单一线性主干，版本定位只用 `rN` |
| **认证方式** | **内建 `AuthService`**：LDAP（LDAPS / StartTLS）+ 本地账号，不以外部 OIDC 为主入口 |
| **自助注册** | **除全库首位用户外需管理员审批**：`PENDING → ACTIVE`（或 `REJECTED` / `EXPIRED`），注册时自设的密码在审批通过后立即可用 |
| **首位管理员** | **第一个注册或第一个 LDAP 登录成功的用户自动成为 `admin`**，免审批，两条入口共用同一事务名额且只发一次；`init-admin` 降级为可选预置 / 救援通道 |
| **仓库创建** | **用户申请 + 管理员审批**：`PENDING → APPROVED` 后才分配 `repo_id`、创建 CAS 目录并置为 `ACTIVE`；未审批不可检出或提交 |
| **用户字段** | **仅用户名、密码、姓名三项**；不收集邮箱、手机号等其它资料 |
| **密码策略** | **≥ 8 位且同时含数字 / 大写 / 小写 / 特殊字符**；`vcs-core` 单一实现，服务端 `Register` 强制校验，LDAP 账号豁免 |
| **初始密码交付** | 用户在注册表单里自己设置密码，服务端不生成、不转交任何初始口令；不做邮件通知 |
| **仓库成员** | **创建者（owner）自行管理成员，只有成员能使用仓库**；`repo_members` 是准入第一道门，ACL 是可选的路径级细粒度层 |
| **全局管理员边界** | `RepoAdminService` 的审批/状态/审计不受成员门限制，但检出、提交等使用操作仍需被加为成员 |
| **配额层级** | **管理员配置全局默认 + 单仓库覆盖**；优先级 单仓库显式值 > 全局默认 > 内置兜底；owner 与成员只读查看生效值及来源 |

### 已按推荐默认定稿（2026-11-10，M0 收口）

原两项待确认按此前给出的推荐默认落地，如需调整只需改一处分支 / 一个编译开关，不影响 M0 契约：

1. **LDAP 用户首次登录免审批**：企业目录已是可信身份源，登录成功即自动生成 `ACTIVE` 映射并写审计日志。若要求全员审批，改为落到 `PENDING`。
2. **保留 `CreateRepoDirect` 管理员直通入口**：供首次部署、迁移、自动化测试与审批积压应急，所有直通创建写审计日志。若要求严格全员审批，可在编译期关闭该 RPC。

**M0 契约层至此无悬项。**

### M1 实施中新增的决策（2026-11-10）

这些是写代码时才暴露、原方案没有覆盖到的点，逐条记录以免后来者重新踩一遍：

| 决策 | 结论与理由 |
| --- | --- |
| 提交分两遍扫描 | **先只读算地址、配额预检通过后才写 CAS**。原实现「边扫边写」，结果被配额拒绝的提交在盘上留下 50 KB 孤儿对象——拒绝本身成了把磁盘填满的途径。第二遍写入时复用第一遍的哈希，已存在对象跳过。 |
| `status` 快路径用纳秒 mtime | 秒级精度会把「同秒内、等长改写」判成未修改（实测：`收入 100` → `收入 120` 后 `status` 报干净）。文件系统时间戳精度不足时用 `status --full` 兜底。 |
| `revisions.author` 存用户名 | 原设计只写 `user_id`，导致 `log` 输出一串随机 ID。用户名在系统内唯一且不可改，直接可读，审计仍用 `user_id`。 |
| 新增错误码 `IO_ERROR = 35` | 客户端需要区分「业务拒绝」（可引导自助处理）与「服务端故障」（应重试）。业务错误码 1–34 保持不变。 |
| 空提交不分配版本号 | `import` 内容与上一版完全一致时返回 `no_changes` 并保持 `head_rev` 不变，避免历史被空提交灌满。 |
| 对象的物理大小以目录为准 | `vcs info` 的「对象池占用」直接遍历 `objects/` 求和，而不是 `SUM(objects.stored_size)`：索引表可能因历史原因缺行，磁盘字节数永远是事实。实测 500 字节 / 9 个对象与 `find | stat` 完全一致。 |
| 检出时同步写 `pristine` | 让 `diff` 真正离线可用，而不是依赖本地能否访问服务端。代价是工作副本多一份基线缓存，由 `pristine.ref_count` 管理回收。 |
| 成员管理 API 提前到 M1 | 成员门写在每个 `RepoService` 入口上，但没有第二个用户的测试等于没测。`create_user` / `add_member` / `list_members` 三条 API + 集成测试提前落地，owner / maintainer / writer / reader 的角色矩阵由 `vcs-core::RepoMemberRole::can_assign` 单一真源判定。 |

### M2 实施中新增的决策（2026-11-10）

写 M2 时暴露、且原方案没有明确写死的点：

| 决策 | 结论与理由 |
| --- | --- |
| 树变更用 copy-on-write | `apply_changes` 只重写变更路径经过的目录，其余子树复用哈希。有测试直接断言「改 202611 后 202610 的文件哈希不变」——否则「不可变完整快照」撑不住高频提交，写入量会随仓库规模线性增长。 |
| 冲突只存三哈希，不写标记文件 | base / mine / theirs 进 `conflicts` 表，本地文件保留用户当前内容。好处是工作目录里不出现 `.mine` / `.theirs` 之类垃圾，GUI 三栏与 CLI 读同一份元数据。 |
| 冲突未解决一律拒绝提交 | 宁可挡住也不猜用户想留哪一边。`add/add` 与 `delete/modify` 归为**树冲突**，不自动合并；`modify/modify` 归为**内容冲突**。 |
| `update` 从「最旧条目版本」取差异 | 混合版本下，被 `revert` 回旧基线的条目仍停在旧 `base_rev`。只看 `head_rev` 会得出「已是最新」，那部分差异永远拉不回来（实测踩到：revert 后 `update` 报「已是最新 r3」，但文件内容还是 r2 的）。 |
| 从旧版本重算差异要跳过「已应用」条目 | 同上场景会把已铺好的 `Add` 条目重新当成新增，制造假的 `add/add` 冲突。规则：条目 `base_rev >= 目标版本` 直接跳过；内容哈希已等于目标内容则视为已应用并推进 `base_rev`。 |
| 整目录提交：未受控文件按 `Add` 处理 | 与 `import` 语义一致，用户不必先 `add` / `rm`。代价是随手放进目录的临时文件也会进版本库，所以 CLI 提交前把新增清单打印出来。 |
| 两阶段提交做两次 out-of-date 校验 | `begin` 拦一次给出明确反馈，`finalize` 再拦一次堵住「会话期间别人抢先提交」。被拒会话转为 `aborted`，避免客户端拿同一个会话重试成功。 |
| GC 只回收 blob，不回收 tree 对象 | 设计 9.4 写的是「旧 tree 中残留的悬空哈希不删除」。一开始连 tree 一起回收，结果旧版本的整棵树展不平，`cat -r 1` 报的是「对象不存在」而不是 `CONTENT_PURGED`，连 `log` 的内容清单都取不到。留树、删内容才符合「元数据永久保留」。 |
| 可达性用反向引用表，不用遍历剪枝 | 一次遍历全部 `DISTINCT root_tree` 建出「对象 → 引用路径表」，`EstimatePurge` 与 GC 共用同一张表。剪枝遍历与预估算两套代码必然对不齐，实测预估与实际释放量一致（40 B / 40 B）靠的就是同源。 |
| GC 竞态防护不靠时间窗 | `objects` 表的行只由 `finalize_commit` 在写版本的同一事务里插入，所以「表里有行」等价于「某个已提交版本引用它」。早先按 `created_at >= mark_started` 兜底是错的：秒级精度会把同一秒内提交的正常内容永久保护起来，GC 一字节都收不回。 |
| `purge` 三件套不可拆 | 删除提交（移出 HEAD）+ `purged_paths` 标记（GC 跳过该路径）+ GC 任务。只删路径不写标记，旧版本仍引用内容，磁盘一字节不少；只写标记不删 HEAD，用户看到的目录还在。 |
| 内容被裁掉返回 `CONTENT_PURGED` | 这是容量策略的**预期状态**，不是数据损坏。客户端据此提示「该内容已按容量策略清理」，而不是让用户以为仓库坏了。 |

### M3 实施中新增的决策（2026-11-10，用户体系闭环阶段）

| 决策 | 结论与理由 |
| --- | --- |
| 新增错误码 `INVALID_ARGUMENT = 36` | 「驳回必须写原因」「scope 不在白名单」「过期时间早于现在」都是入参问题。复用 `INVALID_SESSION` 会让客户端误判成登录态失效并跳去重新登录，把用户带进死循环。 |
| 注册与账号在同一事务里创建 | `users(status='pending')` + `registration_requests(pending)` 必须一起出现，否则会出现「有申请无账号」或「有账号无申请」，审批时无从下手。 |
| 用户名冲突分三种提示 | 已被本地账号占用 / 属于 LDAP 账号 / 已有待审批申请。笼统的「已存在」会让用户反复重试或以为系统坏了。 |
| 审批通过时也递增 `token_version` | 与禁用账号同一机制：即便此前有人拿到过该账号的任何凭证，审批动作本身就把它们全部作废。 |
| `PasswordPolicyError` 实现 `Display` | 服务端要把「缺哪几类字符」一次性告诉用户，否则用户会一次改一条、来回试四遍。这条文案也是 Electron 端实时提示的同一份来源。 |
| 仓库申请审批复用一个 `insert_new_repo` | 直通建库与审批建库必须产出完全一致的仓库（repos 行 + owner 成员行 + r0 空版本），两处各写一遍必然漂移。 |
| SHARED_CAS 下「先建目录再提交事务」退化为空操作 | 设计 8.8 的初始化顺序假设了每仓库独立目录。统一共享 CAS 后不存在该目录，不变量简化为「事务提交成功即代表仓库可用」，不会出现「有 repos 行却不可用」的中间态。 |
| 申请审批后 r0 的作者是申请人而非审批人 | 仓库的主人是申请人；`log` 的第一条如果写着管理员，事后完全无法解释归属。 |
| PAT 的 scopes 独立于成员角色 | 角色决定「在仓库里能做什么」，scope 决定「这个令牌能做什么」。CI 只读令牌即便持有人是 owner 也不能提交、不能清理空间——这正是最小权限的落点。 |
| PAT 默认 scope 为 `read`，且不能签发超出本人权限的 scope | 不指定就按最小权限；普通用户创建 `admin` scope 直接拒绝，否则提权只需一步。 |
| `authenticate` 一个入口同时吃 JWT 与 PAT | 客户端手上只有一个「token」，不该让调用方先判断类型；靠 `vcs_pat_` 前缀分流，不试错解析。 |
| 申请列表按身份收窄 | 管理员看全部，普通用户只看自己的。审批状态本身也是信息，不该在用户之间泄露。 |
| 惰性过期而不是定时任务 | 没有后台调度器，所有读写申请入口先跑一次「把到期的置为 EXPIRED」。代价是一次多余的 UPDATE，收益是不会出现「列表 PENDING、审批说已过期」的割裂。 |

### M3 第二阶段：gRPC 服务层的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 网络层单独成 `vcs-grpc` crate，不塞进 `vcs-server` | 两者都保持为纯 lib：`vcs-server` 是业务库，`vcs-grpc` 是网络层。若塞在一起，业务库会拖上 tonic/tokio 全套依赖，而且 bin 与 lib 会互相反向依赖（同一 package 里 bin 无法依赖自己新加的 service 库）。后来合并单二进制时由 `vcs-cli` 同时链接二者：`vcs serve` 提供常驻入口，存储运维改为 `vcs server info`，依赖方向依然单向。 |
| 业务错误放响应体，只有认证失败与未实现走 gRPC 状态码 | proto 每个响应都带 `Status`，客户端按 `ErrorCode` 分支即可。把 `QUOTA_EXCEEDED` 塞进 `tonic::Status` 会让客户端只能解析错误串，且丢了随状态返回的 `candidates`。 |
| **错误码必须显式 match，不能 `code as i32`** | Rust 侧 `ErrorCode` 没有显式判别值、`Ok` 是 0，而 proto 的 0 是 `UNSPECIFIED`、`OK` 是 1。直接 `as i32` 会让 36 个错误码整体错一位——客户端把「配额超限」当「入参非法」。改成穷尽 match 后新增变体会编译失败，强制回来对齐。 |
| `Arc<Mutex<ServerStore>>` 串行化，不用连接池 | `rusqlite::Connection` 是 `Send` 不是 `Sync`，而 tonic handler 要 `&self` 跨线程。SQLite 本来就单写入者，单机形态用一把锁最简单；换连接池 + `spawn_blocking` 是 M4 之后的并发优化。 |
| `MutexGuard` 绝不跨 `await` | 流式接口（`FetchObjects`）先把请求里的哈希收齐再一次性加锁读内容，避免 `!Send` 的 guard 被带过 await 点。 |
| `match self.api.store().xxx()` 是自死锁陷阱 | 探针表达式里的临时 guard 会活到整个 match 结束，arm 里再锁一次就是不可重入锁自锁。写完 `add_member` 后网络测试直接挂住，改为「先收进变量再 match」。 |
| `GetTree` 目前返回扁平全量 | proto 的 `Depth` 语义（立即子项 / 稀疏）需要客户端配合，M4 检出向导才真正用得上；现阶段把完整相对路径放进 `name` 字段，配合 `Checkout` 的对象流已够用，遇到非 `INFINITY` 直接返回 `INVALID_ARGUMENT` 而不是假装支持。 |
| `Change` 的条目类型借 `props` 承载 | proto 的 `Change` 没有 entry_kind 字段（M0 定稿时只有普通文件）。符号链接必须原样带过去，否则提交后退化成普通文件；约定 `props == b"symlink"`，加字段属于破坏契约的改动，留到确有需要时再做。 |
| 未实现的方法一律 `UNIMPLEMENTED` | 当时 `Lock`/`Unlock`、`RefreshToken`、`Logout`、LDAP 三项、`SetUserStatus`/`SetUserRole`、`SetRepoStatus`、`RemoveMember`/`SetMemberRole`、`SetRepoQuota` 都明确报错；后续逐步补齐——**LDAP 三项已实现**，成员/配额/仓库状态方法也已落地（见 §21）。宁可客户端收到「未实现」，也不能悄悄返回成功——那会让 GUI 以为操作生效了。 |
| `GetHead` / `GetRepo` 也走成员门 | 非成员连 head 与仓库名都不该知道。设计 8.9 的「ListRepos 不列出非成员仓库」是同一口径。 |

### M3 第三阶段：成员 / 配额 / 仓库状态的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 成员变更立刻作废在途提交会话 | 设计 8.9 只要求「移除成员时同步把未完成会话置 aborted」。实现上把「降级到只读」与「禁用整个仓库」也纳进来：靠 `FinalizeCommit` 的二次校验虽然能拦住结果，但用户是在上传完才失败，白等一场。 |
| 不能移除自己 | owner 本来就受保护；maintainer 移除自己会把自己锁在外面（`repo_members` 已无该行，再想加回来需要 owner）。直接拒绝，让人去找 owner。 |
| `owner` 不可降级、也不可授予 | `RepoMemberRole::can_assign` 里 `Owner => !target.is_protected()` 已经表达「不能造 owner」，`set_member_role` 复用同一判定，不另写一套规则。 |
| `SetRepoQuota(inherit=true)` 与「显式给 0」是两件事 | 前者清零恢复继承全局（`QuotaSource` 会回到 `GLOBAL_DEFAULT` / `FALLBACK`），后者在 `inherit=false` 时表示「不设覆盖」，同样写 0 但语义靠调用方意图区分。审计里同时记录 from / to / inherit 三个值。 |
| 禁用仓库要连整个仓库的在途会话一起中止 | 只作废调用者自己的会话没有意义——仓库已经不可写，谁的会话都注定失败。`repo.status.set` 的审计里记下 `aborted_sessions` 数量。 |
| `GetHead` 从 `find_repo` 换成 `require_repo` | 之前只查成员资格，禁用的仓库仍能返回 head；网络测试里「禁用后 GetHead 应报 `REPO_DISABLED`」直接抓到了。现在状态与成员门都由 `require_repo` 统一把关。 |
| `GetRepo` / `GetRepoQuota` 对禁用仓库仍然可读 | 与 `GetHead` 相反：仓库设置页需要看到「已禁用」并提供启用入口，管理动作不能被自己的状态挡住。口径差异写在这里，避免后来者以为是漏改。 |

### M4 第一步：客户端技术选型的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| **传输层用 `@grpc/grpc-js`，本地语义留给 napi-rs** | 不是二选一。proto 是稳定契约（错误码编号被契约测试钉死），用 JS 客户端成本低，且流式接口（Checkout / PushObjects）在 grpc-js 里是一等公民；而版本语义（blake3、树编码、wc.db、冲突合并）是一致性风险最高的部分，必须复用 `vcs-core`，那只能走 napi。**若只能选一个，选 napi**：语义重复的代价远高于传输层重写。 |
| 不在 napi 里包 gRPC 客户端 | 那会把 tonic + tokio 拖进 Electron 主进程：预编译产物变大、崩溃半径变大、加载变慢，而它只负责转发 RPC。传输层的复杂度用 JS 承担更划算。 |
| M4 第一步的本地工作副本是 **JS 最小实现** | 单人提交闭环需要先在真实网络与文件系统上跑通；把 `vcs-core` 编进主进程是独立的一步工程（三平台预编译、加载路径、签名）。所以先用 `manifest.json` + `baseline/` 做到「检出 / 状态 / 离线 diff / 提交后刷新基线」，接口按 `WorkingCopy` 的语义设计，**下一步换成 napi 实现时调用方不用改**。代价明确写在文件头：没有冲突合并、没有稀疏检出、没有纳秒 mtime 快路径。 |
| 主进程是唯一发起网络与文件操作的地方 | 渲染进程 `sandbox: true` + `contextIsolation: true` + CSP `script-src 'self'`，即使页面被注入脚本也拿不到 token 与磁盘。所有 IPC 返回统一 `Envelope<T>`，业务错误带 proto 的 `ErrorCode`，界面按码决定「提示」还是「让我重新登录」。 |
| 客户端也做一次 out-of-date 检查 | 服务端会拦，但客户端先拦能给出可操作的提示（「请先更新再提交」）。这是 UX，不是安全边界——服务端严格校验一次都没省。 |
| 提交前重读内容并校验哈希 | 扫描与推送之间用户可能又改了文件。`localChanges` 给出哈希后，提交前再读一次并比对，不一致直接拒绝重来，避免提交出「内容与地址不符」的对象。 |
| 端到端冒烟而不是只 typecheck | `npm run typecheck` 只能证明类型对；gRPC 流式调用、proto-loader 的字段映射、路径处理只能真跑。冒烟脚本起真实 `vcs serve`、真实建库检出提交，并校验服务端对象内容与本地一致。 |

### M4 第二步：napi 工作副本与更新闭环的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 工作副本换成 napi 复用 `vcs-core::wc`，删掉 JS 实现 | 上一轮的 JS 最小实现是脚手架，不是终点：`wc` 有 22 个单测盯着纳秒 mtime 快路径、冲突三哈希、COW 更新，在 JS 里复制必然走样。现在 `desktop/src/main/wc.ts` 只剩类型化包装，一行语义都没有。 |
| 不引 `@napi-rs/cli`，直接 `cargo build` + 改后缀 | 我们只需要「产出一个 Node 能 `require` 的动态库」。cdylib 改名 `.node` 即可加载，少一条构建工具链，CI 也少一步。`scripts/build-native.cjs` 负责按平台选 `dylib`/`so`/`dll`。 |
| 内容用 `MemoryObjectStore` 喂进 `vcs-core` | 桌面端没有本地 CAS，而检出 / 更新 / 提交收尾都需要 `&dyn ObjectStore`。把刚拿到的 Buffer 塞进内存存储，直接复用同一套 pristine 写入逻辑，不必为桌面端造第二个存储实现。 |
| napi 只做类型转换，不做业务判断 | 所有判定（哈希校验、冲突分类、删除幂等）都在 Rust。这样「桌面端和 CLI 行为不一致」这类 bug 没有产生的土壤。 |
| `wcInfo` 同时返回 `oldestRev` | 混合版本：head 已推进，但刚 revert 过的条目仍停在旧基线。`update` 必须从最旧版本取差异——这个坑在 CLI 阶段踩过一次（M2），桌面端复用了同一个解。 |
| 冲突在客户端也拦一次 | 与 CLI 的 `vcs commit` 一致：带冲突一律拒绝，列出冲突路径，让用户还原或手工合并。服务端也会拦，这里只是更早、更具体。 |
| 冒烟用 CLI 扮演「另一个人」 | 更新与冲突需要服务端前进。与其在测试里伪造 RPC，不如真用 `vcs import` 从另一个目录提交——这也顺带验证了「CLI 与桌面端写的是同一个仓库、同一套语义」。 |

### M4 第三步：注册与审批台的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 免认证接口用「临时连接」而不是全局 client | 注册发生在登录之前。给它们单独开一个 `VcsClient`（用完即关），避免把「未登录」状态混进已登录会话，也不会污染当前 token。 |
| 拒绝必须填原因（界面与后端双约束） | 后端已经强制（`INVALID_ARGUMENT`）；界面上把原因输入框放在每次操作旁边，而不是弹一个全局对话框——审批是逐条进行的，全局输入框很容易填错行。 |
| 角色只影响入口显隐，权限判定仍在服务端 | 管理员看到「直通新建」与审批台，普通用户看到「提交建库申请」。这只决定"看不看得见"，每个 RPC 都会独立校验角色与成员资格。 |
| 审批建库默认不核定配额（继承全局） | 审批时强行让管理员填一个数字，结果多半是随手填。留空即继承全局默认，需要限额时再到仓库设置里调——这也是设计 9.6 的优先级顺序。 |
| 冒烟覆盖「非管理员自开通」 | 注册/审批/建库是给"别人"用的能力，只有管理员自己跑通不算数。冒烟里真的注册 bob、审批、设密码、用 bob 登录、申请建库、审批、用 bob 读 head。 |

### M4 第四步：成员管理页的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| owner 那一行不给操作按钮 | 服务端会拒（`REPO_OWNER_PROTECTED`），但让用户点了才被告知是浪费。界面上直接显示 `owner` 标签、无下拉无移除，把规则摆在明处。 |
| 角色下拉只列 maintainer / writer / reader | 把别人提成 owner 服务端也不允许（owner 唯一），列出来只会诱导误操作。 |
| 搜索结果里「已是成员」的置灰 | 添加成员最常见的误操作是重复添加（服务端返回 `REPO_MEMBER_EXISTS`）。在候选列表里直接标出来，比报错更省一次往返。 |
| 改角色 / 移除后立刻刷新成员列表 | 这两个动作会顺带作废对方的在途提交会话，界面上要能立刻看到结果，而不是等下次切仓库。 |
| 搜索只返回 ACTIVE 用户的三项字段 | 服务端已如此（`SearchUsers`），界面上也不显示角色与状态：加人只需要认人。 |
| 冒烟用「owner 加管理员进来再移除」验证权限收回 | 管理员被移除后也会立刻失去访问权，这是「管理通道 ≠ 成员资格」的最直接证明。 |

### M4 第五步：空间治理面板的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 候选目录同时显示「可释放」与「当前」两个数 | 两者口径不同：`unique_bytes` 是「只被该目录独占的对象字节（跨全部历史版本）」，可能**大于** `logical_bytes`（HEAD 里这些文件的字节）。只显示一个数字必然让人困惑，面板里直接写清口径。 |
| 清理必须先「预估所选」再「清理所选」，并在确认框里写清后果 | 清理会生成删除提交并物理删除内容，不可撤销。dry-run 是设计 9.3 的核心体验（"预计释放 12.4 GB，影响 3 个版本"），界面不能跳过。 |
| 「立即释放」默认勾选 | 会走到这个面板的人，基本就是空间不够；默认 7 天宽限期等于没解决问题。未勾选时提示「需要立刻腾空间请勾选」。 |
| 清理后立刻查 GC 状态并展示实际释放量 | 预估与实际的对比是这套机制的可信度来源。M2 的实现里两者同源（同一张反向引用表），界面上把结果直接摆出来，用户才能相信"预估不是拍脑袋"。 |
| 侧栏限高 + 滚动 | 仓库 / 成员 / 空间三块叠在一列，窗口不高时会溢出。`max-height: calc(100vh - 110px)` + 内部滚动，比让整页滚动更可控。 |

### M4 第六步：打包与首次运行体验的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 连接信息明文、凭证加密，两者分开存 | `settings.json` 里只放服务端地址与上次的用户名；token 用 `safeStorage`（钥匙串 / DPAPI / libsecret）加密后再落盘。**加密不可用时宁可不存**，也不写明文——重启多登一次是小事，凭证泄露是大事。 |
| 渲染进程永远拿不到 token | 只有主进程持有凭证；渲染进程通过 `loadPrefs` 得到「地址 + 用户名 + 有没有记住」，通过 `tryResume` 得到「恢复了 / 没恢复」。少一个泄露面。 |
| 恢复失败静默处理 | token 过期、被撤销、换机器（钥匙串重置）都会走到这里，这不是用户做错了什么。清掉凭证、把登录框留在眼前即可，不弹错误框。 |
| 主动「断开连接」会清除凭证 | 用户点断开的意思是「别再记住我」。服务端地址与用户名保留，方便下次登录。 |
| `.node` 必须从磁盘加载，asar 里不行 | electron-builder 配了 `asarUnpack: native/**/*.node`，`wc.ts` 里把 `app.asar` 改写为 `app.asar.unpacked`。这一条在开发态无法覆盖，只能靠打包后实测。 |
| proto 在打包前复制进 `desktop/proto/` | monorepo 的真源在仓库根，而 electron-builder 只打包应用目录。与其在运行时猜相对路径，不如构建时复制一份（`scripts/stage-assets.cjs`），客户端按两个候选路径探测。 |
| 不做代码签名与公证 | 现阶段产物用于内部试用。正式发布需要开发者证书、macOS notarize 与 Windows 代码签名，凭证不该进仓库；配置里显式 `identity: null`，避免有人误以为已签名。 |
| asar 默认开启，但提供 `pack:unpacked` | asar 能减少文件数与启动索引开销；但受限环境可能拒绝写 asar 流（本次验证的沙箱就是），因此保留 `--config.asar=false` 的等价脚本。两种布局下路径改写都已覆盖。 |

### M4 第七步：冲突三栏与基线收敛的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 冲突发生时把服务端内容也缓存进 pristine | 三栏合并必须能**离线**读到 theirs。原先只有 base 进了 pristine，theirs 只在服务端；每次合并都要联网重取一遍，且断网就没法处理冲突。缓存成本是一份内容（CAS 去重后通常为 0）。 |
| 提供 mine / theirs / custom / delete 四种选择，不做自动合并 | 三方自动合并要在没有语法理解的前提下猜语义，猜错比重来一次代价大得多。用户明确点一个按钮，语义就明确了。 |
| 默认编辑框填「我的内容」 | 大多数冲突的正确结果是「以我为主、补上对方的改动」。预填 mine 让最常见的路径只需改几行，而不是从空白开始。 |
| 二进制文件禁止文本合并 | 出现 NUL 就按二进制处理：三栏显示「（二进制内容，无法文本合并）」，编辑框与「保存合并结果」禁用，只留「保留我的 / 采用服务端 / 删除」。给二进制一个编辑框只会产生损坏的文件。 |
| 解决后不刷新 mtime/size | 只清冲突、把状态改回 `normal`。下一次扫描会发现内容与基线不同 → 正确显示为待提交的 `MODIFY`。若顺手把 mtime 一起更新，`status` 的快路径会认为「没变过」，用户会以为合并丢了。 |
| 新增 `advance_normal_bases`（全量更新后推进基线版本） | 早先 `oldest_rev` 会被「在仓库里从未被改过」的条目一直钉在旧版本上，导致每次 update 都重算同一段差异、重复下载对象。全量 update 后把干净条目推进到 head，`oldest_rev` 才收敛。冲突条目跳过——它们的内容是本地版本，推进等于谎报「已是最新」。 |
| 该推进放在 core 而不是自动发生 | `apply_update` 是通用接口，理论上可以只传部分路径（单文件更新）。「变更集覆盖整个仓库」是调用方的知识，所以由 `flow.updateWorkingCopy` 显式调用，并在 core 的方法注释里写清前提。 |

### M4 第八步：回收站与撤销清理的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 撤销 = **新提交**，不是回滚历史 | revision 只增不改（设计 4.1）。撤销会再写一个把内容放回去的版本，"撤销"这个动作本身也留在历史里，事后能查清谁在什么时候恢复了什么。 |
| 撤销前必须检查对象是否还在 | GC 已把 blob 物理删掉的，**没有任何办法恢复**。此时返回 `CONTENT_PURGED` 并说清"有几个对象已被回收"，而不是造一个指向空内容的版本——后者会让用户以为数据回来了，直到检出才发现是空文件。 |
| 失败时不改标记状态 | 撤销失败（内容已回收）时 `purged_paths.state` 保持 `active`：记录还在，用户能看到"这条清理过、且内容已经没了"，而不是记录神秘消失。 |
| 内容从"最近一个仍包含该路径的版本"取 | 不重写历史、不改版本号，只是从旧树里读出 blob 哈希再引用一次。CAS 下这就是零拷贝。 |
| 重复撤销返回 `INVALID_ARGUMENT` | 幂等的"再撤销一次什么都不做"会让人怀疑到底生效没有。明确告诉用户"已经撤销过了"。 |
| UI 用两段式：状态标签 + 撤销按钮 | 三种状态（宽限期 / 已生效 / 已撤销）语义差别很大，尤其"宽限期"意味着还来得及。列表里直接标出来，比统一显示"已清理"有用。 |
| 客户端新建 `protoPath()` 探测顺序：开发态优先仓库根 | 打包会在 `desktop/proto/` 留一份副本；若让它优先，新增 RPC 后开发态会一直用旧契约，报"方法不存在"，排查成本极高（本轮实际踩到）。 |

### M4 第九步：保留策略的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| `path_pattern` 用真正的正则，引入 `regex` crate | 设计 9.7 写的就是 `^20\\d{4}$` 这种形式。自造匹配器（通配 / 前缀）看起来能省一个依赖，但用户迟早要写 `20(1[0-2]|0[1-9])`，那时再造一遍就是灾难。正则语法**在保存策略时**就校验，不留到执行时才报错。 |
| 三条限制是「并集」语义 | `keep_months` / `max_bytes` / `max_age_days` 各自是一条上限，**任何一条被违反**该目录就成为候选。这符合「保留策略 = 一组不该越过的界」的直觉，也避免"三个条件都满足才删"这种几乎不会触发的组合。 |
| 目录按名称倒序当作「从新到旧」 | 按月归档的命名（`202610`）天然可比。规则里没有引入日期解析——那会把"目录名叫什么"和"什么时候创建"绑死，反而不灵活。 |
| 策略默认 `dry_run`，删除必须显式开关 | 自动删数据的默认值只能是"不删"。CLI 需要 `--execute`、界面需要勾「允许自动删除」，服务端还会拒绝"全部策略都是 dry_run 却要求执行"的请求——三层都拦一次，代价很低。 |
| 配置权限收到 owner / maintainer | 手动 `purge` 只要 writer 就够（那是人当场确认的动作）；而策略会在无人值守时删数据，性质不同。用 `can_manage_members` 复用既有角色矩阵，不新造一套。 |
| 执行复用 `purge`，不另写一条删除路径 | 配额预检、路径裁剪、GC、审计、回收站记录全部自动继承。少一条路径就少一处不一致。 |
| 规则扫描只看 HEAD 的顶层目录 | 保留策略是"按月归档"场景的工具，先做对一件事。子目录粒度、按文件规则留给真正需要的人提需求——现在加上只会让语义变模糊。 |
| `purge_estimate` 从 `estimate_purge` 抽出 | 保留策略要预估可释放量，但它是内部调用、不该再走一遍成员与 scope 校验（外层已经校验过）。抽出纯计算部分，权限检查留在公开方法里。 |

### M4 第十步：配额设置页与后台维护的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 配额设置做成弹窗而不是第四块面板 | 侧栏已经三块（仓库 / 成员 / 空间），再加一块就挤了。配额是「偶尔改一次」的设置，弹窗比常驻面板更合适，也避免了把「用量」和「上限」混在一起看。 |
| 弹窗里同时给「全局默认」与「本仓库」 | 设计 9.6 的核心是三层优先级（单仓库覆盖 → 全局默认 → 系统兜底）。只显示本仓库的数字，用户永远不知道为什么生效值是那个数；两个都摆在眼前，`来源` 那一行才有意义。 |
| `repos.grace_days` 用起来了 | schema 里一直有这个字段，但 `purge` 早先硬编码 7 天忽略了它。现在宽限期按仓库配置计算，并补了 `set_repo_grace_days`（写权限即可：这是 owner 自己的空间治理动作，不是全局策略）。 |
| 后台维护只做「转正 + GC」，不自动执行保留策略 | 保留策略要删数据，而 `purge` 的整条链路（`begin_commit` / `finalize_commit`）都要求 session 与成员资格。让系统身份绕过这套权限，等于在权限模型上开了个后门。要做就得正经引入「系统 actor」并逐步放开，那是独立一步——在那之前宁可少做，也不绕。 |
| 维护任务在 `spawn_blocking` 里跑 | `ServerStore` 全是阻塞的 SQLite / 文件操作。直接在 async 任务里调用会把整个 gRPC 运行时卡住一次大 GC 的时长。 |
| 定时器用 `MissedTickBehavior::Delay` | 上一轮超时不补跑：维护是幂等的，堆积反而伤磁盘。 |
| 维护任务 `repo_id` 留空 | 它是系统级任务，不属于任何仓库。代价是 `gc-status` 查不到它（那个接口按仓库维度鉴权），所以 CLI 里明确写「系统级任务，不归属任何仓库」。 |

### M4 第十一步：系统身份与保留策略自动执行的决策（2026-11-10）

| 决策 | 结论与理由 |
| --- | --- |
| 系统身份是 `Session` 上的一个 `kind`，不是「超级管理员」 | 定时任务确实需要一个「没有成员记录」的调用者。做法是给 `Session` 加 `ActorKind::System`，并且**只**跳过成员资格与成员角色这两道门；仓库状态、配额、out-of-date、变更集校验一律照旧。写成 `role = Admin` 的假会话会顺手绕过一堆管理员专属接口，那是另一个后门。 |
| 系统身份的构造**收口在 `Session::system()`** | `authenticate` 的两条路径（JWT / PAT）都显式构造 `ActorKind::User`，所以任何 token 都换不出系统身份。这一点有测试守着（`system_session_cannot_be_authenticated`）。 |
| 放行点逐个标注，而不是在入口统一 `if system { return Ok }` | 现在的做法是把「取成员角色」集中成一个表达式：`if session.is_system() { Owner } else { member_role(...)? }`，共 10 处。每一处都是显式的、可 grep 的；将来要审计「系统到底能做什么」，一次搜索就能列全。 |
| 自动执行只挑「启用且非 dry_run」的策略 | 手动执行时，全部策略都是 dry_run 会报错（提醒用户先关掉）；后台执行则**跳过** dry_run 的策略而不是报错——否则一个仓库里放了一条试运行的策略，就会让整轮维护失败。 |
| 自动执行走回收站宽限期（`immediate=false`） | 无人值守的删除更需要「能反悔」。`vcs maintenance` 之后 `purged` 里能看到这些记录，还能撤销。 |
| 单个仓库失败不中断整轮维护 | 定时任务最忌讳「一个坏仓库卡死全库」。失败只打日志（带错误码），继续下一个仓库。 |
| 维护顺序：保留策略 → 转正 → GC | 策略产生的删除记录可以在**同一轮**里转正（宽限期为 0 的仓库）并回收，用户不必等下一轮。 |

## 16. M0 验收记录（2026-11-10，配额层级与契约收口后复验）

本机环境：Rust 1.95、Node 22、系统未安装 protoc（本地校验由 `grpcio-tools` 提供 protoc，CI 使用 `arduino/setup-protoc`）。

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | 通过，`vcs-core` 4 个单测全绿 |
| `cargo run -p vcs-cli -- "docs/../proto/repo.proto"` | 输出 `proto/repo.proto`，路径规范化生效 |
| `protoc -I=proto --descriptor_set_out=... proto/repo.proto` | 通过，生成 descriptor 7.8 KB |
| `schema/server.sql` / `schema/wc.sql` 在 SQLite 内存库执行 | 通过，分别创建 14 / 7 个表与视图 |
| `desktop` 的 `npm run typecheck` | 通过 |

M0 workspace 保持零外部依赖，可离线 `cargo check`；`vcs-core` 已包含领域模型、`ObjectStore` / `PurgePlanner` 契约、容量治理类型、路径规范化与内存对象存储的测试和实现。tonic / sqlx / blake3 / zstd / napi-rs 均在 M1 引入，不在本阶段产生网络依赖。

## 17. M1 验收记录（2026-11-10）

### 17.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | **62 个测试全绿**：`vcs-core` 42 单测、`vcs-server` 14 集成、`vcs-proto` 4 契约、`vcs-cli` 2 单测 |
| `vcs-proto` 代码生成 | 通过，`protoc` 由 `protoc-bin-vendored` 提供，宿主机无需预装 |
| 生成规模 | 133 个消息、14 个枚举、3 个 service（server + client 双套） |
| `schema/server.sql` | 22 个表 / 视图；`schema/wc.sql` 7 个表 |
| `desktop` 的 `npm run typecheck` | 通过 |

### 17.2 端到端流程（真实执行，非模拟）

```
init-admin（bootstrap，仅空库一次） → login（Argon2id → HS256 JWT）
→ repo-create（管理员直通，写审计 + 登记 owner 成员行）
→ user-create / member-add / member-list（角色矩阵校验）
→ import（两遍扫描：预检 → 落盘 → 单事务分配 rN）
→ checkout（r1/r2 任意版本，同步写 pristine）
→ status（纳秒 mtime 快路径）→ diff（完全离线，LCS 行级 diff）
→ import 再次提交（自动识别 A/M/D）→ log → info
```

关键观测结果：

| 场景 | 观测 |
| --- | --- |
| 首次导入 4 个文件（含 200 KB 全 'A' 的 CSV） | `r1：新增 4`；对象池 **500 字节 / 9 个对象**，与 `find + stat` 逐字节一致（zstd 把 200 KB 压到 200 字节以内） |
| 等长改写 `收入 100` → `收入 120` 后立刻 `status` | 正确报 `M`（修复前会漏报为「干净」） |
| 离线 `diff` | 输出标准 unified 格式，`@@ -1,2 +1,2 @@` 行号正确 |
| 重复 `import` 无改动 | `没有变更：r2 之后工作内容一致，未生成新版本` |
| 非成员读取仓库 | `RepoNotMember: 不是该仓库成员，无权访问` |
| reader 提交 | `RepoMemberForbidden: 当前成员角色为只读，不能提交` |
| 1 KiB 上限仓库导入 50 KB 随机文件 | `QuotaExceeded`，且**对象池前后均为 13 字节**（只有建库时的空树），无孤儿对象 |
| 单仓库 8 MiB 覆盖全局 1 KiB 默认 | 导入成功，`生效配额` 显示「来源：单仓库覆盖」 |
| 检出历史版本 `-r 1` | 得到旧内容；`cat -r 1` 同样正确 |
| 未登录调用 | `尚未登录，请先执行：vcs login --username <用户名>` |

### 17.3 M1 未覆盖（明确留给后续）

- 尚无网络服务：`ServerStore` 未挂 gRPC，Electron 端 IPC 仍是 `notWired` 占位；
- `commit` 仍是「整目录全量提交」，没有 `BeginCommit` 会话、断点续传与单文件级提交；
- 无 `update` / `revert` / 冲突三栏；
- 无 GC：被删除路径的对象仍在盘上，`reclaimable_hint` 只是乐观估计；
- 无 LDAP、无自助注册审批、无 PAT、无 ACL / 锁；
- 无 FastCDC 分块：整个文件作为一个 blob，大文件改动会整份重传。

## 18. M2 验收记录（2026-11-10）

### 18.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | **93 个测试全绿**：`vcs-core` 59 单测、`vcs-server` 28 集成、`vcs-proto` 4 契约、`vcs-cli` 2 单测 |
| `schema/server.sql` / `schema/wc.sql` | 未改动（M1 定稿的 22 / 7 个表视图继续够用） |
| `desktop` 的 `npm run typecheck` | 通过 |

### 18.2 端到端流程（真实执行，非模拟）

```
import（r1 三个文件）
→ checkout wc1 / wc2（同版本两份工作副本）
→ wc1 改一个、加一个、删一个 → status（M/A/D 三项）
→ commit wc1（BeginCommit → 分批 PushObjects → FinalizeCommit → mark_committed）→ r2，status 干净
→ update wc2（GetChanges r1→r2 → apply_update）→ 更新 1 / 新增 1 / 删除 1，status 干净
→ wc1 与 wc2 同时改同一文件，wc1 先 commit（r3）
→ wc2 直接 commit → OutOfDate 拒绝
→ wc2 update → 内容冲突 1 处，本地内容保留，status 显示 C
→ wc2 带冲突 commit → 被拦截
→ wc2 revert → 回基线、冲突清除 → 再 update → 补齐服务端版本（oldest_rev 生效）
```

清理闭环：

```
candidates → 202610 / 202611 各自的逻辑大小与独占大小
→ estimate 202610 → dry-run：可释放 40 B，不会释放 46 B（被 202611 共享），影响 1 个版本
→ purge 202610 --immediate → 删除提交 r2 + 路径裁剪 + GC 清扫 1 个对象，实际释放 40 B
→ cat -r 1 202610/oct.txt → ContentPurged（内容已按容量策略清理）
→ purge 202611（不带 --immediate）→ 进入回收站，gc 清扫 0 字节
```

关键观测：

| 场景 | 观测 |
| --- | --- |
| 预估 vs 实际释放量 | 预估 `reclaimable_bytes` 40 B，GC `reclaimed_bytes` 40 B，**逐字节一致**（同一张反向引用表推算） |
| 共享对象 | 两个月份共享的模板压缩后 46 B，被计入 `shared_bytes`，清理后文件仍在、`cat` 可读 |
| 跨仓库共享 | 另一个仓库引用同一份内容，清理 + 全库 GC 后该内容仍可读（只按路径裁剪，不按仓库裁剪） |
| 旧版本读取 | `ContentPurged`，而不是 `ObjectMissing` |
| 宽限期 | 未加 `--immediate` 时 `gc` 清扫 0 字节，内容保留；PENDING 不会提前转 ACTIVE |
| COW 生效 | 改 202611 后 202610 的文件哈希不变（单测断言） |
| 混合版本 | 更新单个条目后，未触及的条目仍保留旧 `base_rev`；`oldest_rev` 保证下次 update 不遗漏 |

### 18.3 M2 未覆盖（明确留给后续）

- 仍是本地直连形态：`ServerStore` 未挂 gRPC，Electron 端 IPC 仍为占位；
- GC 是同步执行、全量遍历：没有后台 worker、没有分批增量标记，`gc_jobs` 的 `queued/marking/sweeping` 状态仅为 M3 的异步化预留；
- `purged_paths` 只做字面前缀匹配，不支持通配 / 正则（留给 `retention_policies`）；
- 没有「撤销清理（revert purge）」与回收站列举接口，宽限期只能等；
- rename / copy 变更 `apply_update` 直接报错，不做自动应用（需要用户显式确认）；
- 无 FastCDC 分块、无 ACL / 锁、无 LDAP / 自助注册 / PAT。

## 19. M3 第一阶段验收记录（2026-11-10，用户体系闭环）

### 19.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | **101 个测试全绿**：`vcs-core` 59 单测、`vcs-server` 36 集成、`vcs-proto` 4 契约、`vcs-cli` 2 单测 |
| 错误码契约 | 1–36 全部钉死（新增 36 = `INVALID_ARGUMENT`），编号只增不改 |
| `schema/server.sql` / `schema/wc.sql` | 未改动（`registration_requests` / `repo_creation_requests` / `pat_tokens` 早在 M0 就位） |
| `desktop` 的 `npm run typecheck` | 通过 |

### 19.2 端到端流程（真实执行，非模拟）

```
register（免登录，只收用户名/姓名/理由）
→ login → AccountPending（不能登录）
→ 注册时弱密码被拒（列出缺哪几类字符）→ 合规密码成功
→ register-approve（管理员，审批后即可登录）
→ login → 成功（角色 writer）
→ repo-request（申请建库）→ import → RepoNotFound（申请未审批时不可用）
→ repo-approve（管理员，核定 1 GiB 配额上限）→ 仓库创建 + 申请人自动成为 owner + r0
→ info → admin 尝试读取该仓库 → RepoNotMember（admin 也需被 owner 加为成员）
→ 申请人 import → r1
→ pat-create --scopes read → login --pat（读 OK）
→ import → PermissionDenied（令牌没有 write 权限，尽管持有人是 owner）
→ pat-revoke → login --pat → PatRevoked
```

### 19.3 关键观测

| 场景 | 观测 |
| --- | --- |
| 未审批登录 | `AccountPending: 账号待审批，尚不能登录` |
| 已注册未审批就登录 | `AccountPending`，提示「账号待审批」，不是「密码错误」 |
| 弱密码 | `PasswordPolicyViolation`，一次列出「长度不足；缺少数字；缺少大写字母；缺少特殊字符」 |
| 普通用户审批 | `PermissionDenied: 仅管理员可审批仓库申请` |
| 未审批仓库 | `RepoNotFound`，`repo_creation_requests` 里只有单据，没有 `repo_id` |
| 审批后归属 | owner 成员行 + r0 空版本 + r0 作者为申请人 |
| admin 使用他人仓库 | `RepoNotMember`（管理通道不等于成员资格） |
| 只读 PAT 提交 | `PermissionDenied: 当前访问令牌没有 write 权限，不能提交` |
| 撤销后 | `PatRevoked`，且列表里明文永不出现 |

### 19.4 本阶段未覆盖

- **LDAP 登录与组角色映射**：当时列为「需要 mock 目录，单独一轮」；2026-11-12 已接入，见 §33（真机 mock 目录集成测试仍待补）；
- **gRPC 服务层**：`ServerStore` 仍未挂 tonic，Electron 的 IPC 还是占位；审批台目前只有 CLI 形态；
- **ACL（路径级权限）与锁**；
- 注册防刷（IP / 用户名限流、验证码）；
- refresh token 轮换：目前 JWT 短时有效，过期需重新登录；
- 成员被移除时批量把未完成 `upload_sessions` 置为 `aborted`（`FinalizeCommit` 的二次校验已能兜住结果，但存量会话不会主动清理）。

## 20. M3 第二阶段验收记录（2026-11-10，gRPC 服务层）

### 20.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | **109 个测试全绿**：`vcs-core` 59、`vcs-server` 36、`vcs-proto` 4、`vcs-grpc` 4 单测 + 4 端到端、`vcs-cli` 2 |
| 端到端形态 | 测试里**真的起服务、真的走 TCP**（内核分配端口 + `serve_with_incoming_shutdown`），不是 mock |
| `desktop` 的 `npm run typecheck` | 通过 |

### 20.2 覆盖的 RPC

| service | 已实现 |
| --- | --- |
| `AuthService` | Login、Register、GetRegistration、ListRegistrations、ApproveRegistration、RejectRegistration、WhoAmI、CreatePat、ListPats、RevokePat |
| `RepoService` | GetHead、GetRevision、GetTree、GetLog、Checkout（服务端流）、Update、FetchObjects（双向流）、BeginCommit、PushObjects（客户端流）、FinalizeCommit、AbortCommit、GetQuota、ListPurgeCandidates、EstimatePurge、Purge、GetGcStatus |
| `RepoAdminService` | RequestRepo、GetRepoCreation、ListRepoCreations、ApproveRepoCreation、RejectRepoCreation、CreateRepoDirect、ListRepos、GetRepo、ListMembers、AddMember、SearchUsers、GetQuotaDefaults、UpdateQuotaDefaults、GetRepoQuota |

明确返回 `UNIMPLEMENTED` 的（当时清单，LDAP 三项的后续补齐见 §33）：Lock/Unlock、RefreshToken、Logout、GetLdapConfig/UpdateLdapConfig/TestLdapConnection、SetUserStatus/SetUserRole。

（`SetRepoStatus` / `RemoveMember` / `SetMemberRole` / `SetRepoQuota` 已在 M3 第三阶段补齐，见 §21。）

### 20.3 端到端实测

| 场景 | 观测 |
| --- | --- |
| 无凭证调用 `GetHead` | 传输层 `UNAUTHENTICATED`（客户端据此跳登录页，而不是弹业务错误） |
| 密码错 | 200 + `INVALID_CREDENTIALS`（编号 12，验证错误码没错位） |
| 完整提交链 | `BeginCommit` 返回 `missing_hashes=[blob]` → 客户端流推对象 → `PushAck{1 个对象}` → `FinalizeCommit` 返回 `rev=1` 与 `root_tree` |
| `GetTree` | 返回 `202610/summary.txt`，与 `Checkout` 流里的对象内容一致 |
| `GetLog` | r0 + r1 两条，r1 的 message 与提交时一致 |
| `GetQuota` | `used_bytes > 0`，`effective.source = REPO_OVERRIDE` |
| 过期提交 | `base_rev=99` → `OUT_OF_DATE`（编号 2） |
| 非成员读仓库 | `REPO_NOT_MEMBER`（编号 28），加成员后同一请求变 `OK` |
| `ListRepos` | 只返回自己参与的仓库（bob 加成员后能看到 1 个） |
| 注册→审批→登录 | 全走网络接口；注册时弱密码被拒、未审批不可登录、审批后同一密码可登录 |
| 30 天免登录 | 全走网络接口：`remember_me=false` 签发 12 小时、`true` 签发 30 天，长期 token 的 `WhoAmI` 可用 |
| 未实现方法 | `Lock` 返回 `UNIMPLEMENTED`，不会假装成功 |
| 清理预估 | 空仓库 `EstimatePurge` 正常返回 0 字节，而非报错 |

### 20.4 本阶段未覆盖

- **Electron 仍未接线**：IPC 还是 `notWired` 占位，M4 才把 GUI 接到这层（这是下一个里程碑的主要工作）；
- 内置 TLS / 鉴权网关：当前只绑 `127.0.0.1`，对外部署需前置反向代理；
- 连接池与 `spawn_blocking`：所有 RPC 共享一把锁，长事务（大提交）会阻塞其他请求；
- 服务端流没有背压控制：`Checkout` 一次性把对象读进内存再分包，仓库很大时会吃内存（M4 改成分批惰性读取）；
- 上面列出的 `UNIMPLEMENTED` 方法（成员与仓库治理四项已在 §21 补齐）；
- CLI 仍是本地直连，暂无 `--remote` 模式（改造成本低，等 Electron 定稿后一起做，避免两套客户端代码）。

## 21. M3 第三阶段验收记录（2026-11-10，成员 / 配额 / 仓库状态治理）

这一阶段补齐的是「GUI 做成员管理与仓库设置页所需的最小 API 集」——没有它们，M4 的界面只能读不能改。

### 21.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | **114 个测试全绿**：`vcs-core` 59、`vcs-server` 40、`vcs-proto` 4、`vcs-grpc` 4 单测 + 5 端到端、`vcs-cli` 2 |
| `desktop` 的 `npm run typecheck` | 通过 |

### 21.2 新增能力

| 层 | 内容 |
| --- | --- |
| `ServerStore` | `remove_member`、`set_member_role`、`set_repo_quota`、`set_repo_status`、`head_info`、`username_by_id`、`abort_sessions_of` |
| gRPC | `RemoveMember`、`SetMemberRole`、`SetRepoQuota`、`SetRepoStatus`（当时 `UNIMPLEMENTED` 剩 Lock/Unlock、RefreshToken、Logout、LDAP 三项、SetUserStatus/SetUserRole；**LDAP 三项已于 2026-11-12 补齐，见 §33**） |
| CLI | `repo-list`、`member-remove`、`member-set-role`、`repo-quota`、`repo-status` |

### 21.3 关键观测

| 场景 | 观测 |
| --- | --- |
| owner 移除 writer | 成员行删除；对方手上的 `BeginCommit` 会话立刻变 `aborted`，`FinalizeCommit` 报 `INVALID_SESSION` |
| 移除 / 降级 owner | `REPO_OWNER_PROTECTED` |
| maintainer 移除 reader | 成功；maintainer 移除自己 → `REPO_MEMBER_FORBIDDEN` |
| writer 降为 reader | 会话作废，之后 `BeginCommit` 报 `REPO_MEMBER_FORBIDDEN` |
| maintainer 把人提成 maintainer | `REPO_MEMBER_FORBIDDEN` |
| 设置 1 MiB 上限 | `QuotaSource::RepoOverride`；`--inherit` 后回到 `FALLBACK` |
| 非管理员改配额 | `PERMISSION_DENIED` |
| 禁用仓库 | 在途会话作废；`GetHead` / `BeginCommit` / `FilesAt` 全部 `REPO_DISABLED`；恢复 active 后立即可用 |
| 网络层一致性 | 同样的四条链路在 gRPC 端到端测试里跑过一遍（加成员 → 改角色 → 移除 → 被移除者失去访问权；配额设置 → 回读；禁用 → 拒绝 → 启用） |

### 21.4 剩余（M3 收尾项）

- ✅ **LDAP 登录与组角色映射**：2026-11-12 已接入（并同时实现首位用户引导），见 §33；
- **ACL（路径级权限）与锁**（`Lock`/`Unlock` 仍是 `UNIMPLEMENTED`）；
- `SetUserStatus` / `SetUserRole` / `RefreshToken` / `Logout`；
- 注册防刷、审计查询接口（目前审计只写不读）。

## 22. M4 验收记录（2026-11-10，提交 / 更新 / 冲突 / 还原闭环）

### 22.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` | 通过，**114 个测试全绿**（本轮未改 Rust 业务代码） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（起真实 `vcs serve`，走真实 HTTP/2 与真实文件系统） |
| 原生模块 | `cargo build -p vcs-napi` 零警告；`wc` 相关行为全部来自 `vcs-core`（114 个 Rust 测试覆盖） |
| CI | `desktop` job：构建 Rust 二进制 → npm install → typecheck → 闭环冒烟（冒烟内含 `build:native`） |

### 22.2 冒烟覆盖的链路

```
bootstrap 管理员（CLI 直连存储根）→ 启动 vcs serve（真实端口）
→ 登录（JWT）→ 创建仓库（管理员直通）
→ 检出 r0（GetTree + Checkout 对象流 → napi 写 wc.db 与 pristine；校验归属）
→ 写入 202610/summary.txt → 状态 ADD → 离线 diff（+十月汇总 / +第二行）
→ 提交 r1（BeginCommit → PushObjects 1 个对象 → FinalizeCommit）→ 本地干净
→ 用 CLI 扮演「另一个人」从另一目录提交（+extra.txt）→ 服务端到 r2
→ 本地直接提交 → 被拒（「本地 r1 已落后于仓库 r2」）
→ 更新（Update → FetchObjects → napi apply_update）→ r2，新增 1，本地拿到 extra.txt
→ 本地与远端同时改 summary.txt → 远端提交 r3 → 本地更新 → 1 处内容冲突
→ 本地内容保持不变；冲突列表非空；带冲突提交被拒
→ 还原 → 清冲突，summary.txt 回到 r1 基线
→ 再次更新 → oldestRev 落后，补齐 r3 的远端内容（混合版本修复生效）
→ 本地恢复干净；服务端 head=r3
```

### 22.3 界面形态

单窗口三段式：顶栏连接（地址 / 用户名 / 密码 / 当前身份）、左侧仓库列表与新建、右侧工作副本（检出目录 → 变更列表 → 差异面板 → 提交框 → 历史）。
渲染进程零 Node 权限，所有操作经 preload → 主进程 IPC。

### 22.4 本阶段未覆盖

- **冲突只有元数据，没有三栏合并界面**：`wc.conflicts` 已能拿到 base/mine/theirs 三个哈希，界面还没做对比与手工合并；
- **稀疏检出 / 单文件更新**：`GetTree` 仍返回扁平全量，update 只能整棵拉；
- 注册、审批台、成员管理、配额配置页、空间面板、打包与自动更新都还没做；
- 连接使用明文 HTTP/2（`createInsecure`）：仅限本机或内网，对外必须前置 TLS 网关；
- token 只存在主进程内存，重启应用需要重新登录（后续接 `safeStorage`）。

## 23. M4 第三步验收记录（2026-11-10，注册与审批台）

### 23.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` | 通过，114 个测试全绿（本轮未改 Rust 代码） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增注册 → 审批 → 设密码 → 建库申请 → 审批 五段） |

### 23.2 新增的界面能力

| 区域 | 能力 |
| --- | --- |
| 登录页 | 「本地账号 / LDAP 账号」tab、用户名 / 密码 / 「30 天内免登录」、申请注册（用户名 / 姓名 / 密码 / 理由）、「设置」（覆盖服务器地址） |
| 主界面顶栏 | 当前用户、服务端地址、会话到期提示、「退出登录」 |
| 仓库面板 | 管理员：直通新建；普通用户：提交建库申请（仓库名 + 用途说明） |
| 审批台（仅管理员） | 待审批注册（可选角色、只放行不发凭证、拒绝需填原因）、待审批建库（通过 / 拒绝） |

### 23.3 冒烟新增链路

```
register(bob) → 待审批列表里能看到 → approve（role=writer）
→ register 直设密码 → 未审批登录被拒 → 审批后同一密码可登录
→ bob 登录成功（role=2）
→ bob 申请仓库 bob-repo → 审批前 listRepos 看不到
→ 管理员在待审批列表里看到 → approve → bob 的 listRepos 出现该仓库
→ bob getHead 成功（r0，owner 身份）
```

### 23.4 本阶段未覆盖

- 成员管理页（add/remove/setRole 的 RPC 已就绪，界面还没做）；
- 空间面板（`GetQuota` / `EstimatePurge` / `Purge` / `GetGcStatus` 的 IPC 仍是 `notWired`）；
- 注册防刷（限流、验证码）与「我的申请」状态查询页；
- 冲突三栏合并、稀疏检出、打包与自动更新。

## 24. M4 第四步验收记录（2026-11-10，成员管理）

### 24.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` | 通过，114 个测试全绿（本轮未改 Rust 代码） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增成员管理四段） |

### 24.2 新增界面

左侧栏拆成「仓库 + 成员」两块：

| 能力 | 说明 |
| --- | --- |
| 成员列表 | 用户名（姓名）+ 角色下拉 + 移除；owner 行只显示标签，不可操作 |
| 加人 | 按用户名 / 姓名搜索 → 选择角色（writer / reader / maintainer）→ 添加；已是成员的显示「已是成员」 |
| 即时反馈 | 改角色或移除后立刻重新拉取成员列表；降为只读时提示「其手上的提交会话已作废」 |

### 24.3 冒烟新增链路

```
bob 的仓库 bob-repo：alice（管理员）加人前 getHead → REPO_NOT_MEMBER（28）
→ bob 搜到 alice → addMember(writer) → listMembers 里角色=3
→ alice getHead 成功（成员资格才是一切的前提，管理员也不例外）
→ setMemberRole(reader) → 列表里角色=4
→ bob 移除自己 → REPO_OWNER_PROTECTED（31）
→ bob 移除 alice → alice 再 getHead → REPO_NOT_MEMBER（28）
```

### 24.4 本阶段未覆盖

- 空间面板（`GetQuota` / `EstimatePurge` / `Purge` / `GetGcStatus` 仍是 `notWired`）；
- 仓库设置里的配额调整页（`SetRepoQuota` 已可用，界面未做）；
- 冲突三栏合并、稀疏检出、打包与自动更新；
- 成员变更的审计查询界面（审计只写不读）。

## 25. M4 第五步验收记录（2026-11-10，空间治理面板）

### 25.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` | 通过，114 个测试全绿（本轮未改 Rust 代码） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增空间治理四段） |

### 25.2 新增界面

侧栏第三块「空间」：

| 能力 | 说明 |
| --- | --- |
| 用量条 | 已用 / 配额上限百分比，85% 起变黄、95% 起变红；文末标注生效来源（单仓库覆盖 / 全局默认 / 系统兜底） |
| 候选目录 | 顶层目录列表，勾选框 + 「可释放（独占，含历史）· 当前（HEAD 字节）」 |
| 预估所选 | dry-run：可释放字节、影响历史版本数、被共享而不会回收的来源 |
| 清理所选 | 确认框写明「生成删除提交 + 是否立即释放 + 不可撤销」；执行后展示新版本号与 GC 实际释放量 |

### 25.3 冒烟新增链路

```
GetQuota → used=578 字节，来源=系统兜底
→ ListPurgeCandidates → 202610：可释放 58 / 当前 32
→ EstimatePurge(202610) → 可释放 58 字节，影响 3 个历史版本（未改动数据）
→ Purge(202610, immediate) → 生成 r4
→ GetGcStatus → DONE，清扫 3 个对象，释放 58 字节
→ GetTree(HEAD) → 不再包含 202610/*
→ GetQuota → 对象池 578 → 520 字节
```

### 25.4 本阶段未覆盖

- 仓库设置里的配额调整页（`SetRepoQuota` 已可用，界面未做）；
- 清理记录 / 回收站列举（`purged_paths` 只写不读，没有撤销入口）；
- 保留策略（`retention_policies`）没有任何界面；
- 冲突三栏合并、稀疏检出、打包与自动更新。

## 26. M4 第六步验收记录（2026-11-10，打包与首次运行体验）

### 26.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` | 通过，114 个测试全绿（本轮未改 Rust 代码） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（功能逻辑未变，凭证持久化不影响无界面链路） |
| `npx electron-builder --dir --config.asar=false` | **成功产出 `svn-next.app`**，逐项核对包内文件 |
| 代码签名 | 未做（`identity: null`），产物仅用于内部试用 |

### 26.2 打包产物核对

```
svn-next.app/Contents/Resources/app/
├── dist/main/index.js        主进程
├── dist/main/wc.js           工作副本（加载 native）
├── native/vcs-napi.node      原生模块（11 个导出，Node 直接 require 可加载）
├── proto/repo.proto          gRPC 契约（由 scripts/stage-assets.cjs 暂存）
└── package.json

产物大小 273 MB（含 Electron 运行时 + node_modules 中 @grpc 依赖）
```

### 26.3 首次运行体验

| 场景 | 行为 |
| --- | --- |
| 首次启动 | 无预填，直接登录 |
| 再次启动 | 自动填好服务端地址与用户名；若有加密凭证则直接恢复会话并提示「已用保存的凭证恢复」 |
| 凭证失效 / 钥匙串重置 | 静默清除凭证，回到登录框，不弹错 |
| 系统不支持加密存储 | 不写凭证，界面提示「重启后需要重新登录（连接信息仍会保留）」 |
| 点「断开连接」 | 关闭连接并清除凭证，保留地址与用户名 |

### 26.4 本阶段未覆盖

- **代码签名与自动更新**：需要证书与发布通道，属于交付阶段的工作；
- asar 打包在本次沙箱环境中被文件服务拒绝（`createWriteStream` 到 asar），已用 `--config.asar=false` 验证；`asar: true` 的路径改写逻辑（`app.asar.unpacked`）需要在真实机器上补一次实测；
- 应用图标仍是 Electron 默认图标；
- 冲突三栏、仓库配额设置页、回收站与保留策略界面。

## 27. M4 第七步验收记录（2026-11-10，冲突三栏合并）

### 27.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通过 |
| `cargo test --workspace` | **118 个测试全绿**（`vcs-core` 63，本轮新增 4 个冲突相关单测） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增三栏合并、第二次冲突还原两段） |

### 27.2 新增能力

| 层 | 内容 |
| --- | --- |
| `vcs-core` | `ConflictRecord` / `ConflictChoice`、`conflict_records`、`conflict_record`、`resolve_conflict`、`advance_normal_bases`；`apply_update` 冲突时缓存 theirs |
| `vcs-napi` | `wcConflictDetail`（三方内容，Buffer）、`wcResolveConflict`、`wcAdvanceBase` |
| 桌面端 | 变更列表里冲突项点开后是**三栏**（base / mine / theirs）+ 可编辑合并结果 + 四个动作按钮；二进制文件禁用文本合并 |
| `flow` | update 成功后推进干净条目的基线版本，使 `oldest_rev` 收敛 |

### 27.3 冒烟新增链路

```
冲突产生（本地改了 summary.txt，远端也改了）
→ 带冲突提交被拒
→ conflictDetail：base=「十月汇总/第二行」 mine=「本地改的」 theirs=「远端改的」（全程离线）
→ resolveConflict(custom, "合并结果：本地+远端")
→ 冲突清除，status 显示 MODIFY（待提交）
→ 提交 r4 → 下载服务端对象校验内容一致
→ 第二次冲突（本地/远端各改一次）→ 还原清冲突
→ 再 update：oldestRev 从旧基线追平到 r5，内容补回「远端第二次」
```

### 27.4 本阶段未覆盖

- 行级三栏合并（现在展示整份内容，没有逐行对齐高亮）；
- 「我的申请」状态页、回收站与撤销清理、仓库配额设置页；
- 代码签名与自动更新。

## 28. M4 第八步验收记录（2026-11-10，回收站与撤销清理）

### 28.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通过 |
| `cargo test --workspace` | **121 个测试全绿**（`vcs-server` 43，本轮新增 3 个回收站集成测试） |
| 契约 | `proto/repo.proto` 新增 `ListPurgedPaths` / `RevertPurge` 两个 RPC 与三条消息；契约测试仍通过 |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增回收站两段） |
| CI CLI 冒烟 | 通过（新增 `purged` / `unpurge` 断言） |

### 28.2 新增能力

| 层 | 内容 |
| --- | --- |
| 契约 | `PurgedPathInfo`、`ListPurgedPaths`、`RevertPurge` |
| `vcs-server` | `list_purged_paths`（成员可见）、`revert_purge`（写权限；恢复为新提交；内容缺失返回 `CONTENT_PURGED`） |
| `vcs-grpc` | 两个 RPC 的适配 |
| CLI | `vcs purged <repo>`、`vcs unpurge <repo> <purge_id>` |
| 桌面端 | 空间面板下新增「回收站」：路径 + 状态（宽限期 / 已生效 / 已撤销）+ 撤销按钮 |

### 28.3 冒烟新增链路

```
（桌面）清理 202610 到回收站（immediate=false）
→ ListPurgedPaths：1 条 pending
→ RevertPurge → 新版本 r7，HEAD 里重新出现 202610/summary.txt，内容仍是「远端第二次」
→ 记录状态变 reverted
→ 再次立即清理（immediate=true）→ GC 清扫 5 个对象
→ ListPurgedPaths：新记录 active
→ RevertPurge → CONTENT_PURGED（"有 2 个对象已被 GC 物理回收"），记录保持 active

（CLI）vcs purged 显示「已生效」 → vcs unpurge 同样失败退出
```

### 28.4 本阶段未覆盖

- 保留策略（`retention_policies`）的界面与自动执行；
- 仓库配额设置页（`SetRepoQuota` 可用未接 UI）；
- 回收站的「彻底删除」动作（现在是等 GC，或立即释放后自动清空）；
- 行级三栏合并、代码签名与自动更新。

## 29. M4 第九步验收记录（2026-11-10，保留策略）

### 29.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通过 |
| `cargo test --workspace` | **124 个测试全绿**（`vcs-server` 46，本轮新增 3 个保留策略集成测试） |
| 契约 | 新增 `RetentionPolicyInfo` 等 6 条消息与 4 个 RPC；契约测试通过 |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增保留策略一段） |
| CI CLI 冒烟 | 通过（新增 `retention-set/list/apply` 断言） |

### 29.2 新增能力

| 层 | 内容 |
| --- | --- |
| 契约 | `ListRetentionPolicies` / `UpsertRetentionPolicy` / `DeleteRetentionPolicy` / `ApplyRetention` |
| `vcs-server` | `list_retention_policies`、`upsert_retention_policy`（校验正则与至少一条限制）、`delete_retention_policy`、`apply_retention`（并集语义 + dry-run 保护）、`retention_candidates`、`purge_estimate` 抽取 |
| CLI | `retention-list` / `retention-set` / `retention-rm` / `retention-apply` |
| 桌面端 | 空间面板「保留策略」：策略列表（模式 + 规则 + 状态）、新建表单（正则 / 保留个数 / 字节上限 / 天数 / 允许自动删除）、评估与执行按钮 |

### 29.3 冒烟新增链路

```
（桌面）建 archive 仓库并归档 202609/202610/202611 → r1
→ 保存策略 ^20\d{4}$ keep_months=2（默认 dry_run）
→ 预览：候选 202609，可释放 18 字节，未产生提交
→ 试图执行 → INVALID_ARGUMENT（策略是 dry-run）
→ 把策略切成可执行 → 执行 → r2，HEAD 里只剩 202610/202611
→ 再次评估：无候选

（CLI）retention-set 默认仅预览 → retention-apply --dry-run 列出 202609
→ 不带 --dry-run 执行 → 被拒（"所有生效的策略都处于 dry_run"）
```

### 29.4 本阶段未覆盖

- 保留策略的**自动定时执行**（现在需要手动触发；设计 9.7 的"到期自动执行"需要后台任务，与 GC 的异步 worker 一起做更合理）；
- 通知（清理前提醒 / 执行后汇总）；
- 子目录粒度与按文件规则；
- 仓库配额设置页、代码签名与自动更新。

## 30. M4 第十步验收记录（2026-11-10，配额设置页与后台维护）

### 30.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通过 |
| `cargo test --workspace` | **127 个测试全绿**（`vcs-server` 49，本轮新增 3 个维护 / 宽限期测试） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（新增配额设置一段） |
| CI CLI 冒烟 | 通过（新增 `vcs maintenance`） |

### 30.2 新增能力

| 层 | 内容 |
| --- | --- |
| `vcs-server` | `run_maintenance`（转正到期清理 + 全库 GC）、`promote_pending_purges` 抽出、`set_repo_grace_days`、`purge` 改为按 `repos.grace_days` 计算宽限期 |
| `vcs-grpc` | `ServeConfig.maintenance_interval_secs`，由 `vcs serve --maintenance-interval` 传入（默认 300 秒，0 关闭）；定时器在 `spawn_blocking` 里跑维护 |
| CLI | `vcs maintenance`：手工触发一次，输出转正条数 / 清扫对象数 / 释放字节 |
| 桌面端 | 空间面板「配额…」弹窗：全局默认（仅管理员可改）+ 本仓库配额（覆盖 / 恢复继承），并显示生效值与来源 |

### 30.3 冒烟新增链路

```
（桌面）全局默认从 0 改为 2 MiB
→ 未核定的 reports 仓库生效来源变成「全局默认」(2)
→ 给 reports 设 1 MiB 覆盖 → 来源变「单仓库覆盖」(1)，生效上限 1 MiB
→ 恢复继承 → 来源回到 (2)

（CLI）vcs maintenance → 输出「维护完成：转正 N 条，清扫 M 个对象，释放 X 字节，覆盖 K 个仓库」
```

### 30.4 本阶段未覆盖

- ~~保留策略的自动执行~~：已在 §31 通过「系统身份」落地（定时任务会执行非 dry_run 的策略）；
- `set_repo_grace_days` 已可用但**没有 RPC 与界面**（服务端 API 就绪，等需要时再接）；
- 维护任务不在 `gc-status` 里可见（系统级任务，不归属仓库）；
- 代码签名与自动更新、应用图标。

## 31. M4 第十一步验收记录（2026-11-10，系统身份与保留策略自动执行）

### 31.1 检查项

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt` / `clippy -D warnings` | 通过 |
| `cargo test --workspace` | **130 个测试全绿**（`vcs-server` 52，本轮新增 3 个系统身份 / 自动执行测试） |
| `desktop`: `npm run typecheck` | 通过 |
| `desktop`: `npm run smoke` | 通过（未改动桌面链路，回归通过） |
| CI CLI 冒烟 | 通过（策略切可执行 → `vcs maintenance` → `purged` 里出现 202609） |

### 31.2 新增能力

| 层 | 内容 |
| --- | --- |
| `vcs-server` | `ActorKind` / `Session::system()` / `is_system()`；成员门与角色门对系统身份放行（10 处显式标注）；`apply_retention_all` / `apply_retention_auto`；`MaintenanceReport` 增加 `retention_repos` / `retention_paths` |
| `vcs-cli` | `vcs maintenance` 输出加入保留策略统计 |
| `vcs-grpc` | 后台维护日志加入保留策略清理数量 |

### 31.3 冒烟新增链路（CI）

```
retention-set（默认仅预览）→ retention-apply 被拒（INVALID_ARGUMENT）
→ retention-set --execute（切成可执行）
→ vcs maintenance
   维护完成：保留策略清理 1 个仓库（1 个目录），转正 0 条清理记录，清扫 0 个对象…
→ vcs purged monthly → 出现 202609（宽限期内，可撤销）
```

### 31.4 本阶段未覆盖

- 保留策略的**定时调度粒度**：目前与 GC 共用同一个 tick（默认 300 秒），没有按策略单独排期；
- 系统身份尚未覆盖「备份 / 对账」这类将来的无人任务（目前只用于维护链路）；
- `set_repo_grace_days` 仍无 RPC / 界面；
- 代码签名与自动更新、行级三栏合并。

## 32. 打包流水线（未签名）

### 32.1 触发方式

| 方式 | 行为 |
| --- | --- |
| 打 tag（`v*`） | 三平台构建 + 上传 artifact + **创建 GitHub Release** 并把包装上去 |
| 手动 `workflow_dispatch` | 只构建并上传 artifact，不发 Release（避免误发） |

日常 push 不触发——三平台打包各要几分钟，不该占用每次提交的反馈时间。

### 32.2 每个平台做什么

```
checkout → Rust 工具链 + 缓存 → Node 22 + npm 缓存
→ npm ci
→ [Windows] 用 node-gyp 缓存里的 node.lib 设置 RUSTFLAGS
→ [Linux]  装 libfuse2t64（ubuntu-24.04 默认没有，AppImage 工具会失败）
→ npm run stage（cargo build 原生模块 + 复制 proto）→ npm run build（tsc）
→ electron-builder --publish never
→ 上传 dmg / zip / exe / AppImage
```

产物：macOS `dmg` + `zip`、Windows `nsis` + `zip`、Linux `AppImage`（deb 需要额外系统工具，收益不大，未纳入）。

### 32.3 三个平台相关的坑（都已在工作流里处理）

| 问题 | 处理 |
| --- | --- |
| macOS 构建时尝试签名 | `CSC_IDENTITY_AUTO_DISCOVERY=false` + 配置里 `identity: null`，日志会显示 "skipped macOS code signing" |
| Windows MSVC 链接缺 `node.lib` | `napi-build` 只处理 windows-gnu；MSVC 需要 `node.lib`。用 `node-gyp install` 拉进缓存后递归查找，再通过 `RUSTFLAGS=-C link-arg=<node.lib>` 交给链接器 |
| ubuntu-24.04 缺 libfuse2 | AppImage 打包工具需要 `libfuse.so.2`，先 `apt-get install libfuse2t64`（回退 libfuse2） |
| Electron 二进制重复下载 | `ELECTRON_SKIP_BINARY_DOWNLOAD=1`：electron-builder 自己会拉运行时，`node_modules/electron` 里那份不必再下 |

### 32.4 本地验证到哪一步

| 步骤 | 结果 |
| --- | --- |
| `npm ci`（锁文件一致性） | 通过（把 `electron-builder` 补进了 devDependencies 与 lock；此前它只在 node_modules 里，CI 会找不到） |
| `npm run stage && npm run build` | 通过 |
| `electron-builder --publish never`（asar 关闭） | 通过，产出 `svn-next-0.1.0.dmg` / `-mac.zip` / blockmap |
| `electron-builder`（asar 开启） | **未在本地验证**：本机沙箱的文件服务拒绝写 asar（`createWriteStream` 被 broker 拒绝，加沙箱豁免也无效）。CI 上会走 asar 路径，首次跑通即为验证 |

### 32.5 已知限制

- **未签名**：macOS 首次打开需右键 → 打开（或 `xattr -dr com.apple.quarantine`），Windows 会出现 SmartScreen「未知发布者」。Release 说明里已写明；
- **架构取决于 runner**：`macos-latest` 目前是 arm64，因此 mac 产物是 arm64；要同时出 x64 需要加 `--x64` 并为原生模块交叉编译（`cargo build --target x86_64-apple-darwin`），本轮未做；
- **无自动更新**：需要更新服务器与签名，属于交付阶段的事；
- 工作流本身尚未在 GitHub 上跑过（本机无法执行 Actions），首次运行可能需要微调缓存路径或系统依赖。

---

## 33. 首位用户引导与 LDAP 接入（2026-11-12）

### 33.1 需求与结论

需求原话：**「第一个注册或者第一个使用 LDAP 登录的用户就自动成为管理员」**。本轮同时把 LDAP 登录从「协议已定义、实现 `UNIMPLEMENTED`」推到可用。

- 两条入口共用 `ServerStore::first_user_slot()`：在 `BEGIN IMMEDIATE` 写事务里判定「用户表为空且没有引导审计记录」，谁先落库谁拿走唯一的管理员名额；跨 CLI / gRPC 进程互斥，删掉首用户也不会重发名额。
- `init-admin` 从「部署前置条件」降级为可选预置 / 救援：只在没有**可登录**管理员时可用；首位注册者没设密码时可用同名 `init-admin` 重置口令，避免 token 丢失导致全库锁死。
- 首位注册也在表单里设置密码：申请单直接 `approved`，注册完即可登录，`RegisterResponse` 不再携带任何凭证。
- 首位 LDAP 用户由 `login_ldap()` 直接建 `ACTIVE` + `admin` 映射并签发 JWT；之后 JIT 用户按 `ldap_group_roles` 取初始角色（未命中 `readonly`），**已有账号角色以本地为准**，防目录侧组调整把引导管理员降级。

### 33.2 改动范围

| 层 | 内容 |
| --- | --- |
| `vcs-core::auth` | `seal_secret` / `open_secret`：HMAC-SHA256 keystream + encrypt-then-MAC，加密 LDAP bind 密码（域分隔，密钥来自 `server.secret`） |
| `vcs-server/src/ldap.rs`（新） | `ldap3 0.11`（sync + `tls-rustls`）：LDAPS / StartTLS、RFC 4515 转义、服务账号 bind → 搜索用户 → 二次 bind、组查询、连通性测试；默认校验证书，连接/操作各 5 秒超时 |
| `vcs-server::ServerStore` | `register` 返回 `RegisterOutcome`（首位自动批准）；`login_ldap` JIT；LDAP 配置读写 / 组映射 / 权限；`bootstrap_admin` 救援；`first_user_slot` |
| `vcs-grpc` | `Login` 的 LDAP 分支（`spawn_blocking` 做同步 bind）、`Get/Update/TestLdapConfig` 三个 RPC、`RegisterResponse` 新字段 |
| `vcs-cli` | `register` 打印首位 token；`login --ldap`；`ldap-config` / `ldap-show` / `ldap-test` |
| `desktop` | 登录页账号类型 tab（LDAP 账号）、打包期默认地址 + 设置覆盖、注册表单直接收密码（首位注册后自动回填用户名）、管理员「目录设置」面板（配置 + 测试连接） |

### 33.3 协议变更（只新增字段，编号不复用）

| 消息 | 新增 |
| --- | --- |
| `RegisterResponse` | `auto_approved=6`、`granted_role=7`（原 4/5 的一次性 token 字段已 reserved） |
| `LdapConfig` | `group_roles=12`（新 `GroupRole{group_dn, role}`）；`tls_mode` 注释明确禁止明文 |
| `GetLdapConfigResponse` | `bind_password_set=3` |

`desktop/proto/repo.proto` 副本同步更新；`ErrorCode` 编号未动，契约测试保持全绿。

### 33.4 安全边界

- **抢注风险**：空库且开放注册时，最先到达的人拿走管理员。缓解：名额事务内只发一次 + 引导审计 + 抢先执行 `init-admin` 可关闭该入口；对公网暴露的场景应前置 TLS 网关并先预置管理员。
- 目录不可用只返回 `LDAP_UNAVAILABLE`，本地账号不受影响；用户不存在与口令错误统一 `INVALID_CREDENTIALS`。
- bind 密码明文既不落盘也不回传（`server.db` 二进制检索断言）；读取接口返回 `bind_password_set` 而非密码。
- 目录配置 / 测试接口要求 `admin` + `admin` scope；CLI 本机运维通道（`actor=None`）与 `init-admin` 同级权限。

### 33.5 验收与遗留

| 检查项 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通过（零警告） |
| `cargo test --workspace` | 通过：新增首位注册 / LDAP 首用户 / 名额互斥 / 配置加密 / 组映射 / 权限 / gRPC 端到端首用户注册等用例 |
| `desktop` 的 `npm run typecheck` | 通过 |
| `desktop` 冒烟 | 增加断言：已有管理员时注册不自动批准、不返回 token |

遗留：真机 / mock 目录的端到端 bind 测试、目录连接池、嵌套组解析、注册接口限流。当前 LDAP 测试用注入 `DirectoryUser` 覆盖业务层，目录协议层覆盖转义、URL 与配置校验。

---

## 34. M5：管理端 Web 化、桌面端职责收敛与菜单式布局（2026-11-12）

### 34.1 诉求与边界判断

用户要求：管理端单独做 Web 页面并集成进 `vcs`；桌面端只提供仓库使用功能；客户端界面没有菜单、布局混乱，需要优化。

边界按「是否需要治理权限」切分，而不是按 proto 里的 service 名：

| 移到 Web 管理端 | 留在桌面端 |
| --- | --- |
| 审批台（注册 / 建库）、LDAP 目录设置、管理员直通新建、全局与单仓库配额核定、成员增删与角色、空间治理（清理 / 回收站 / 保留策略 / GC）、全库用户与仓库视图 | 登录页登录 / 注册、30 天免登录、退出登录、提交建库申请、仓库列表与选择、检出 / 状态 / diff / 冲突三栏 / 提交 / 更新 / 还原 / 历史、**只读**容量条 |

桌面端原来的「成员」「空间」两个大面板占了渲染层近 500 行，且 `applyRole` 只隐藏管理员专属入口、对所有角色都展示了治理面板——这正是「混乱」的根源。剥离后 `applyRole` 不再需要，`guard()` 里误嵌入的重复 `bootstrap()`（每次点击按钮都会重跑会话恢复）也一并修掉。

### 34.2 服务端：一个二进制同时跑 gRPC 与 HTTP

| 位置 | 改动 |
| --- | --- |
| `crates/vcs-grpc/src/admin.rs`（新） | axum 0.7 路由 + 约 40 个 JSON 接口；`include_str!` 把 `admin_assets/{index.html,app.js,styles.css}` 编进二进制，无需静态文件服务器 |
| `crates/vcs-grpc/src/lib.rs` | `ServeConfig.admin_listen: Option<SocketAddr>`；`serve()` 用 `tokio::select!` 同时驱动 gRPC 与 HTTP，任一退出即结束进程 |
| `crates/vcs-cli/src/main.rs` | `--admin-listen`（默认 `127.0.0.1:8080` / `VCS_ADMIN_LISTEN`）与 `--no-admin`（`VCS_NO_ADMIN`） |
| `crates/vcs-server/src/admin.rs`（新） | `AdminOverview` / `AdminUser` 与 `admin_overview` / `admin_list_users` / `admin_list_repos` / `admin_set_user_status` / `admin_set_user_role`；补上了原先 gRPC `SetUserStatus`/`SetUserRole` 未落地的业务方法 |

关键决策：

- **先绑定 HTTP 再拉起 gRPC**。管理端口被占用时立刻失败，避免「gRPC 就绪但管理页面没起来」的半残状态；冒烟脚本用 `--no-admin` 或随机端口，不抢固定 8080。
- **不引入新的认证体系**。管理端登录复用 `ServerStore::login` / `login_ldap` 签发的同一份 JWT，请求头与 gRPC 一样是 `Authorization: Bearer`；每个 `/api/*` 都校验 token 且要求 `role == Admin`，非管理员登录直接拒绝。
- **仓库级治理用 `Session::system()`**。成员门的设计是「全局 admin 也不能绕过」，但管理后台必须能治理任意仓库。做法是：中间件先完成管理员鉴权，再克隆会话、把 `kind` 标成 `System`（user_id 保留，审计仍是真人）。`ServerStore` 的成员管理三处与 `list_members` 增加 system 例外；桌面端 / gRPC 客户端拿不到 system 身份，成员门不变。
- **状态 / 角色变更递增 `token_version`**，停用或降级的账号已签发 JWT 立即失效；禁止停用自己、禁止动最后一个可用管理员。
- 存储调用一律走 `spawn_blocking` 并在闭包内取锁，避免阻塞 tokio worker；注意 `std::sync::Mutex` 不可重入，不在 `match` 表达式里直接持有 guard。

### 34.3 桌面端：菜单式布局

顶部连接条 + 左侧垂直菜单（**工作副本 / 仓库 / 账号**）+ 右侧单一内容视图，菜单切换只切 class、不发请求；未登录默认「账号」，登录 / 恢复会话后切「工作副本」，断开回「账号」。管理面板、配额弹窗、LDAP 表单、审批台全部删除；`main/index.ts` 与 `preload/index.ts` 的通道从 52 个精简到 23 个（同时清掉 preload 里重复的成员接口声明），`client.ts` 的方法全部保留——冒烟脚本直连它，不经过 IPC。

`smoke-ui.cjs` 的 `domIds > 50` 阈值随页面瘦身调整为 `> 25`；`.brand` / `#target` / bootstrap 回填等断言保持原样。

### 34.4 验收

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` | 通过 |
| `cargo test --workspace` | 通过（含 `system_session_cannot_be_authenticated` 等既有权限用例） |
| `npm run typecheck` / `npm run build` / `npm run smoke:ui` | 通过（UI 冒烟 `pass:true`，`apiKeys=23`） |
| `npm run smoke` | 通过（提交 / 冲突 / 审批 / 成员 / 空间治理 / 配额全链路；脚本带 `--no-admin`） |
| `npm run smoke:admin`（新，已接入 CI） | 通过：静态页、未登录拦截、错口令、管理员登录、全库视图、直通建库、任意仓库成员、配额覆盖、建号 / 停用、无效 token |

### 34.5 遗留

- 管理端是**明文 HTTP**，默认只绑本机；跨机必须 `--admin-listen 0.0.0.0:8080` 并前置 TLS 网关。
- 登录令牌存浏览器 `localStorage`（请求走 `Authorization` 头而非 Cookie，因此没有 CSRF 面），但没有服务端会话撤销；停用账号靠 `token_version` 立即失效。
- 用户列表 / 审批列表是单次上限查询（500 / 200 条），没有分页与搜索。
- 管理端没有真实浏览器自动化用例，当前用 HTTP 层冒烟覆盖；`vcs serve` 的 `--admin-listen :0` 场景没有对外暴露实际绑定端口。
