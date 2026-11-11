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
 * M4 第一步只接线「单人提交闭环」：
 *   连接 → 仓库列表/新建 → 检出 → 状态 → 离线 diff → 提交 → 历史。
 * 审批台、成员管理、空间面板等通道仍是 `notWired` 占位，等对应界面一起做。
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
  void win.loadFile(path.join(__dirname, '../renderer/index.html'));
}

const NOT_WIRED = '该功能尚未接线（M4 后续里程碑）';

function notWired(): Promise<never> {
  return Promise.reject(new Error(NOT_WIRED));
}

// ---------------- 单人提交闭环 ----------------

ipcMain.handle('vcs:connect', (_event, target: string, username: string, password: string) =>
  wrap<{ user: UserInfo }>(async () => {
    client?.close();
    const next = new VcsClient(target);
    const user = await next.login(username, password);
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

ipcMain.handle('vcs:createRepo', (_event, name: string) =>
  wrap(async () => requireClient().createRepo(name)),
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

// ---------------- 注册、审批台、建库申请 ----------------

/** 免认证接口（注册、设置密码）用一个临时连接，不污染当前登录态。 */
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
  (_event, target: string, username: string, displayName: string, reason: string) =>
    wrap(async () => withTemp(target, (client) => client.register(username, displayName, reason))),
);

ipcMain.handle(
  'vcs:setPassword',
  (_event, target: string, token: string, password: string) =>
    wrap(async () => withTemp(target, (client) => client.setPassword(token, password))),
);

ipcMain.handle('vcs:listRegistrations', (_event, state = 0) =>
  wrap(async () => requireClient().listRegistrations(state)),
);

ipcMain.handle('vcs:approveRegistration', (_event, requestId: string, role: number) =>
  wrap(async () => requireClient().approveRegistration(requestId, role)),
);

ipcMain.handle('vcs:rejectRegistration', (_event, requestId: string, reason: string) =>
  wrap(async () => requireClient().rejectRegistration(requestId, reason)),
);

ipcMain.handle('vcs:requestRepo', (_event, name: string, reason: string, quota = 0) =>
  wrap(async () => requireClient().requestRepo(name, reason, quota)),
);

ipcMain.handle('vcs:listRepoCreations', (_event, state = 0) =>
  wrap(async () => requireClient().listRepoCreations(state)),
);

ipcMain.handle(
  'vcs:approveRepoCreation',
  (_event, requestId: string, soft: number, hard: number, note: string) =>
    wrap(async () => requireClient().approveRepoCreation(requestId, soft, hard, note)),
);

ipcMain.handle('vcs:rejectRepoCreation', (_event, requestId: string, reason: string) =>
  wrap(async () => requireClient().rejectRepoCreation(requestId, reason)),
);

// ---------------- 成员管理 ----------------

ipcMain.handle('vcs:listMembers', (_event, repoId: string) =>
  wrap(async () => requireClient().listMembers(repoId)),
);

ipcMain.handle('vcs:addMember', (_event, repoId: string, userId: string, role: number) =>
  wrap(async () => requireClient().addMember(repoId, userId, role)),
);

ipcMain.handle('vcs:setMemberRole', (_event, repoId: string, userId: string, role: number) =>
  wrap(async () => requireClient().setMemberRole(repoId, userId, role)),
);

ipcMain.handle('vcs:removeMember', (_event, repoId: string, userId: string) =>
  wrap(async () => requireClient().removeMember(repoId, userId)),
);

ipcMain.handle('vcs:searchUsers', (_event, query: string, limit = 20) =>
  wrap(async () => requireClient().searchUsers(query, limit)),
);

// ---------------- 容量治理 ----------------

ipcMain.handle('vcs:getQuota', (_event, repoId: string) =>
  wrap(async () => requireClient().getQuota(repoId)),
);

ipcMain.handle('vcs:listPurgeCandidates', (_event, repoId: string, limit = 10) =>
  wrap(async () => requireClient().listPurgeCandidates(repoId, limit)),
);

ipcMain.handle('vcs:estimatePurge', (_event, repoId: string, paths: string[]) =>
  wrap(async () => requireClient().estimatePurge(repoId, paths)),
);

ipcMain.handle(
  'vcs:purge',
  (_event, repoId: string, paths: string[], immediate: boolean, reason: string) =>
    wrap(async () => requireClient().purge(repoId, paths, immediate, reason)),
);

ipcMain.handle('vcs:getGcStatus', (_event, repoId: string, jobId: string) =>
  wrap(async () => requireClient().getGcStatus(repoId, jobId)),
);

ipcMain.handle('vcs:listPurgedPaths', (_event, repoId: string) =>
  wrap(async () => requireClient().listPurgedPaths(repoId)),
);

ipcMain.handle('vcs:revertPurge', (_event, repoId: string, purgeId: string) =>
  wrap(async () => requireClient().revertPurge(repoId, purgeId)),
);

ipcMain.handle('vcs:getQuotaDefaults', () =>
  wrap(async () => requireClient().getQuotaDefaults()),
);

ipcMain.handle('vcs:updateQuotaDefaults', (_event, softBytes: number, hardBytes: number) =>
  wrap(async () => requireClient().updateQuotaDefaults(softBytes, hardBytes)),
);

ipcMain.handle('vcs:getRepoQuota', (_event, repoId: string) =>
  wrap(async () => requireClient().getRepoQuota(repoId)),
);

ipcMain.handle(
  'vcs:setRepoQuota',
  (_event, repoId: string, softBytes: number, hardBytes: number, inherit: boolean) =>
    wrap(async () => requireClient().setRepoQuota(repoId, softBytes, hardBytes, inherit)),
);

ipcMain.handle('vcs:listRetentionPolicies', (_event, repoId: string) =>
  wrap(async () => requireClient().listRetentionPolicies(repoId)),
);

ipcMain.handle(
  'vcs:upsertRetentionPolicy',
  (
    _event,
    repoId: string,
    policy: {
      policyId?: string;
      pathPattern: string;
      keepMonths: number;
      maxBytes: number;
      maxAgeDays: number;
      enabled: boolean;
      dryRun: boolean;
    },
  ) => wrap(async () => requireClient().upsertRetentionPolicy(repoId, policy)),
);

ipcMain.handle('vcs:deleteRetentionPolicy', (_event, repoId: string, policyId: string) =>
  wrap(async () => requireClient().deleteRetentionPolicy(repoId, policyId)),
);

ipcMain.handle(
  'vcs:applyRetention',
  (_event, repoId: string, dryRun: boolean, immediate: boolean) =>
    wrap(async () => requireClient().applyRetention(repoId, dryRun, immediate)),
);

// ---------------- 未接线（后续里程碑） ----------------

ipcMain.handle('vcs:getHead', notWired);
ipcMain.handle('vcs:getRegistration', notWired);
ipcMain.handle('vcs:getRepoCreation', notWired);
ipcMain.handle('vcs:checkPassword', notWired);

app.whenReady().then(createWindow);
app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
