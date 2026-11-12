import { contextBridge, ipcRenderer } from 'electron';

/**
 * preload 只做一件事：把主进程的 IPC 通道暴露成类型化的 API。
 *
 * 所有返回都是统一信封 `Envelope<T>`：业务错误（配额超限、out-of-date、非成员……）
 * 与传输错误都走 `ok: false`，渲染进程按 `code` 分支即可，不必解析异常字符串。
 * 真正的网络与文件操作全在主进程，渲染进程拿不到 token，也碰不到磁盘。
 *
 * 桌面端只暴露「仓库使用」所需的 API：连接、仓库列表、工作副本全链路、
 * 账号自助与只读容量。管理与审批相关通道已移除，由服务端 Web 管理端承载。
 */

export type Envelope<T> =
  | { ok: true; data: T }
  | { ok: false; code?: number; message: string };

export interface LoginDto {
  username: string;
  password: string;
  source: 'LOCAL' | 'LDAP';
}

export interface RegisterDto {
  username: string;
  displayName: string;
  password: string;
  reason?: string;
}

export interface RegisterResultDto {
  requestId: string;
  state: number;
  /** 全库第一个账号：已自动批准为管理员，注册时的密码立即可用。 */
  autoApproved: boolean;
  grantedRole: number;
}

export interface RequestRepoDto {
  repoName: string;
  repoIdHint?: string;
  reason?: string;
  requestedQuotaBytes?: number;
}

export interface UserDto {
  userId: string;
  username: string;
  displayName: string;
  role: number;
}

export interface RepoDto {
  repoId: string;
  name: string;
  headRev: number;
  status: number;
}

export interface UpdateResultDto {
  rev: number;
  updated: number;
  added: number;
  removed: number;
  conflicts: number;
  treeConflicts: number;
}

export interface StatusEntryDto {
  path: string;
  kind: 'ADD' | 'MODIFY' | 'DELETE';
}

export interface DiffEntryDto {
  path: string;
  text: string;
}

export interface CommitResultDto {
  rev: number;
  added: number;
  modified: number;
  deleted: number;
  pushed: number;
}

export interface RevisionDto {
  rev: number;
  author: string;
  message: string;
  committedAt: number;
  changeCount: number;
}

export interface ConflictDetailDto {
  path: string;
  kind: string;
  base: string | null;
  mine: string | null;
  theirs: string | null;
  binary: boolean;
}

export type ConflictChoiceDto = 'mine' | 'theirs' | 'custom' | 'delete';

export interface PrefsDto {
  target: string;
  username: string;
  hasToken: boolean;
  canPersistToken: boolean;
  /** 已记住会话的到期时间（Unix 秒）；0 表示没有长期会话。 */
  sessionExpiresAt: number;
}

export interface SessionDto {
  user: UserDto;
  /** 0 表示本次会话有效（没有勾选 30 天免登录）。 */
  expiresAt: number;
}

export interface QuotaDto {
  usedBytes: number;
  /** 生效的配额上限（字节）。 */
  limitBytes: number;
  /** 1=单仓库覆盖 2=全局默认 3=系统兜底 */
  source: number;
}

export interface VcsApi {
  // —— 连接与仓库使用 ——
  connect(
    target: string,
    username: string,
    password: string,
    source?: 'LOCAL' | 'LDAP',
    rememberMe?: boolean,
  ): Promise<Envelope<SessionDto>>;
  disconnect(): Promise<Envelope<null>>;
  /** 上次的连接信息（不含凭证）。 */
  loadPrefs(): Promise<Envelope<PrefsDto>>;
  /** 用保存的凭证恢复会话；没有可用凭证时返回 null。target 由渲染层给出。 */
  tryResume(target: string): Promise<Envelope<SessionDto | null>>;
  /** 登录页「设置」里覆盖服务器地址；空串表示恢复打包默认地址。 */
  setTarget(target: string): Promise<Envelope<null>>;
  listRepos(): Promise<Envelope<RepoDto[]>>;
  pickDir(): Promise<Envelope<string | null>>;
  checkout(
    repoId: string,
    dir: string,
    rev?: number,
  ): Promise<Envelope<{ dir: string; rev: number; files: number }>>;
  status(
    dir: string,
  ): Promise<
    Envelope<{
      repoId: string;
      rev: number;
      entries: StatusEntryDto[];
      conflicts: Array<{ path: string; kind: string }>;
    }>
  >;
  diff(dir: string): Promise<Envelope<DiffEntryDto[]>>;
  commit(dir: string, message: string): Promise<Envelope<CommitResultDto>>;
  update(dir: string): Promise<Envelope<UpdateResultDto>>;
  revert(dir: string, paths?: string[]): Promise<Envelope<number>>;
  conflictDetail(dir: string, path: string): Promise<Envelope<ConflictDetailDto>>;
  resolveConflict(
    dir: string,
    path: string,
    choice: ConflictChoiceDto,
    content?: string,
  ): Promise<Envelope<null>>;
  log(repoId: string, limit?: number): Promise<Envelope<RevisionDto[]>>;

  // —— 账号自助与建库申请 ——
  register(
    target: string,
    username: string,
    displayName: string,
    password: string,
    reason: string,
  ): Promise<Envelope<RegisterResultDto>>;
  requestRepo(
    name: string,
    reason: string,
  ): Promise<Envelope<{ requestId: string; state: number }>>;
  /** 申请扩容：容量固定 +10 GiB，由管理员结合磁盘余量审批。 */
  requestRepoExpansion(
    repoId: string,
    reason?: string,
  ): Promise<Envelope<{ requestId: string; state: number }>>;

  // —— 只读容量 ——
  getQuota(repoId: string): Promise<Envelope<QuotaDto>>;

  // —— 预留占位通道（桌面端不调用，实现仍为 notWired） ——
  getHead(repoId: string): Promise<Envelope<unknown>>;
  getRegistration(requestId: string): Promise<Envelope<unknown>>;
  getRepoCreation(requestId: string): Promise<Envelope<unknown>>;
  checkPassword(password: string): Promise<Envelope<unknown>>;
}

const api: VcsApi = {
  connect: (target, username, password, source, rememberMe) =>
    ipcRenderer.invoke('vcs:connect', target, username, password, source, rememberMe),
  disconnect: () => ipcRenderer.invoke('vcs:disconnect'),
  loadPrefs: () => ipcRenderer.invoke('vcs:loadPrefs'),
  tryResume: (target) => ipcRenderer.invoke('vcs:tryResume', target),
  setTarget: (target) => ipcRenderer.invoke('vcs:setTarget', target),
  listRepos: () => ipcRenderer.invoke('vcs:listRepos'),
  pickDir: () => ipcRenderer.invoke('vcs:pickDir'),
  checkout: (repoId, dir, rev) => ipcRenderer.invoke('vcs:checkout', repoId, dir, rev),
  status: (dir) => ipcRenderer.invoke('vcs:status', dir),
  diff: (dir) => ipcRenderer.invoke('vcs:diff', dir),
  commit: (dir, message) => ipcRenderer.invoke('vcs:commit', dir, message),
  update: (dir) => ipcRenderer.invoke('vcs:update', dir),
  revert: (dir, paths) => ipcRenderer.invoke('vcs:revert', dir, paths),
  conflictDetail: (dir, path) => ipcRenderer.invoke('vcs:conflictDetail', dir, path),
  resolveConflict: (dir, path, choice, content) =>
    ipcRenderer.invoke('vcs:resolveConflict', dir, path, choice, content),
  log: (repoId, limit) => ipcRenderer.invoke('vcs:log', repoId, limit),

  register: (target, username, displayName, password, reason) =>
    ipcRenderer.invoke('vcs:register', target, username, displayName, password, reason),
  requestRepo: (name, reason) => ipcRenderer.invoke('vcs:requestRepo', name, reason),
  requestRepoExpansion: (repoId, reason) =>
    ipcRenderer.invoke('vcs:requestRepoExpansion', repoId, reason),

  getQuota: (repoId) => ipcRenderer.invoke('vcs:getQuota', repoId),

  getHead: (repoId) => ipcRenderer.invoke('vcs:getHead', repoId),
  getRegistration: (requestId) => ipcRenderer.invoke('vcs:getRegistration', requestId),
  getRepoCreation: (requestId) => ipcRenderer.invoke('vcs:getRepoCreation', requestId),
  checkPassword: (password) => ipcRenderer.invoke('vcs:checkPassword', password),
};

contextBridge.exposeInMainWorld('vcs', api);
