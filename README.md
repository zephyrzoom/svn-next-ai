# svn-next

参考 SVN 核心语义、用现代技术栈重新实现的集中式版本控制系统：**服务端 Rust**（gRPC + SQLite + 共享 CAS 对象池），**客户端 Electron**（工作副本逻辑经 napi 复用同一份 Rust `vcs-core`）。当前版本 `0.1.0`，定位为内网试用。

> 与 SVN 网络协议不兼容，不提供分支 / 标签：仓库只有一条线性主干，版本用全局 `rN` 定位。完整设计与验收记录见 [`docs/design.md`](docs/design.md)。

## 架构速览

| 组件 | 职责 | 主要产物 |
| --- | --- | --- |
| `vcs-core` | 哈希、树对象、CAS、工作副本、认证（唯一的版本语义实现） | Rust 库 |
| `vcs-server` | 业务层 `ServerStore`，被其它 crate 链接（无独立进程） | Rust 库 |
| `vcs-grpc` | gRPC 服务 + 内置 Web 管理端（HTTP/JSON + 内嵌静态页面），由 `vcs serve` 调用（无独立进程） | Rust 库 |
| `vcs-cli` | **唯一二进制**：客户端命令 + 服务端子命令 | `vcs` |
| `vcs-napi` / `desktop` | Node 原生模块 / Electron 界面 | `vcs-napi.node`、安装包 |
| `proto/repo.proto` | 唯一协议真源（Rust 与 TS 都从这里生成 / 加载） | — |

关键点：**服务端只需要分发并启动一个 `vcs` 文件**，用子命令区分角色：

| 命令 | 角色 | 是否常驻 |
| --- | --- | --- |
| `vcs <客户端命令>` | 本地直连存储目录（init-admin / import / commit / …） | 否，执行完即退出 |
| `vcs serve` | gRPC 服务 + 内置 Web 管理端，桌面端连 gRPC，管理员用浏览器连管理端 | **是** |
| `vcs server info` | 存储目录统计（布局 / 对象数 / 是否初始化） | 否 |

CLI 不走网络，直接用 `--store` 打开服务端存储目录；桌面端走 gRPC，只做仓库使用；管理员通过 `vcs serve` 内置的 Web 管理端（默认 `http://127.0.0.1:8080`）做审批、用户、成员、配额、目录与空间治理。三者操作的是同一份数据，只是入口不同。管理端的静态页面编进二进制，部署时依然只有 `vcs` 一个文件。

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
# 产物：target/release/vcs   ← 唯一可执行文件
```

开发期用 `cargo build --workspace`（或只构建入口 `cargo build -p vcs-cli`）即可，产物在 `target/debug/`。把这**一个**二进制复制到目标机器（例如 `/opt/svn-next/vcs`）就能部署，不依赖源码；客户端命令、常驻服务、存储运维都在里面。

### 2.2 桌面客户端

```bash
cd desktop
npm ci
npm run start          # = build:native + tsc 编译 + 启动 Electron
```

`npm run build:native` 会执行 `cargo build -p vcs-napi`，把 cdylib 复制成 `desktop/native/vcs-napi.node`；`npm run build` 把 TS 编译到 `desktop/dist/`，并同步 `src/renderer/` 的 html/js/css 到 `dist/renderer/`、复制契约文件（主进程从 `dist/renderer/index.html` 加载界面，tsc 不会产出这些静态资源）。这些产物都不入库（已在 `.gitignore` 中）。

打包安装包：

```bash
npm run pack           # 只产出解压目录，便于核对包内文件
npm run dist           # 出安装包，输出到 desktop/release/
```

`stage` 步骤会把根目录的 `proto/repo.proto` 复制进 `desktop/proto/`、把渲染层静态资源复制进 `dist/renderer/` 一起打包（builder 的 files 规则包含整个 `dist`）；原生模块通过 `asarUnpack` 解包，因为 `.node` 无法从 asar 内 `dlopen`。若沙箱环境无法写 asar，可用 `npm run pack:unpacked` / `dist:unpacked` 关闭 asar，功能一致。

三平台产物也可由 CI 生成：push `v*` tag 会触发 `.github/workflows/release.yml`，构建 macOS `dmg`/`zip`、Windows `nsis`/`zip`、Linux `AppImage` 并创建 Release；手动触发 `workflow_dispatch` 只上传 artifact、不发 Release。

---

## 3. 部署服务端

### 3.1 初始化存储目录与管理员

**空库不必预先建管理员**：全库第一个提交注册申请的人，或第一个用 LDAP 登录成功的人，会自动成为管理员（`admin`），无需任何人审批。两条入口共用同一个名额，谁先在写事务里落到用户表谁拿走；之后所有人都走正常流程（注册待审批 / LDAP 按组映射初始角色）。

- **首位注册者**：申请单直接 `approved`，注册时自己填的密码立即可用，提交后就能连接，不需要再设置密码。
- **首位 LDAP 用户**：目录 bind 成功后即时创建 `ACTIVE` 映射并签发 JWT，无需设置本地密码。

如果希望在部署阶段就固定管理员，或首位账号没有可用口令需要救援，仍可在服务器本机执行：

```bash
sudo mkdir -p /srv/vcs
sudo chown -R vcs:vcs /srv/vcs        # 按实际运行账号调整

vcs init-admin --store /srv/vcs \
  --username alice --name "张爱丽" --password 'Abcdef1!'

vcs --store /srv/vcs server info      # 核对：users 应显示「已初始化」
```

- `init-admin` 只在**没有可登录管理员**时可用；已有管理员时执行会被拒绝。若同名本地账号存在但没有可用口令（例如旧流程遗留的未激活账号），它会重置口令并提权，作为救援通道。
- 密码参数留空则交互输入、不回显。
- `schema/server.sql` 由程序自动执行，**不需要手工建表**；首次打开目录会自动建好：

```
/srv/vcs/server.db          SQLite 元数据（WAL，另有 -wal / -shm）
/srv/vcs/server.secret      JWT 签名密钥，首次生成，权限 0600
/srv/vcs/objects/           全仓库共享的 CAS 对象池（blake3 + zstd）
/srv/vcs/tmp/               原子写入暂存区，启动时清理残留
```

### 3.2 启动服务（`vcs serve`）

```bash
vcs --store /srv/vcs serve --listen 127.0.0.1:50051
# 看到「vcs serve 已启动：127.0.0.1:50051」和
# 「Web 管理端已启动：http://127.0.0.1:8080」即就绪
```

`--store` 是全局参数，放在子命令前后都可以（`vcs serve --store /srv/vcs …` 等价）。所有子命令共用它，所以不会再出现「服务与 CLI 指向不同目录」的错配。

| 参数 | 环境变量 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `--store` | `VCS_STORE` | `.vcs-server` | 存储根目录（全局参数，所有子命令共用） |
| `--listen` | `VCS_LISTEN` | `127.0.0.1:50051` | gRPC 监听地址；跨机器需显式 `0.0.0.0:50051` |
| `--admin-listen` | `VCS_ADMIN_LISTEN` | `127.0.0.1:8080` | Web 管理端监听地址；跨机器需显式 `0.0.0.0:8080` |
| `--no-admin` | `VCS_NO_ADMIN` | 关闭 | 只跑 gRPC，不启动 Web 管理端（测试 / 最小化部署用） |
| `--maintenance-interval` | `VCS_MAINTENANCE_INTERVAL` | `300` | 后台维护间隔（秒）：到期清理转正 + 全库 GC；`0` 关闭，可手工执行 `vcs maintenance --store /srv/vcs` |

管理端在启动 gRPC **之前**先完成端口绑定：端口被占用会直接报错退出，不会出现「gRPC 已就绪、管理页面却没起来」的半残状态。

查看目录概况用 `vcs --store /srv/vcs server info`（一次性命令，不占端口）。

Linux 用 systemd 常驻的示例：

```ini
[Unit]
Description=svn-next gRPC server
After=network.target

[Service]
Type=simple
User=vcs
ExecStart=/opt/svn-next/vcs --store /srv/vcs serve --listen 0.0.0.0:50051
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Windows 直接运行 `vcs.exe --store D:\vcs-data serve --listen 0.0.0.0:50051` 即可。

### 3.3 网络与安全（部署前务必确认）

- 客户端连接是**明文 HTTP/2**（`createInsecure`），服务端**没有内置 TLS**。默认只绑 `127.0.0.1`；要对外提供服务，请加 TLS 网关 / 反向代理，或用内网 + 防火墙限制来源，不要把 50051 直接暴露到公网。
- Web 管理端默认是**明文 HTTP** 且只绑 `127.0.0.1:8080`。跨机访问要显式 `--admin-listen 0.0.0.0:8080` 并前置 TLS 网关；登录令牌保存在浏览器 `localStorage`，不要在公用浏览器上登录后不清理。
- `server.secret` 是 JWT 签名密钥，**泄露等于可伪造任意身份**。保持 0600，不要放进版本库或镜像公共层。
- 备份时把整个存储根目录一起备份（`server.db` + `-wal`/`-shm` + `server.secret` + `objects/`）；建议先停服务或做一致性快照，并定期验证恢复。
- 元数据是单机 SQLite，**不支持多机 / 高可用部署**；CLI 与 gRPC 服务同时写同一目录会形成跨进程并发，生产环境建议只让 gRPC 服务作为写入入口。
- 数据目录 `.vcs-server/`、工作副本 `.vcs/` 已在 `.gitignore` 中，不要提交到仓库。

### 3.4 接入 LDAP / AD（可选）

在服务器本机配置（无需先有管理员），或由管理员在 Web 管理端「目录设置」里配置：

```bash
export VCS_STORE=/srv/vcs
vcs ldap-config \
  --host ldap.example.com --port 636 --tls-mode ldaps \
  --base-dn 'dc=example,dc=com' --bind-dn 'cn=svc,dc=example,dc=com' \
  --bind-password 'SvcPass1!' --user-filter '(uid={username})' \
  --group-role 'cn=admins,dc=example,dc=com=admin' \
  --enable
vcs ldap-test     # 服务账号 bind + 抽样搜索
vcs ldap-show     # 回显配置，不显示密码
```

- 只支持 `ldaps` 或 `starttls`，证书一律校验，**没有明文 bind 选项**；`{username}` 拼进 filter 前按 RFC 4515 转义。
- bind 密码用 `server.secret` 加密落库，读取接口不回传；再次配置时 `--bind-password` 留空表示沿用。
- 桌面端在登录页把身份来源切到「LDAP 目录」即可用目录口令登录；CLI 用 `vcs login --ldap --username alice`。
- 目录用户首次登录免审批（JIT 建号）；组 → 角色映射只决定**新建账号的初始角色**，已有账号角色以本地为准，避免目录侧调整把管理员降级。
- 目录不可用时只影响 LDAP 登录（`ERROR_CODE_LDAP_UNAVAILABLE`），本地账号始终能登录，绝不退化成「校验通过」。

### 3.5 Web 管理端（管理员入口）

浏览器打开 `http://127.0.0.1:8080`（跨机时换成服务器地址），用**管理员账号**登录；身份来源可选「本地账号」或「LDAP 目录」，复用的就是 `ServerStore::login` / `login_ldap` 签发的同一份 JWT。非管理员登录会被拒绝，所有 `/api/*` 接口都要求 `Authorization: Bearer <token>` 且角色为 `admin`。

左侧菜单覆盖全部管理职能，桌面端不再提供这些入口：

| 菜单 | 能做什么 |
| --- | --- |
| 概览 | 账号 / 仓库 / 待审批 / 对象池 / 回收站统计 |
| 审批台 | 注册申请通过（申请人注册时已设密码，审批只放行、不发放任何凭证）与拒绝、建库申请通过（可填一个配额上限，0=继承全局默认）与拒绝 |
| 用户 | 全库账号列表、直接建号、停用 / 启用、调整全局角色 |
| 仓库 | 全库仓库列表、直通新建、启停 / 归档、成员增删与角色、单仓库配额覆盖、状态管理 |
| 配额 | 全局默认配额（新仓库继承值） |
| 目录设置 | LDAP / AD 配置保存与连通性测试（bind 密码不回传） |
| 空间治理 | 清理候选 / 预估 / 清理 / 立即 GC、回收站撤销、保留策略与试运行 |

管理端与 gRPC 共用同一个 `Arc<Mutex<ServerStore>>`，不复制数据、不额外进程。仓库级操作以服务端内部的 `Session::system()` 执行（管理员已先通过鉴权，审计记录仍是本人 user_id），因此管理员不必是每个仓库的成员也能治理；桌面端 / gRPC 客户端依旧受成员门约束。

只想跑 gRPC 时加 `--no-admin`；`--admin-listen` 可换端口。管理端端口随 `vcs serve` 一起启停，无需单独部署前端或反向代理静态文件。

---

## 4. 使用桌面客户端

桌面端**打开先进入登录页**（登录 / 申请注册）；登录成功后才进入主界面：顶部信息条 + 左侧菜单（工作副本 / 仓库 / 账号）+ 右侧单一内容视图，切换菜单不会触发网络请求。审批、成员、配额、目录配置、空间治理等管理职能一律在 [§3.5 Web 管理端](#35-web-管理端管理员入口) 完成。

### 4.1 登录与 30 天免登录

1. 安装后打开应用。产物**未做代码签名**：
   - macOS：首次「右键 → 打开」，或 `xattr -dr com.apple.quarantine /Applications/svn-next.app`；
   - Windows：SmartScreen 提示「未知发布者」时选择「仍要运行」；
   - Linux：`chmod +x *.AppImage` 后运行。
2. 登录页默认填的是 `127.0.0.1:50051`，改成实际服务端地址（`host:port`，不带 `http://`），选身份来源「本地账号」或「LDAP 目录」，填用户名与密码后点「登录」。空库时第一个注册 / 第一个 LDAP 登录的人自动成为管理员；也可以先用 `init-admin` 建好管理员再登录。管理员登录后界面**不会**出现审批台等管理入口，请改用 Web 管理端。
3. **「30 天内免登录」默认勾选**：服务端为这次登录签发 30 天有效的 JWT，凭证由系统钥匙串 / DPAPI / libsecret（`safeStorage`）加密保存在 `userData/settings.json`，重启客户端会自动恢复会话并直接进入主界面。取消勾选则只在本次会话有效、关闭应用后需重新登录。到期、账号被禁用 / 改角色 / 重置口令后会自动回到登录页。
4. 当前系统不支持加密存储时勾选框会置灰，界面提示「重启后需要重新登录」。点「退出登录」会清除已保存的凭证，但保留服务端地址与用户名。

### 4.2 注册账号（审批链路）

管理员不必为每个人手工建号，登录页（用户侧）与 Web 管理端（管理员侧）已接好完整闭环：

> 例外：**空库的第一个注册用户**不走这套流程——在登录页点「申请注册」提交后自动批准为管理员，注册时填的密码直接可用。下面是之后所有普通账号的流程。

1. 用户在**登录页**点「还没有账号？申请注册」，填用户名 / 姓名 / **登录密码** / 申请理由（密码策略由服务端复核，未通过审批前不能登录）。
2. 管理员在 **Web 管理端「审批台 → 注册申请」**通过（可选角色），审批只做放行，不再发放任何一次性链接。
3. 用户回到登录页，用**注册时设置的密码**登录，成功后进入主界面。
4. 登录后若需要自己的仓库，在「仓库」视图点「提交建库申请」；管理员在 **Web 管理端「审批台 → 建库申请」**批准，申请人自动成为该仓库 owner。管理员也可在「仓库」页「直通新建」。

等价的 CLI 操作（管理员在服务器本机执行）：

```bash
export VCS_STORE=/srv/vcs
vcs register --username bob --name "李波"            # 交互式输入登录密码，注册即生效
vcs register-list --state pending
vcs register-approve <request_id> --role writer
vcs repo-request-list --state pending
vcs repo-approve <request_id> --quota 1073741824 --note "同意"
```

### 4.3 日常操作

| 场景 | 界面操作 |
| --- | --- |
| 检出 | 「工作副本」选目录 → 「检出」，目录下生成 `.vcs/`（`wc.db` + pristine 基线） |
| 提交 | 改文件 → 「重新扫描」看变更 → 差异面板 → 填提交信息 → 「提交」 |
| 更新 | 「更新」拉取服务端新版本。**本地版本落后时提交会被拒**，必须先更新 |
| 冲突 | 三栏查看 base / mine / theirs，选择「保留我的」「采用服务端」或手工合并后保存；带冲突无法提交 |
| 还原 | 「全部还原」丢弃本地改动并清冲突（未受控文件不会被删） |
| 只读容量 | 「仓库」视图显示当前仓库的已用 / 上限（只读，桌面端不能改配额） |
| 成员 / 配额 | Web 管理端「仓库」页：增删成员与调整角色（owner 受保护）、单仓库配额覆盖与恢复继承；「配额」页设全局默认 |
| 空间治理 | Web 管理端「空间治理」页：预估可释放 → 清理所选 → 回收站撤销（内容已被 GC 回收则明确失败）；保留策略先预览、再切为可执行 |

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
| 账号 / 会话 | `init-admin`、`login`（`--password` / `--pat` / `--ldap`）、`logout`、`user-create`、`register*`、`ldap-config` / `ldap-show` / `ldap-test` |
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

cargo build -p vcs-cli                # 冒烟脚本只用到 target/debug/vcs 这一个二进制

cd desktop
npm run typecheck                      # 只证明 .ts 类型正确
npm run build                          # 编译 + 同步静态资源 + 启动契约自检
npm run check:assets                   # 单独复查：入口 / 页面 / html 引用 / 源码一致性
npm run smoke                          # 起真实 gRPC 服务，跑提交/更新/冲突/审批/治理全链路
npm run smoke:admin                    # 起真实服务，用 HTTP 验证内置 Web 管理端
npm run smoke:ui                       # 可选：真启动 Electron，验证页面、CSS、preload 与 app.js
```

各层覆盖的边界要清楚，别把「业务逻辑跑通」当成「窗口能显示」：

| 检查 | 需要 Electron | 能抓到 |
| --- | --- | --- |
| `typecheck` | 否 | TS 类型错误；碰不到 html/js/css 与产物装配 |
| `check:assets`（`build` 末尾自动跑） | 否 | 页面 / preload 漏拷、主进程路径漂移、html 引用失效、源码与产物不一致 |
| `smoke` | 否 | 主进程业务逻辑在真实网络与文件系统下的行为；不创建 BrowserWindow（脚本带 `--no-admin`） |
| `smoke:admin` | 否 | 内置 Web 管理端的静态页面、登录鉴权与全部管理类 HTTP 接口；端口随机分配 |
| `smoke:ui` | **是** | CSP 拦截、preload 桥接失败、渲染脚本启动即抛错等运行时故障 |

`npm run smoke` 不需要 Electron 界面，它用临时目录起真实服务端、走真实 HTTP/2 与文件系统。`npm run smoke:admin` 同样无需浏览器：它会给管理端分配随机空闲端口，验证静态页面、未登录拦截、管理员登录、全库视图、直通建库、成员 / 配额 / 用户治理接口。CI 的 `desktop` job 刻意设置 `ELECTRON_SKIP_BINARY_DOWNLOAD=1`，所以 **`check:assets` 是 CI 里唯一拦住「构建成功但白屏」的防线**；`smoke:ui` 需要本机有 Electron 二进制与图形环境（Linux 无 DISPLAY 时用 `xvfb-run npm run smoke:ui`），目前只在本地跑。`.github/workflows/ci.yml` 还会在三平台执行 Rust 检查、proto / SQL 契约校验、CLI 冒烟与管理端冒烟。

---

## 7. 常见问题

| 现象 | 处理 |
| --- | --- |
| 桌面端连不上 | 确认 `vcs serve` 已启动并打印「vcs serve 已启动」；地址格式是 `host:port`；跨机器时服务端需 `--listen 0.0.0.0:50051` 且防火墙放行 |
| 浏览器打不开管理端 | 确认启动日志里有「Web 管理端已启动」；默认地址 `http://127.0.0.1:8080` **只能在服务器本机**打开。跨机访问需 `--admin-listen 0.0.0.0:8080` + 防火墙放行 + TLS 网关 |
| 管理端提示「仅管理员可登录」 | 该入口只接受全局 `admin`。空库第一个注册 / LDAP 登录用户自动成为管理员；也可在服务器本机用 `vcs init-admin` 预置 |
| 8080 端口被占用 | 启动会直接报错退出（不会半残启动）；换端口用 `--admin-listen 127.0.0.1:8081`，或加 `--no-admin` 只跑 gRPC |
| 提示「尚未连接服务端」 | 先在登录页登录；凭证过期会自动退回登录页，重新登录即可 |
| 每次启动都要重新登录 | 确认登录时勾了「30 天内免登录」，且系统支持加密存储（钥匙串 / DPAPI / libsecret）；勾选框置灰时本机无法加密保存凭证。主动点过「退出登录」或 30 天到期也会要求重新登录 |
| 启动报找不到 `vcs-napi.node` | 执行 `npm run build:native`（需要本机有 Cargo），确认 `desktop/native/` 下有产物 |
| 桌面端窗口白屏 | 先 `npm run check:assets` 定位（页面 / preload / html 引用 / 源码一致性）；通常是没跑过 `npm run build` 或 `dist/` 被清过，重新执行 `npm run start` 即可。加载失败现在也会弹错误框说明原因 |
| CLI 报未登录 / 权限不足 | `vcs login` 重新登录；注意 `--store` 与登录时用的是同一个目录（凭证按目录绝对路径缓存） |
| `init-admin` 提示已存在管理员 | 它只在没有可登录管理员时可用；普通账号请走注册审批或 `vcs user-create`。若首位账号没有可用口令（旧流程遗留），用**同名** `init-admin` 在服务器本机重置口令即可救援 |
| macOS 提示「已损坏，无法打开」 | 未签名包被 Gatekeeper 拦截，执行 `xattr -dr com.apple.quarantine <app>` |
| Linux AppImage 打包失败 | 安装 `libfuse2t64`（或 `libfuse2`） |
| Windows 原生模块链接失败 | MSVC 需要 `node.lib`，参考 release 工作流里通过 `node-gyp install` + `RUSTFLAGS=-C link-arg=…` 的处理 |

---

## 8. 已知限制

- 安装包**未签名、无自动更新**，仅用于内部试用；macOS CI 产物目前是 arm64。
- 服务端**无内置 TLS**，明文 gRPC 与明文 HTTP 管理端；公网部署必须前置 TLS 网关。管理端登录令牌存在浏览器 `localStorage`，无服务端会话撤销能力（停用账号会递增 `token_version` 使令牌立即失效）。桌面端「30 天免登录」同样只靠 `token_version` 撤销，**没有 refresh / 服务端会话表**：本机凭证泄露后需管理员停用账号或改角色才能立刻下线。
- 单机 SQLite，无多机 / 高可用；大文件分块续传、路径级 ACL、锁、稀疏检出尚未实现。LDAP 已接入（LDAPS / StartTLS），但目录连接池、嵌套组解析与真机目录集成测试仍待补。
- 不兼容 SVN 协议，无历史仓库导入能力；`import` 只是内容初始化。

设计与决策细节、里程碑与验收记录见 [`docs/design.md`](docs/design.md)。
