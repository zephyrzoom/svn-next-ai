import { contextBridge, ipcRenderer } from 'electron';

/**
 * preload 只做一件事：把主进程的 IPC 通道暴露成类型化的 API。
 *
 * 所有返回都是统一信封 `Envelope<T>`：业务错误（配额超限、out-of-date、非成员……）
 * 与传输错误都走 `ok: false`，渲染进程按 `code` 分支即可，不必解析异常字符串。
 * 真正的网络与文件操作全在主进程，渲染进程拿不到 token，也碰不到磁盘。
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
  reason?: string;
}

export interface RequestRepoDto {
  repoName: string;
  repoIdHint?: string;
  reason?: string;
  requestedQuotaBytes?: number;
}

export interface PurgeRequestDto {
  paths: string[];
  immediate: boolean;
  reason?: string;
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

export interface RegistrationDto {
  requestId: string;
  username: string;
  displayName: string;
  reason: string;
  state: number;
  submittedAt: number;
}

export interface RepoRequestDtoRow {
  requestId: string;
  repoName: string;
  requesterUserId: string;
  reason: string;
  requestedQuotaBytes: number;
  state: number;
  submittedAt: number;
}

export interface ApprovedRegistrationDto {
  userId: string;
  setupToken: string;
  expiresAt: number;
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
}

export interface QuotaDto {
  usedBytes: number;
  softLimit: number;
  hardLimit: number;
  /** 1=单仓库覆盖 2=全局默认 3=系统兜底 */
  source: number;
}

export interface CandidateDto {
  path: string;
  logicalBytes: number;
  uniqueBytes: number;
  firstSeenRev: number;
  lastModifiedRev: number;
}

export interface PurgeEstimateDto {
  reclaimableBytes: number;
  affectedRevisions: number;
  sharedPaths: string[];
}

export interface PurgeResultDto {
  newRev: number;
  gcJobId: string;
}

export interface QuotaDefaultsDto {
  softBytes: number;
  hardBytes: number;
  updatedAt: number;
  updatedBy: string;
}

export interface RepoQuotaDto {
  repoSoftBytes: number;
  repoHardBytes: number;
  effectiveSoft: number;
  effectiveHard: number;
  /** 1=单仓库覆盖 2=全局默认 3=系统兜底 */
  source: number;
}

export interface RetentionPolicyDto {
  policyId: string;
  pathPattern: string;
  keepMonths: number;
  maxBytes: number;
  maxAgeDays: number;
  enabled: boolean;
  dryRun: boolean;
  createdAt: number;
}

export interface RetentionApplyDto {
  candidates: string[];
  reclaimableBytes: number;
  newRev: number;
}

export interface PurgedPathDto {
  purgeId: string;
  path: string;
  state: string;
  reason: string;
  purgedBy: string;
  createdAt: number;
  graceUntil: number;
  activatedAt: number;
}

export interface GcStatusDto {
  state: number;
  progressPercent: number;
  markedObjects: number;
  sweptObjects: number;
  reclaimedBytes: number;
  error: string;
}

export interface MemberDto {
  userId: string;
  username: string;
  displayName: string;
  /** 1=owner 2=maintainer 3=writer 4=reader */
  role: number;
}

export interface UserSummaryDto {
  userId: string;
  username: string;
  displayName: string;
}

export interface VcsApi {
  // —— 单人提交闭环（M4 第一步，已接线）——
  connect(target: string, username: string, password: string): Promise<Envelope<{ user: UserDto }>>;
  disconnect(): Promise<Envelope<null>>;
  /** 上次的连接信息（不含凭证）。 */
  loadPrefs(): Promise<Envelope<PrefsDto>>;
  /** 用保存的凭证恢复会话；没有可用凭证时返回 null。 */
  tryResume(): Promise<Envelope<{ user: UserDto } | null>>;
  listRepos(): Promise<Envelope<RepoDto[]>>;
  createRepo(name: string): Promise<Envelope<RepoDto>>;
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

  // —— 注册、审批台、建库申请（M4 第三段）——
  register(
    target: string,
    username: string,
    displayName: string,
    reason: string,
  ): Promise<Envelope<{ requestId: string; state: number }>>;
  setPassword(target: string, token: string, password: string): Promise<Envelope<null>>;
  listRegistrations(state?: number): Promise<Envelope<RegistrationDto[]>>;
  approveRegistration(
    requestId: string,
    role: number,
  ): Promise<Envelope<ApprovedRegistrationDto>>;
  rejectRegistration(requestId: string, reason: string): Promise<Envelope<null>>;
  requestRepo(
    name: string,
    reason: string,
    quota?: number,
  ): Promise<Envelope<{ requestId: string; state: number }>>;
  listRepoCreations(state?: number): Promise<Envelope<RepoRequestDtoRow[]>>;
  approveRepoCreation(
    requestId: string,
    soft: number,
    hard: number,
    note: string,
  ): Promise<Envelope<RepoDto>>;
  rejectRepoCreation(requestId: string, reason: string): Promise<Envelope<null>>;

  // —— 成员管理 ——
  listMembers(repoId: string): Promise<Envelope<MemberDto[]>>;
  addMember(repoId: string, userId: string, role: number): Promise<Envelope<null>>;
  setMemberRole(repoId: string, userId: string, role: number): Promise<Envelope<null>>;
  removeMember(repoId: string, userId: string): Promise<Envelope<null>>;
  searchUsers(query: string, limit?: number): Promise<Envelope<UserSummaryDto[]>>;

  // —— 成员管理 ——
  listMembers(repoId: string): Promise<Envelope<MemberDto[]>>;
  addMember(repoId: string, userId: string, role: number): Promise<Envelope<null>>;
  setMemberRole(repoId: string, userId: string, role: number): Promise<Envelope<null>>;
  removeMember(repoId: string, userId: string): Promise<Envelope<null>>;
  searchUsers(query: string, limit?: number): Promise<Envelope<UserSummaryDto[]>>;

  // —— 后续里程碑（通道已留，实现为 notWired）——
  getHead(repoId: string): Promise<Envelope<unknown>>;
  getQuota(repoId: string): Promise<Envelope<QuotaDto>>;
  listPurgeCandidates(repoId: string, limit?: number): Promise<Envelope<CandidateDto[]>>;
  estimatePurge(repoId: string, paths: string[]): Promise<Envelope<PurgeEstimateDto>>;
  purge(
    repoId: string,
    paths: string[],
    immediate: boolean,
    reason: string,
  ): Promise<Envelope<PurgeResultDto>>;
  getGcStatus(repoId: string, jobId: string): Promise<Envelope<GcStatusDto>>;
  listPurgedPaths(repoId: string): Promise<Envelope<PurgedPathDto[]>>;
  revertPurge(repoId: string, purgeId: string): Promise<Envelope<number>>;
  listRetentionPolicies(repoId: string): Promise<Envelope<RetentionPolicyDto[]>>;
  upsertRetentionPolicy(
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
  ): Promise<Envelope<RetentionPolicyDto>>;
  deleteRetentionPolicy(repoId: string, policyId: string): Promise<Envelope<null>>;
  applyRetention(
    repoId: string,
    dryRun: boolean,
    immediate: boolean,
  ): Promise<Envelope<RetentionApplyDto>>;
  getRegistration(requestId: string): Promise<Envelope<unknown>>;
  getRepoCreation(requestId: string): Promise<Envelope<unknown>>;
  checkPassword(password: string): Promise<Envelope<unknown>>;
  getQuotaDefaults(): Promise<Envelope<QuotaDefaultsDto>>;
  updateQuotaDefaults(
    softBytes: number,
    hardBytes: number,
  ): Promise<Envelope<QuotaDefaultsDto>>;
  getRepoQuota(repoId: string): Promise<Envelope<RepoQuotaDto>>;
  setRepoQuota(
    repoId: string,
    softBytes: number,
    hardBytes: number,
    inherit: boolean,
  ): Promise<Envelope<RepoQuotaDto>>;
}

const api: VcsApi = {
  connect: (target, username, password) =>
    ipcRenderer.invoke('vcs:connect', target, username, password),
  disconnect: () => ipcRenderer.invoke('vcs:disconnect'),
  loadPrefs: () => ipcRenderer.invoke('vcs:loadPrefs'),
  tryResume: () => ipcRenderer.invoke('vcs:tryResume'),
  listRepos: () => ipcRenderer.invoke('vcs:listRepos'),
  createRepo: (name) => ipcRenderer.invoke('vcs:createRepo', name),
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

  register: (target, username, displayName, reason) =>
    ipcRenderer.invoke('vcs:register', target, username, displayName, reason),
  setPassword: (target, token, password) =>
    ipcRenderer.invoke('vcs:setPassword', target, token, password),
  listRegistrations: (state) => ipcRenderer.invoke('vcs:listRegistrations', state),
  approveRegistration: (requestId, role) =>
    ipcRenderer.invoke('vcs:approveRegistration', requestId, role),
  rejectRegistration: (requestId, reason) =>
    ipcRenderer.invoke('vcs:rejectRegistration', requestId, reason),
  requestRepo: (name, reason, quota) =>
    ipcRenderer.invoke('vcs:requestRepo', name, reason, quota),
  listRepoCreations: (state) => ipcRenderer.invoke('vcs:listRepoCreations', state),
  approveRepoCreation: (requestId, soft, hard, note) =>
    ipcRenderer.invoke('vcs:approveRepoCreation', requestId, soft, hard, note),
  rejectRepoCreation: (requestId, reason) =>
    ipcRenderer.invoke('vcs:rejectRepoCreation', requestId, reason),

  listMembers: (repoId) => ipcRenderer.invoke('vcs:listMembers', repoId),
  addMember: (repoId, userId, role) => ipcRenderer.invoke('vcs:addMember', repoId, userId, role),
  setMemberRole: (repoId, userId, role) =>
    ipcRenderer.invoke('vcs:setMemberRole', repoId, userId, role),
  removeMember: (repoId, userId) => ipcRenderer.invoke('vcs:removeMember', repoId, userId),
  searchUsers: (query, limit) => ipcRenderer.invoke('vcs:searchUsers', query, limit),

  getHead: (repoId) => ipcRenderer.invoke('vcs:getHead', repoId),
  getQuota: (repoId) => ipcRenderer.invoke('vcs:getQuota', repoId),
  listPurgeCandidates: (repoId, limit) =>
    ipcRenderer.invoke('vcs:listPurgeCandidates', repoId, limit),
  estimatePurge: (repoId, paths) => ipcRenderer.invoke('vcs:estimatePurge', repoId, paths),
  purge: (repoId, paths, immediate, reason) =>
    ipcRenderer.invoke('vcs:purge', repoId, paths, immediate, reason),
  getGcStatus: (repoId, jobId) => ipcRenderer.invoke('vcs:getGcStatus', repoId, jobId),
  listPurgedPaths: (repoId) => ipcRenderer.invoke('vcs:listPurgedPaths', repoId),
  revertPurge: (repoId, purgeId) => ipcRenderer.invoke('vcs:revertPurge', repoId, purgeId),
  listRetentionPolicies: (repoId) => ipcRenderer.invoke('vcs:listRetentionPolicies', repoId),
  upsertRetentionPolicy: (repoId, policy) =>
    ipcRenderer.invoke('vcs:upsertRetentionPolicy', repoId, policy),
  deleteRetentionPolicy: (repoId, policyId) =>
    ipcRenderer.invoke('vcs:deleteRetentionPolicy', repoId, policyId),
  applyRetention: (repoId, dryRun, immediate) =>
    ipcRenderer.invoke('vcs:applyRetention', repoId, dryRun, immediate),
  getRegistration: (requestId) => ipcRenderer.invoke('vcs:getRegistration', requestId),
  getRepoCreation: (requestId) => ipcRenderer.invoke('vcs:getRepoCreation', requestId),
  checkPassword: (password) => ipcRenderer.invoke('vcs:checkPassword', password),
  getQuotaDefaults: () => ipcRenderer.invoke('vcs:getQuotaDefaults'),
  updateQuotaDefaults: (softBytes, hardBytes) =>
    ipcRenderer.invoke('vcs:updateQuotaDefaults', softBytes, hardBytes),
  getRepoQuota: (repoId) => ipcRenderer.invoke('vcs:getRepoQuota', repoId),
  setRepoQuota: (repoId, softBytes, hardBytes, inherit) =>
    ipcRenderer.invoke('vcs:setRepoQuota', repoId, softBytes, hardBytes, inherit),
};

contextBridge.exposeInMainWorld('vcs', api);
