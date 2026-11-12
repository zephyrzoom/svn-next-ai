import { app, BrowserWindow, dialog, ipcMain } from 'electron';
import * as path from 'node:path';

import { VcsError, VcsClient, type UserInfo } from './client';
import { checkoutRepo, commitWorkingCopy, revertWorkingCopy, updateWorkingCopy } from './flow';
import { clearToken, loadPrefs, loadToken, saveLogin } from './settings';
import * as wc from './wc';
import type { ConflictChoice } from './wc';

/**
 * Electron 主进程：**唯一**发起网络与文件操作的地方，渲染进程只通过 preload 暴露的
 * 类型化 API 调用。渲染进程开着 sandbox + contextIsolation，即使页面被注入脚本，
 * 也拿不到文件系统与 token。
 *
 * 桌面端定位：只做「仓库使用」的客户端 —— 连接、仓库列表/选择、工作副本的
 * 检出/状态/差异/冲突/提交/更新/还原/历史，以及账号自助注册（注册即设密码）、提交
 * 建库申请。管理与审批（成员、配额、空间治理、目录配置、审批台等）统一由服务端
 * vcs serve 内置的 Web 管理端承载，桌面端不再暴露这些通道。
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
    wrap<{ user: UserInfo }>(async () => {
      client?.close();
      const next = new VcsClient(target);
      const user = await next.login(username, password, source);
      client = next;
      // 登录成功才写入本地设置：凭证用 safeStorage 加密，不落明文。
      if (next.tokenValue) saveLogin(target, user.username, next.tokenValue);
      return { user };
    }),
);

/** 上次的连接信息（不含凭证），用于把登录框填好。 */
ipcMain.handle('vcs:loadPrefs', () => wrap(async () => loadPrefs()));

/**
 * 用保存的凭证尝试恢复会话。失败（过期、被撤销、钥匙串重置）就静默清掉，
 * 让用户重新登录——不要弹错误框，这不是用户做错了什么。
 */
ipcMain.handle('vcs:tryResume', () =>
  wrap<{ user: UserInfo } | null>(async () => {
    const token = loadToken();
    if (!token) return null;
    const prefs = loadPrefs();
    if (!prefs.target) return null;
    const next = new VcsClient(prefs.target);
    next.useToken(token);
    try {
      const user = await next.whoAmI();
      client?.close();
      client = next;
      saveLogin(prefs.target, user.username, token);
      return { user };
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

ipcMain.handle('vcs:checkout', (_event, repoId: string, dir: string, rev = 0) =>
  wrap(async () => checkoutRepo(requireClient(), repoId, dir, rev)),
);

ipcMain.handle('vcs:status', (_event, dir: string) =>
  wrap(async () => {
    const info = wc.openInfo(dir);
    return {
      repoId: info.repoId,
      rev: info.rev,
      entries: wc.status(dir),
      // 冲突单独给出来：界面要用红点标出来，且带冲突时提交会被拦。
      conflicts: wc.conflicts(dir),
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
      wc.resolveConflict(dir, path, choice, content);
      return null;
    }),
);

ipcMain.handle('vcs:diff', (_event, dir: string) =>
  wrap(async () => wc.diff(dir)),
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

ipcMain.handle('vcs:requestRepo', (_event, name: string, reason: string, quota = 0) =>
  wrap(async () => requireClient().requestRepo(name, reason, quota)),
);

// ---------------- 只读容量 ----------------

ipcMain.handle('vcs:getQuota', (_event, repoId: string) =>
  wrap(async () => requireClient().getQuota(repoId)),
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
