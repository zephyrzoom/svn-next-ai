# svn-next

参考 SVN 核心语义、用现代技术栈重新实现的集中式版本控制系统：**服务端 Rust**（gRPC + SQLite + 共享 CAS 对象池），**客户端 Electron**（工作副本逻辑经 napi 复用同一份 Rust `vcs-core`）。当前版本 `0.1.0`，定位为内网试用。

> 与 SVN 网络协议不兼容，不提供分支 / 标签：仓库只有一条线性主干，版本用全局 `rN` 定位。完整设计与验收记录见 [`docs/design.md`](docs/design.md)。

## 架构速览

| 组件 | 职责 | 主要产物 |
| --- | --- | --- |
| `vcs-core` | 哈希、树对象、CAS、工作副本、认证（唯一的版本语义实现） | Rust 库 |
| `vcs-server` | 业务层 + 存储运维工具 | `vcs-server` |
| `vcs-grpc` | 长期运行的 gRPC 服务，桌面端连它 | `vcs-grpc` |
| `vcs-cli` | 本地直连存储目录的命令行客户端 | `vcs` |
| `vcs-napi` / `desktop` | Node 原生模块 / Electron 界面 | `vcs-napi.node`、安装包 |
| `proto/repo.proto` | 唯一协议真源（Rust 与 TS 都从这里生成 / 加载） | — |

关键点：**CLI 不走网络**，直接用 `--store` 打开服务端存储目录；**桌面端走 gRPC**。两者操作的是同一份数据，只是入口不同。

---

## 1. 环境要求

| 依赖 | 版本 / 说明 |
| --- | --- |
| Rust + Cargo | ≥ 1.85（workspace `rust-version`） |
| Node.js + npm | 22（CI 使用 22） |
| protoc | **无需预装**，`vcs-proto` 用 `protoc-bin-vendored` 自带编译器 |
| Windows | MSVC 工具链；链接原生模块需要 `node.lib`（见 CI 的处理方式） |
| Linux 打包 AppImage | 需 `libfuse2` / `libfuse2t64` |

SQLite 为 bundled 编译，不需要系统额外安装数据库。

---

## 2. 构建

### 2.1 服务端与 CLI

```bash
cargo build --workspace --release
# 产物：target/release/vcs  target/release/vcs-grpc  target/release/vcs-server
```

开发期用 `cargo build --workspace` 即可，产物在 `target/debug/`。把这三个二进制复制到目标机器的同一目录（例如 `/opt/svn-next/`）就能部署，不依赖源码。

### 2.2 桌面客户端

```bash
cd desktop
npm ci
npm run start          # = build:native + tsc 编译 + 启动 Electron
```

`npm run build:native` 会执行 `cargo build -p vcs-napi`，把 cdylib 复制成 `desktop/native/vcs-napi.node`；`npm run build` 把 TS 编译到 `desktop/dist/`。这两步的产物都不入库（已在 `.gitignore` 中）。

打包安装包：

```bash
npm run pack           # 只产出解压目录，便于核对包内文件
npm run dist           # 出安装包，输出到 desktop/release/
```

`stage` 步骤会把根目录的 `proto/repo.proto` 复制进 `desktop/proto/` 一起打包；原生模块通过 `asarUnpack` 解包，因为 `.node` 无法从 asar 内 `dlopen`。若沙箱环境无法写 asar，可用 `npm run pack:unpacked` / `dist:unpacked` 关闭 asar，功能一致。

三平台产物也可由 CI 生成：push `v*` tag 会触发 `.github/workflows/release.yml`，构建 macOS `dmg`/`zip`、Windows `nsis`/`zip`、Linux `AppImage` 并创建 Release；手动触发 `workflow_dispatch` 只上传 artifact、不发 Release。

---

## 3. 部署服务端

### 3.1 初始化存储目录与管理员

```bash
sudo mkdir -p /srv/vcs
sudo chown -R vcs:vcs /srv/vcs        # 按实际运行账号调整

vcs init-admin --store /srv/vcs \
  --username alice --name "张爱丽" --password 'Abcdef1!'

vcs-server --root /srv/vcs info       # 核对：users 应显示「已初始化」
```

- `init-admin` **只能在空库执行一次**，之后所有账号走注册审批或 `vcs user-create`。
- 密码参数留空则交互输入、不回显。
- `schema/server.sql` 由程序自动执行，**不需要手工建表**；首次打开目录会自动建好：

```
/srv/vcs/server.db          SQLite 元数据（WAL，另有 -wal / -shm）
/srv/vcs/server.secret      JWT 签名密钥，首次生成，权限 0600
/srv/vcs/objects/           全仓库共享的 CAS 对象池（blake3 + zstd）
/srv/vcs/tmp/               原子写入暂存区，启动时清理残留
```

### 3.2 启动 gRPC 服务

```bash
vcs-grpc --root /srv/vcs --listen 127.0.0.1:50051
# 看到「vcs-grpc 已启动：127.0.0.1:50051」即就绪
```

| 参数 | 环境变量 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--root` | `VCS_STORE` | `.vcs-server` | 存储根目录，必须与 CLI / `vcs-server` 指向同一个 |
| `--listen` | `VCS_LISTEN` | `127.0.0.1:50051` | 默认只绑本机；跨机器需显式 `0.0.0.0:50051` |
| `--maintenance-interval` | `VCS_MAINTENANCE_INTERVAL` | `300` | 后台维护间隔（秒）：到期清理转正 + 全库 GC；`0` 关闭，可手工执行 `vcs maintenance --store /srv/vcs` |

Linux 用 systemd 常驻的示例：

```ini
[Unit]
Description=svn-next gRPC server
After=network.target

[Service]
Type=simple
User=vcs
ExecStart=/opt/svn-next/vcs-grpc --root /srv/vcs --listen 0.0.0.0:50051
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Windows 直接运行 `vcs-grpc.exe --root D:\vcs-data --listen 0.0.0.0:50051` 即可。

### 3.3 网络与安全（部署前务必确认）

- 客户端连接是**明文 HTTP/2**（`createInsecure`），服务端**没有内置 TLS**。默认只绑 `127.0.0.1`；要对外提供服务，请加 TLS 网关 / 反向代理，或用内网 + 防火墙限制来源，不要把 50051 直接暴露到公网。
- `server.secret` 是 JWT 签名密钥，**泄露等于可伪造任意身份**。保持 0600，不要放进版本库或镜像公共层。
- 备份时把整个存储根目录一起备份（`server.db` + `-wal`/`-shm` + `server.secret` + `objects/`）；建议先停服务或做一致性快照，并定期验证恢复。
- 元数据是单机 SQLite，**不支持多机 / 高可用部署**；CLI 与 gRPC 服务同时写同一目录会形成跨进程并发，生产环境建议只让 gRPC 服务作为写入入口。
- 数据目录 `.vcs-server/`、工作副本 `.vcs/` 已在 `.gitignore` 中，不要提交到仓库。

---

## 4. 使用桌面客户端

### 4.1 首次运行

1. 安装后打开应用。产物**未做代码签名**：
   - macOS：首次「右键 → 打开」，或 `xattr -dr com.apple.quarantine /Applications/svn-next.app`；
   - Windows：SmartScreen 提示「未知发布者」时选择「仍要运行」；
   - Linux：`chmod +x *.AppImage` 后运行。
2. 顶栏默认填的是 `127.0.0.1:50051`，改成实际服务端地址（`host:port`，不带 `http://`），用 `init-admin` 创建的账号登录。
3. 连接信息与用户名明文保存在 `userData/settings.json`，token 用系统钥匙串 / DPAPI / libsecret（`safeStorage`）加密保存；再次启动会自动恢复会话。点「断开连接」会清除凭证但保留地址。

### 4.2 开通普通账号（审批链路）

管理员不必为每个人手工建号，界面已接好完整闭环：

1. 用户点「申请注册」，填用户名 / 姓名 / 申请理由（**不收集密码**）。
2. 管理员在「审批台 → 注册申请」通过，系统返回一个一次性 token（`vcs_setup_…`）。
3. 用户点「用一次性链接设置密码」，用该 token 设置初始密码（服务端会校验强度，token 只能用一次）。
4. 登录后若需要自己的仓库，点「提交建库申请」；管理员在「审批台 → 建库申请」批准，申请人自动成为该仓库 owner。管理员也可直接「直通新建」。

等价的 CLI 操作（管理员在服务器本机执行）：

```bash
export VCS_STORE=/srv/vcs
vcs register-list --state pending
vcs register-approve <request_id> --role writer      # 输出 vcs_setup_… token
vcs set-password --token vcs_setup_... --password 'Bobpass1!'
vcs repo-request-list --state pending
vcs repo-approve <request_id> --hard 1073741824 --note "同意"
```

### 4.3 日常操作

| 场景 | 界面操作 |
| --- | --- |
| 检出 | 「工作副本」选目录 → 「检出」，目录下生成 `.vcs/`（`wc.db` + pristine 基线） |
| 提交 | 改文件 → 「重新扫描」看变更 → 差异面板 → 填提交信息 → 「提交」 |
| 更新 | 「更新」拉取服务端新版本。**本地版本落后时提交会被拒**，必须先更新 |
| 冲突 | 三栏查看 base / mine / theirs，选择「保留我的」「采用服务端」或手工合并后保存；带冲突无法提交 |
| 还原 | 「全部还原」丢弃本地改动并清冲突（未受控文件不会被删） |
| 成员 / 配额 | 「成员」增删成员与调整角色（owner 受保护）；「配额…」设全局默认或单仓库覆盖、恢复继承 |
| 空间治理 | 「空间」面板：预估可释放 → 清理所选 → 回收站撤销（内容已被 GC 回收则明确失败）；保留策略先预览、再切为可执行 |

---

## 5. 使用 CLI（本地直连）

CLI 必须与服务在同一台机器、能访问同一文件系统；`--store` 也可用环境变量 `VCS_STORE` 提供。登录凭证缓存在 `~/.vcs/credentials.json`（0600），`vcs logout` 清除。

一个最小闭环：

```bash
export VCS_STORE=/srv/vcs
vcs login --username alice --password 'Abcdef1!'
vcs repo-create reports

mkdir -p /tmp/src/202610 && echo "月度汇总" > /tmp/src/202610/summary.txt
vcs import reports /tmp/src -m "初始导入"       # 从本地目录导入首个版本
vcs checkout reports /tmp/wc

cd /tmp/wc
vcs status                                      # 离线，不访问服务端
echo "月度汇总（修订）" > 202610/summary.txt
vcs diff
vcs commit -m "修订"                            # 落后于服务端会被拒，先 vcs update
vcs update
vcs log reports
vcs info reports                                # 用量与生效配额
```

### 命令速查

| 分组 | 命令 |
| --- | --- |
| 账号 / 会话 | `init-admin`、`login`（`--password` / `--pat`）、`logout`、`user-create`、`register*`、`set-password` |
| 仓库 | `repo-create`、`repo-request*`、`repo-approve` / `repo-reject`、`repo-list`、`repo-quota`、`repo-status` |
| 成员 | `member-add` / `member-list` / `member-remove` / `member-set-role` |
| 读写 | `import`、`checkout`（`-r` 指定历史版本）、`status`、`diff`、`commit`、`update`、`revert`、`log`、`cat` |
| 容量治理 | `info`、`candidates`、`estimate`、`purge`（`--immediate`）、`gc` / `gc-status`、`purged` / `unpurge`、`retention-*`、`maintenance` |
| 令牌 | `pat-create` / `pat-list` / `pat-revoke`；CI 用 `vcs login --pat <token>` |

完整命令与对应 RPC 见 [`docs/design.md` §11.3](docs/design.md)，或 `vcs --help` / `vcs <子命令> --help`。

---

## 6. 验证与测试

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

cargo build -p vcs-cli -p vcs-grpc    # 冒烟脚本要用到这两个 debug 二进制，在仓库根目录执行

cd desktop
npm run typecheck
npm run smoke                          # 起真实 gRPC 服务，跑提交/更新/冲突/审批/治理全链路
```

`npm run smoke` 不需要 Electron 界面，它用临时目录起真实服务端、走真实 HTTP/2 与文件系统，是部署前最值得跑一遍的检查。`.github/workflows/ci.yml` 会在三平台执行 Rust 检查、proto / SQL 契约校验、CLI 冒烟与桌面端冒烟。

---

## 7. 常见问题

| 现象 | 处理 |
| --- | --- |
| 桌面端连不上 | 确认 `vcs-grpc` 已启动并打印监听地址；地址格式是 `host:port`；跨机器时服务端需 `--listen 0.0.0.0:50051` 且防火墙放行 |
| 提示「尚未连接服务端」 | 先点「连接」登录；凭证过期会自动回登录框，重新登录即可 |
| 启动报找不到 `vcs-napi.node` | 执行 `npm run build:native`（需要本机有 Cargo），确认 `desktop/native/` 下有产物 |
| CLI 报未登录 / 权限不足 | `vcs login` 重新登录；注意 `--store` 与登录时用的是同一个目录（凭证按目录绝对路径缓存） |
| `init-admin` 提示已初始化 | 该命令只允许执行一次；后续账号走注册审批或 `vcs user-create` |
| macOS 提示「已损坏，无法打开」 | 未签名包被 Gatekeeper 拦截，执行 `xattr -dr com.apple.quarantine <app>` |
| Linux AppImage 打包失败 | 安装 `libfuse2t64`（或 `libfuse2`） |
| Windows 原生模块链接失败 | MSVC 需要 `node.lib`，参考 release 工作流里通过 `node-gyp install` + `RUSTFLAGS=-C link-arg=…` 的处理 |

---

## 8. 已知限制

- 安装包**未签名、无自动更新**，仅用于内部试用；macOS CI 产物目前是 arm64。
- 服务端**无内置 TLS**，明文 gRPC；公网部署必须前置 TLS 网关。
- 单机 SQLite，无多机 / 高可用；大文件分块续传、LDAP 登录、路径级 ACL、锁、稀疏检出尚未实现。
- 不兼容 SVN 协议，无历史仓库导入能力；`import` 只是内容初始化。

设计与决策细节、里程碑与验收记录见 [`docs/design.md`](docs/design.md)。
