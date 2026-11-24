import { app, BrowserWindow, dialog, ipcMain } from 'electron';
import * as fs from 'node:fs';
import * as path from 'node:path';

import { VcsError, VcsClient, type UserInfo } from './client';
import {
  checkoutRepo,
  commitWorkingCopy,
  revertWorkingCopy,
  setSparseRules,
  updateWorkingCopy,
} from './flow';
import {
  bumpRepoUsage,
  clearToken,
  loadPrefs,
  loadRepoDirs,
  loadRepoUsage,
  loadToken,
  saveLogin,
  saveRepoDirs,
  saveTarget,
  type Account,
} from './settings';
import * as wc from './wc';
import type { ConflictChoice } from './wc';

/**
 * Electron 主进程：**唯一**发起网络与文件操作的地方，渲染进程只通过 preload 暴露的
 * 类型化 API 调用。渲染进程开着 sandbox + contextIsolation，即使页面被注入脚本，
 * 也拿不到文件系统与 token。
 *
 * 桌面端定位：只做「仓库使用」的客户端 —— 连接、仓库列表/选择、工作副本的
 * 检出/状态/差异/冲突/提交/更新/还原/历史、账号自助注册（注册即设密码）、提交
 * 建库申请，以及**仓库拥有者**对自己的仓库做成员增删改（成员页只对 owner 显示）。
 * 其余治理与审批（用户/注册审批、仓库审批、配额配置、空间治理、目录配置）统一由
 * 服务端 vcs serve 内置的 Web 管理端承载，桌面端不暴露这些通道。
 */

/** 统一返回信封：业务错误带 proto 的 ErrorCode，渲染进程据此决定提示还是跳登录。 */
type Envelope<T> =
  | { ok: true; data: T }
  | { ok: false; code?: number; message: string };

async function wrap<T>(run: () => Promise<T>): Promise<Envelope<T>> {
  try {
    return { ok: true, data: await run() };
  } catch (err) {
    if (err instanceof VcsError) {
      return { ok: false, code: err.code, message: err.message };
    }
    const message = err instanceof Error ? err.message : String(err);
    return { ok: false, message };
  }
}

let client: VcsClient | null = null;

/**
 * 当前登录身份（服务端地址 + 服务端确认过的用户名）。
 *
 * 只由主进程维护：渲染进程传什么都不会改变它。工作副本登记按它隔离——界面上
 * 传来的目录列表只写进这个账号名下，读也只读这个账号的，A / B 各看各的。
 */
let session: Account | null = null;

function requireClient(): VcsClient {
  if (!client) {
    throw new Error('尚未连接服务端');
  }
  return client;
}

function createWindow(): void {
  const win = new BrowserWindow({
    width: 1180,
    height: 780,
    // 下限宽度按内容反推：左侧仓库栏固定 216px，内容区左右 padding 44px、卡片 padding 32px，
    // 最吃宽度的是冲突编辑器的三列并排（每列 minmax(0,1fr)）——960 时每列还有约 228px 可编辑；
    // 再窄下去三列会被压成一条、变更路径和成员行的角色下拉也会互相挤。
    // 高度下限保证「tab + 变更列表 + 提交区」能同时露出，更长的差异交给内容区滚动。
    minWidth: 960,
    minHeight: 640,
    title: 'svn-next',
    webPreferences: {
      preload: path.join(__dirname, '../preload/index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  // 静态资源由 `npm run build` 同步到 dist/renderer/。万一缺失，别只留一个白窗口：
  // 把路径和原因直接弹出来，方便定位是没构建还是文件被清掉了。
  const page = path.join(__dirname, '../renderer/index.html');
  void win.loadFile(page).catch((err: unknown) => {
    const message = err instanceof Error ? err.message : String(err);
    console.error(`界面加载失败：${page} — ${message}`);
    void dialog.showErrorBox(
      '界面加载失败',
      `无法加载界面文件：\n${page}\n\n请先执行 npm run build（会把 src/renderer 同步到 dist/renderer）。\n\n${message}`,
    );
  });
}

const NOT_WIRED = '该功能在桌面端不可用，请使用服务端 Web 管理端';

/**
 * 桌面端登录一律按 30 天长期会话签发：登录页不再提供「免登录」开关，
 * CLI / Web 管理端仍走服务端的 12 小时默认有效期。
 */
const DESKTOP_REMEMBER_ME = true;

function notWired(): Promise<never> {
  return Promise.reject(new Error(NOT_WIRED));
}

// ---------------- 连接与仓库使用 ----------------

ipcMain.handle(
  'vcs:connect',
  (
    _event,
    target: string,
    username: string,
    password: string,
    source: 'LOCAL' | 'LDAP' = 'LOCAL',
  ) =>
    wrap<{ user: UserInfo; expiresAt: number }>(async () => {
      const next = new VcsClient(target);
      // 登录失败不能把当前已登录的 client 关掉：新连接自己收拾干净。
      let user: UserInfo;
      try {
        user = await next.login(username, password, source, DESKTOP_REMEMBER_ME);
      } catch (err) {
        next.close();
        throw err;
      }
      client?.close();
      client = next;
      // 身份以服务端返回的 user 为准（LDAP 的 JIT 建号会把用户名规范化）。
      session = { target, username: user.username };
      const expiresAt = next.expiresAtValue;
      // 登录响应带姓名就顺手缓存：旧服务端的 WhoAmI 不返回姓名，恢复会话时要用它兜底。
      // 没拿到（旧服务端）就传 undefined，保留上次的缓存，不要把姓名清掉。
      const displayName = user.displayName || undefined;
      if (next.tokenValue) {
        // 30 天免登录：凭证加密落盘，并记下到期时间供启动时快速判断。
        saveLogin(target, user.username, {
          token: next.tokenValue,
          expiresAt,
          displayName,
        });
      } else {
        // 理论上登录成功必有 token；真没拿到时只留地址、账号与姓名，不落可复用凭证。
        saveLogin(target, user.username, { displayName });
      }
      return { user, expiresAt };
    }),
);

/** 上次的连接信息（不含凭证），用于把登录页填好。 */
ipcMain.handle('vcs:loadPrefs', () => wrap(async () => loadPrefs()));

/** 登录页「设置」：保存自定义服务器地址（空串 = 恢复打包默认地址）。 */
ipcMain.handle('vcs:setTarget', (_event, target: string) =>
  wrap<null>(async () => {
    // 换了服务端就别把旧地址的会话留在内存里。
    client?.close();
    client = null;
    session = null;
    saveTarget(target);
    return null;
  }),
);

/**
 * 用保存的凭证尝试恢复会话。失败（过期、被撤销、钥匙串重置）就静默清掉，
 * 让用户重新登录——不要弹错误框，这不是用户做错了什么。
 *
 * `target` 由渲染层传入（默认地址来自打包配置，主进程不读 config.js）。
 */
ipcMain.handle('vcs:tryResume', (_event, target: string = '') =>
  wrap<{ user: UserInfo; expiresAt: number } | null>(async () => {
    const prefs = loadPrefs();
    const resolved = target || prefs.target;
    if (!resolved) return null;
    const now = Math.floor(Date.now() / 1000);
    // 本地记着的会话已到 30 天：不必发请求，直接回登录页。
    if (prefs.sessionExpiresAt > 0 && prefs.sessionExpiresAt <= now) {
      clearToken();
      return null;
    }
    const token = loadToken();
    if (!token) return null;
    const next = new VcsClient(resolved);
    next.useToken(token);
    try {
      const user = await next.whoAmI();
      // 旧服务端的 WhoAmI 把 display_name 留空：用上次登录缓存的姓名兜底，
      // 否则免登录恢复后右上角会从姓名变回账号。
      if (!user.displayName && prefs.displayName) {
        user.displayName = prefs.displayName;
      }
      client?.close();
      client = next;
      session = { target: resolved, username: user.username };
      // 不带 expiresAt 调用：刷新用户名与姓名，但保留原来的到期时间。
      saveLogin(resolved, user.username, {
        token,
        displayName: user.displayName || undefined,
      });
      return { user, expiresAt: prefs.sessionExpiresAt };
    } catch {
      next.close();
      clearToken();
      return null;
    }
  }),
);

ipcMain.handle('vcs:disconnect', () =>
  wrap<null>(async () => {
    client?.close();
    client = null;
    session = null;
    // 主动断开就不要再记住凭证；服务端地址与用户名保留，方便下次登录。
    clearToken();
    return null;
  }),
);

ipcMain.handle('vcs:listRepos', () =>
  wrap(async () => requireClient().listRepos()),
);

ipcMain.handle('vcs:pickDir', () =>
  wrap<string | null>(async () => {
    const result = await dialog.showOpenDialog({
      title: '选择检出目录',
      properties: ['openDirectory', 'createDirectory'],
    });
    return result.canceled ? null : result.filePaths[0];
  }),
);

/**
 * 目录是否仍是一份工作副本：`.vcs/wc.db` 就是唯一判据。
 *
 * 用户删掉检出目录、或者手滑清掉 `.vcs/`，登记在 `settings.json` 里的路径就失效了。
 * 这类路径绝不能再交给原生的 `wc.*`：它们过去会「没有就新建」，等于把被删的工作副本
 * 悄悄重建出来。所有读操作先过这一关。
 */
function isWorkingCopyAt(dir: string): boolean {
  return fs.existsSync(path.join(dir, '.vcs', 'wc.db'));
}

/**
 * 目录属于哪个仓库。**不是工作副本时返回 null**，让界面能平静地引导「检出到这里」，
 * 而不是先弹一条「wc.db 不存在」的错误。
 *
 * 必须只读：原生 `wc.info` 遇到没有 wc.db 的目录会 create 一份空工作副本，
 * 而「换目录 / 再检出一份」只是探测，不该在用户刚选的目录里凭空写 `.vcs/`。
 */
ipcMain.handle('vcs:wcInfo', (_event, dir: string) =>
  wrap(async () => {
    if (!isWorkingCopyAt(dir)) return null;
    try {
      // 必须 await：原生调用是异步的，直接 return 会让这层 try 接不住它的拒绝。
      return await wc.openInfo(dir);
    } catch {
      return null;
    }
  }),
);

/**
 * 当前账号记住的工作副本登记。**按账号读**：目录是「我用这个账号在这台机器上
 * 检出的」，别人的账号看不到，也就不会在别人的界面里冒出来。
 */
ipcMain.handle('vcs:repoDirs', () => wrap(async () => loadRepoDirs(session)));

/**
 * 校验并清理绑定关系：本地目录已经不存在（或 `.vcs/wc.db` 没了）的登记就地摘掉。
 *
 * 界面每次刷新仓库页都会走一次。用户删掉检出目录之后，列表里不该还挂着一份虚拟的
 * 工作副本，更不该因为随后的一次「扫描」把目录重建出来。清理结果立刻落盘，
 * 同时把摘掉了哪些路径回给界面，让这次「消失」有据可查。
 */
ipcMain.handle('vcs:verifyRepoDirs', () =>
  wrap(async () => {
    const before = loadRepoDirs(session);
    const kept: Record<string, string[]> = {};
    const removed: Array<{ repoId: string; dir: string }> = [];
    for (const [repoId, dirs] of Object.entries(before)) {
      const alive = dirs.filter((dir) => {
        if (isWorkingCopyAt(dir)) return true;
        removed.push({ repoId, dir });
        return false;
      });
      if (alive.length > 0) kept[repoId] = alive;
    }
    if (removed.length > 0) saveRepoDirs(session, kept);
    return { dirs: removed.length > 0 ? kept : before, removed };
  }),
);

ipcMain.handle('vcs:saveRepoDirs', (_event, dirs: Record<string, string[]>) =>
  wrap<null>(async () => {
    // 账号由主进程的 session 决定，渲染进程只能写「自己那份」。
    saveRepoDirs(session, dirs);
    return null;
  }),
);

ipcMain.handle(
  'vcs:checkout',
  (_event, repoId: string, dir: string, rev = 0, paths: string[] = []) =>
    wrap(async () => {
      const result = await checkoutRepo(requireClient(), repoId, dir, rev, paths);
      // 检出是很强的「常用」信号：连工作副本都铺了，说明这个仓库真在用。
      bumpRepoUsage(session, repoId);
      return result;
    }),
);

/**
 * 当前账号的仓库使用权重：渲染层按它把常用的仓库排在列表最上面。
 *
 * 记录的是「这个人在这台机器上实际用了什么」，所以按 session 隔离（同工作副本
 * 登记一样），并且只由下面的 `vcs:touchRepo` / `vcs:checkout` 写入。
 */
ipcMain.handle('vcs:repoUsage', () => wrap(async () => loadRepoUsage(session)));

/**
 * 记一次使用，回传折算后的新权重表。
 *
 * 「用了一次」由界面显式上报，而不是由主进程猜：自动选中的第一个仓库不算使用，
 * 否则排在最上面的那个每次启动都会再得一分，越用越钉死在顶部。
 */
ipcMain.handle('vcs:touchRepo', (_event, repoId: string) =>
  wrap(async () => bumpRepoUsage(session, repoId)),
);

/** 仓库 HEAD 的完整相对路径清单：渲染层据此推导目录树，供稀疏检出勾选。 */
ipcMain.handle('vcs:tree', (_event, repoId: string) =>
  wrap(async () =>
    (await requireClient().getTree(repoId, 0)).map((entry) => entry.path),
  ),
);

/** 调整已检出工作副本的稀疏规则（增量补拉 / 移除，不重新铺盘）。 */
ipcMain.handle('vcs:setSparseRules', (_event, dir: string, rules: string[]) =>
  wrap(async () => setSparseRules(requireClient(), dir, rules)),
);

/**
 * 扫描工作副本。目录已经不是工作副本（用户删掉了检出目录）时回报 `missing`，
 * 由界面把这条登记摘掉——**不在这里重新检出**，被删的目录不该被扫描悄悄建回来。
 */
ipcMain.handle('vcs:status', (_event, dir: string) =>
  wrap(async () => {
    if (!isWorkingCopyAt(dir)) {
      return { repoId: '', rev: 0, entries: [], conflicts: [], missing: true };
    }
    const info = await wc.openInfo(dir);
    return {
      repoId: info.repoId,
      rev: info.rev,
      entries: await wc.status(dir),
      // 冲突单独给出来：界面要用红点标出来，且带冲突时提交会被拦。
      conflicts: await wc.conflicts(dir),
      missing: false,
    };
  }),
);

ipcMain.handle('vcs:update', (_event, dir: string) =>
  wrap(async () => updateWorkingCopy(requireClient(), dir)),
);

ipcMain.handle('vcs:revert', (_event, dir: string, paths: string[] = []) =>
  wrap(async () => revertWorkingCopy(dir, paths)),
);

ipcMain.handle('vcs:conflictDetail', (_event, dir: string, path: string) =>
  wrap(async () => wc.conflictDetail(dir, path)),
);

ipcMain.handle(
  'vcs:resolveConflict',
  (_event, dir: string, path: string, choice: ConflictChoice, content?: string) =>
    wrap(async () => {
      await wc.resolveConflict(dir, path, choice, content);
      return null;
    }),
);

/** 离线差异；目录已不是工作副本时没有差异可言，返回空表而不是让原生层建目录。 */
ipcMain.handle('vcs:diff', (_event, dir: string) =>
  wrap(async () => (isWorkingCopyAt(dir) ? await wc.diff(dir) : [])),
);

ipcMain.handle('vcs:commit', (_event, dir: string, message: string) =>
  wrap(async () => commitWorkingCopy(requireClient(), dir, message)),
);

ipcMain.handle('vcs:log', (_event, repoId: string, limit = 20) =>
  wrap(async () => requireClient().getLog(repoId, limit)),
);

// ---------------- 账号自助与建库申请 ----------------

/** 免认证接口（注册）用一个临时连接，不污染当前登录态。 */
async function withTemp<T>(target: string, run: (client: VcsClient) => Promise<T>): Promise<T> {
  const temp = new VcsClient(target);
  try {
    return await run(temp);
  } finally {
    temp.close();
  }
}

ipcMain.handle(
  'vcs:register',
  (
    _event,
    target: string,
    username: string,
    displayName: string,
    password: string,
    reason: string,
  ) =>
    wrap(async () =>
      withTemp(target, (client) => client.register(username, displayName, password, reason)),
    ),
);

ipcMain.handle('vcs:requestRepo', (_event, name: string, reason: string) =>
  wrap(async () => requireClient().requestRepo(name, reason)),
);

ipcMain.handle('vcs:requestRepoExpansion', (_event, repoId: string, reason = '') =>
  wrap(async () => requireClient().requestRepoExpansion(repoId, reason)),
);

// ---------------- 仓库成员（仅 owner 的仓库在界面上可见） ----------------

/**
 * 成员接口挂在 `RepoAdminService` 上，但鉴权走的是**成员门**：调用者必须是该仓库的
 * owner / maintainer。桌面端只在「当前用户就是 owner」时展示成员页，服务端仍会
 * 独立复核一次——界面藏起来不等于权限收窄。
 */
ipcMain.handle('vcs:listMembers', (_event, repoId: string) =>
  wrap(async () => requireClient().listMembers(repoId)),
);

ipcMain.handle('vcs:addMember', (_event, repoId: string, userId: string, role: number) =>
  wrap<null>(async () => {
    await requireClient().addMember(repoId, userId, role);
    return null;
  }),
);

ipcMain.handle('vcs:setMemberRole', (_event, repoId: string, userId: string, role: number) =>
  wrap<null>(async () => {
    await requireClient().setMemberRole(repoId, userId, role);
    return null;
  }),
);

ipcMain.handle('vcs:removeMember', (_event, repoId: string, userId: string) =>
  wrap<null>(async () => {
    await requireClient().removeMember(repoId, userId);
    return null;
  }),
);

/**
 * 注销仓库：只允许仓库拥有者（服务端按 owner 复核）。
 * 注销后仓库从列表中消失，进入服务端回收站；本地工作副本不动，恢复后还能接着用。
 */
ipcMain.handle('vcs:deleteRepo', (_event, repoId: string, reason = '') =>
  wrap(async () => requireClient().deleteRepo(repoId, reason)),
);

/** 按用户名 / 姓名找可添加的 ACTIVE 用户；只返回用户名与姓名，不含敏感信息。 */
ipcMain.handle('vcs:searchUsers', (_event, query: string, limit = 20) =>
  wrap(async () => requireClient().searchUsers(query, limit)),
);

// ---------------- 只读容量 ----------------

ipcMain.handle('vcs:getQuota', (_event, repoId: string) =>
  wrap(async () => requireClient().getQuota(repoId)),
);

// ---------------- 主动清理（彻底删除） ----------------

/** dry-run：只回报「删这些路径能释放多少」，不动任何数据。 */
ipcMain.handle('vcs:estimatePurge', (_event, repoId: string, paths: string[]) =>
  wrap(async () => requireClient().estimatePurge(repoId, paths)),
);

/** 彻底删除：删除提交 + 裁剪标记 + 立即 GC。没有宽限期，也没有撤销。 */
ipcMain.handle('vcs:purge', (_event, repoId: string, paths: string[], reason = '') =>
  wrap(async () => requireClient().purge(repoId, paths, reason)),
);

// ---------------- 预留占位通道 ----------------
// 桌面端不调用；保留仅为维持既有 IPC 契约，实现仍是 notWired。
ipcMain.handle('vcs:getHead', notWired);
ipcMain.handle('vcs:getRegistration', notWired);
ipcMain.handle('vcs:getRepoCreation', notWired);
ipcMain.handle('vcs:checkPassword', notWired);

app.whenReady().then(createWindow);
app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
