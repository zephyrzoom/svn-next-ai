/**
 * gRPC 传输层（M4 第一步）。
 *
 * 用 `@grpc/grpc-js` + `@grpc/proto-loader` 直接吃 `proto/repo.proto`：契约是唯一真源，
 * 不再生成第二份 .d.ts 或手写桩代码。选中它而不是把 gRPC 客户端编进 napi 的理由：
 *
 *   - 传输层是**稳定契约**（proto 已定稿、契约测试钉住错误码编号），用 JS 实现成本低；
 *   - 流式接口（Checkout / PushObjects）在 grpc-js 里是一等公民，调试与拦截器生态成熟；
 *   - 不把 tonic + tokio 拖进 Electron 主进程：预编译产物变大、崩溃半径变大，
 *     而本地版本语义才是需要复用 `vcs-core` 的部分（见 wc.ts 的说明）。
 *
 * 本文件只做协议适配，不做任何版本语义判断；工作副本相关的编排在 flow.ts。
 */
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

import * as grpc from '@grpc/grpc-js';
import * as protoLoader from '@grpc/proto-loader';

import { hashBytes, type ChangeKind } from './wc';

/** 与 `proto/repo.proto` 的 ErrorCode 一一对应（编号由契约测试钉死，只增不改）。 */
export const ErrorCode = {
  OK: 1,
  OUT_OF_DATE: 2,
  PATH_CONFLICT: 3,
  TREE_CONFLICT: 4,
  PERMISSION_DENIED: 5,
  QUOTA_EXCEEDED: 6,
  CONTENT_PURGED: 7,
  LOCKED: 8,
  OBJECT_MISSING: 9,
  INVALID_SESSION: 10,
  UNAUTHENTICATED: 11,
  INVALID_CREDENTIALS: 12,
  ACCOUNT_PENDING: 13,
  ACCOUNT_DISABLED: 14,
  REGISTRATION_EXISTS: 15,
  APPROVAL_NOT_FOUND: 16,
  SETUP_TOKEN_INVALID: 17,
  LDAP_UNAVAILABLE: 18,
  TOKEN_EXPIRED: 19,
  PAT_REVOKED: 20,
  REPO_NOT_FOUND: 21,
  REPO_DISABLED: 22,
  REPO_ARCHIVED: 23,
  REPO_NAME_INVALID: 24,
  REPO_CREATION_EXISTS: 25,
  REPO_APPROVAL_NOT_FOUND: 26,
  PASSWORD_POLICY_VIOLATION: 27,
  REPO_NOT_MEMBER: 28,
  REPO_MEMBER_EXISTS: 29,
  REPO_MEMBER_NOT_FOUND: 30,
  REPO_OWNER_PROTECTED: 31,
  REPO_MEMBER_FORBIDDEN: 32,
  USER_NOT_FOUND: 33,
  QUOTA_CONFIG_INVALID: 34,
  IO_ERROR: 35,
  INVALID_ARGUMENT: 36,
} as const;

export type ErrorCodeValue = (typeof ErrorCode)[keyof typeof ErrorCode];

/** 目录组 → 全局身份（role 与 proto 的 Role 一致：1 普通用户 / 3 管理员）。 */
export interface LdapGroupRoleDto {
  groupDn: string;
  role: number;
}

/** 目录服务配置。bind 密码不在这个结构里，写入时单独传、读取时不回传。 */
export interface LdapConfigDto {
  host: string;
  port: number;
  tlsMode: string;
  baseDn: string;
  bindDn: string;
  userFilter: string;
  groupFilter: string;
  attrUsername: string;
  attrDisplayName: string;
  enabled: boolean;
  groupRoles: LdapGroupRoleDto[];
}

/** proto-loader 对未设置的字段会给 undefined，这里统一补默认值给界面用。 */
export function normalizeLdapConfig(raw: any): LdapConfigDto {
  return {
    host: raw?.host ?? '',
    port: Number(raw?.port ?? 636),
    tlsMode: raw?.tlsMode ?? 'ldaps',
    baseDn: raw?.baseDn ?? '',
    bindDn: raw?.bindDn ?? '',
    userFilter: raw?.userFilter ?? '(uid={username})',
    groupFilter: raw?.groupFilter ?? '',
    attrUsername: raw?.attrUsername ?? 'uid',
    attrDisplayName: raw?.attrDisplayName ?? 'cn',
    enabled: Boolean(raw?.enabled),
    groupRoles: (raw?.groupRoles ?? []).map((g: any) => ({
      groupDn: String(g.groupDn ?? ''),
      role: Number(g.role ?? 1),
    })),
  };
}

/** 服务端业务错误：`code` 是 proto 的 ErrorCode，客户端据此给不同提示。 */
export class VcsError extends Error {
  constructor(
    readonly code: number,
    message: string,
  ) {
    super(message);
    this.name = 'VcsError';
  }

  /** 便于 UI 决定「弹提示」还是「跳登录」。 */
  get isAuthRelated(): boolean {
    return (
      this.code === ErrorCode.UNAUTHENTICATED ||
      this.code === ErrorCode.INVALID_CREDENTIALS ||
      this.code === ErrorCode.TOKEN_EXPIRED ||
      this.code === ErrorCode.PAT_REVOKED
    );
  }
}

interface PbStatus {
  code: number;
  message: string;
}

export interface UserInfo {
  userId: string;
  username: string;
  displayName: string;
  role: number;
}

export interface RepoInfo {
  repoId: string;
  name: string;
  headRev: number;
  status: number;
  /**
   * 仓库拥有者的 user_id。owner 唯一且不可转让，所以「我是不是 owner」
   * 直接用它与当前登录用户比较即可，不必再单独查一次成员角色。
   */
  ownerUserId: string;
}

export interface TreeEntry {
  path: string;
  hash: string;
  size: number;
  kind: number;
}

export interface ChangePayload {
  path: string;
  kind: ChangeKind;
  hash?: string;
  size: number;
}

export interface CommitOutcome {
  rev: number;
  rootTree: string;
}

export interface RegistrationRow {
  requestId: string;
  username: string;
  displayName: string;
  reason: string;
  /** 1=pending 2=approved 3=rejected 4=expired（与 proto 的 RegistrationStatus 一致）。 */
  state: number;
  submittedAt: number;
}

export interface RepoRequestRow {
  requestId: string;
  repoName: string;
  requesterUserId: string;
  reason: string;
  requestedQuotaBytes: number;
  /** 1=pending 2=approved 3=rejected 4=expired（与 proto 的 RepoCreationStatus 一致）。 */
  state: number;
  submittedAt: number;
}

/** 扩容申请：申请人只提交申请，容量由服务端按固定 10 GiB 增量算出。 */
export interface RepoExpansionRow {
  requestId: string;
  repoId: string;
  repoName: string;
  incrementBytes: number;
  quotaBeforeBytes: number;
  targetQuotaBytes: number;
  reason: string;
  /** 1=pending 2=approved 3=rejected 4=expired（与 proto 的 RepoCreationStatus 一致）。 */
  state: number;
  submittedAt: number;
}

export interface QuotaRow {
  usedBytes: number;
  /** 生效的配额上限（字节）。 */
  limitBytes: number;
  /** 1=单仓库覆盖 2=全局默认 3=系统兜底（与 proto 的 QuotaSource 一致）。 */
  source: number;
}

export interface CandidateRow {
  path: string;
  logicalBytes: number;
  uniqueBytes: number;
  firstSeenRev: number;
  lastModifiedRev: number;
}

export interface PurgeEstimateRow {
  reclaimableBytes: number;
  affectedRevisions: number;
  sharedPaths: string[];
}

export interface PurgeResultRow {
  newRev: number;
  gcJobId: string;
}

export interface QuotaDefaultsRow {
  bytes: number;
  updatedAt: number;
  updatedBy: string;
}

export interface RepoQuotaRow {
  repoBytes: number;
  effectiveBytes: number;
  /** 1=单仓库覆盖 2=全局默认 3=系统兜底 */
  source: number;
}

export interface RetentionPolicyRow {
  policyId: string;
  pathPattern: string;
  keepMonths: number;
  maxBytes: number;
  maxAgeDays: number;
  enabled: boolean;
  dryRun: boolean;
  createdAt: number;
}

export interface RetentionApplyRow {
  candidates: string[];
  reclaimableBytes: number;
  /** 0 表示没有产生提交（预览或没有候选）。 */
  newRev: number;
}

export interface PurgedPathRow {
  purgeId: string;
  path: string;
  /** pending（宽限期）/ active（已生效）/ reverted（已撤销） */
  state: string;
  reason: string;
  purgedBy: string;
  createdAt: number;
  graceUntil: number;
  activatedAt: number;
}

export interface GcStatusRow {
  state: number;
  progressPercent: number;
  markedObjects: number;
  sweptObjects: number;
  reclaimedBytes: number;
  error: string;
}

export interface MemberRow {
  userId: string;
  username: string;
  displayName: string;
  /** 1=owner 2=maintainer 3=writer 4=reader（与 proto 的 RepoMemberRole 一致）。 */
  role: number;
}

export interface UserSummaryRow {
  userId: string;
  username: string;
  displayName: string;
}

export interface RevisionInfo {
  rev: number;
  author: string;
  message: string;
  committedAt: number;
  changeCount: number;
}

const CHANGE_KIND_NUMBER: Record<ChangeKind, number> = {
  ADD: 1,
  MODIFY: 2,
  DELETE: 3,
};

/** 反向映射：服务端返回的枚举数字 → 客户端字符串。RENAME/COPY 暂按修改处理。 */
const CHANGE_KIND_NAME: Record<number, ChangeKind> = {
  1: 'ADD',
  2: 'MODIFY',
  3: 'DELETE',
};

function assertOk(status: PbStatus | undefined): void {
  if (!status) return;
  if (status.code !== ErrorCode.OK) {
    throw new VcsError(status.code, status.message);
  }
}

/**
 * 契约文件的位置。
 *
 * 开发态在仓库根（monorepo 的唯一真源）；打包后由 `scripts/stage-assets.cjs`
 * 复制进 `desktop/proto/` 随 asar 一起分发。按顺序探测，避免为「打包 or 开发」
 * 写两套分支——这个函数也不能 import electron，否则脱离 Electron 的冒烟跑不起来。
 */
function protoPath(): string {
  // 顺序很重要：开发态永远用仓库根的那份（真源），否则 `desktop/proto/` 里上次
  // 打包留下的副本会盖住它——新增 RPC 后客户端会报「方法不存在」，极难排查。
  const candidates = [
    path.resolve(__dirname, '..', '..', '..', 'proto', 'repo.proto'), // 开发：仓库根
    path.resolve(__dirname, '..', '..', 'proto', 'repo.proto'), // 打包：asar 内的 proto
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error(
    `找不到 proto/repo.proto（已查找：${candidates.join('、')}）。` +
      '打包前请先执行 npm run stage。',
  );
}

type Unary = (
  request: unknown,
  metadata: grpc.Metadata,
  callback: (err: grpc.ServiceError | null, response: any) => void,
) => void;

/**
 * 单条 gRPC 消息的收发上限，与服务端 `vcs_grpc::MAX_MESSAGE_BYTES` 保持一致。
 *
 * **与单文件大小无关**：对象内容按 {@link CHUNK_BYTES} 分块传输，所以这个数值只兜
 * 元数据类的大响应（如 `GetTree`）。grpc-js 默认只有 4 MiB，多文件仓库的树就会撞上。
 */
const MAX_MESSAGE_BYTES = 32 * 1024 * 1024;

/** 单个对象超过它就切成 `ObjectChunk` 发送：单文件因此没有大小上限。 */
const CHUNK_BYTES = 1024 * 1024;

/** 推送时单条消息的目标体积与条数（与服务端分批节奏一致）。 */
const PUSH_BATCH_BYTES = 4 * 1024 * 1024;
const PUSH_BATCH_OBJECTS = 32;
/** 单条消息的 protobuf 开销余量，避免「负载刚好 4 MiB + 开销」贴到上限。 */
const PUSH_MESSAGE_HEADROOM = 64 * 1024;
const PUSH_PAYLOAD_BYTES = PUSH_BATCH_BYTES - PUSH_MESSAGE_HEADROOM;

/** 把 grpc-js / 服务端的传输层错误翻成能看懂的中文提示。 */
function toVcsError(err: grpc.ServiceError): VcsError {
  const detail = err.details || err.message || '';
  if (/larger than max|message length too large/i.test(detail)) {
    return new VcsError(
      err.code,
      '服务端返回的数据超过单条消息上限（大文件本身已按 1 MiB 分块传输，' +
        '不受此限制）；通常是服务端版本过旧，升级后重试',
    );
  }
  return new VcsError(err.code, detail);
}

/** 对象文件的存储标记：0 = 原始字节（不压缩），与服务端 CAS 的布局一致。 */
const RAW_OBJECT_FLAG = Buffer.from([0]);

/** spool 里一个对象的落盘路径：`objects/<前两位>/<其余>`，正是 CAS 的目录布局。 */
export function spoolObjectPath(spoolDir: string, hash: string): string {
  return path.join(spoolDir, 'objects', hash.slice(0, 2), hash.slice(2));
}

/**
 * 把对象流收进 spool 目录，返回对象个数。
 *
 * 关键是**全程只驻留一个分片**：小对象整体到达、大对象按 1 MiB 分片追写到文件，
 * 既不再 `Buffer.concat` 整个文件，也不会把整库攒在内存里。落盘格式就是 CAS 的
 * 布局（首字节标记 0 + 原始内容），native 侧用 `FileObjectStore` 直接流式读。
 */
function receiveToSpool(call: any, spoolDir: string): Promise<number> {
  return new Promise((resolve, reject) => {
    let current: { hash: string; fd: number; nextIndex: number } | null = null;
    let count = 0;
    let failure: Error | null = null;

    const closeCurrent = () => {
      if (!current) return;
      fs.closeSync(current.fd);
      current = null;
    };
    const begin = (hash: string) => {
      const target = spoolObjectPath(spoolDir, hash);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      const fd = fs.openSync(target, 'w');
      fs.writeSync(fd, RAW_OBJECT_FLAG);
      current = { hash, fd, nextIndex: 0 };
    };

    call.on('data', (batch: any) => {
      if (failure) return;
      try {
        for (const object of batch.objects ?? []) {
          // 小对象整份到达：一条消息一个文件。
          closeCurrent();
          begin(object.hash as string);
          fs.writeSync(current!.fd, Buffer.from(object.data));
          closeCurrent();
          count += 1;
        }
        for (const chunk of batch.chunks ?? []) {
          const hash = chunk.hash as string;
          if (!current || current.hash !== hash) {
            closeCurrent();
            begin(hash);
          }
          if (Number(chunk.index ?? 0) !== current!.nextIndex) {
            throw new VcsError(
              ErrorCode.INVALID_ARGUMENT,
              `对象 ${hash} 的分块序号不连续：期望第 ${current!.nextIndex} 块，收到第 ${chunk.index} 块`,
            );
          }
          fs.writeSync(current!.fd, Buffer.from(chunk.data));
          current!.nextIndex += 1;
          if (chunk.last) {
            closeCurrent();
            count += 1;
          }
        }
      } catch (err) {
        // 事件回调里抛错不会 reject 这个 Promise，只能记下来并主动取消流。
        failure = err as Error;
        closeCurrent();
        call.cancel();
      }
    });
    call.on('error', (err: grpc.ServiceError) => {
      closeCurrent();
      reject(failure ?? toVcsError(err));
    });
    call.on('end', () => {
      closeCurrent();
      if (failure) reject(failure);
      else resolve(count);
    });
  });
}

/**
 * 推送器：小对象攒批、大对象从磁盘按 1 MiB 分片流式读。
 *
 * 关键有两点：
 *   1. 同一时刻内存里只有一个分片，与文件 / 仓库大小无关；
 *   2. `call.write` 返回 false 时必须等 `drain` 再继续——不等的话 grpc-js 会把
 *      几十 MB 的分片全排进发送队列，等于又把文件攒回内存（实测 64 MiB 文件会涨 80 MiB）。
 */
class ObjectUploader {
  private batch: any[] = [];
  private bytes = 0;
  private chunks: any[] = [];
  private chunkBytes = 0;

  constructor(
    private readonly call: any,
    private readonly sessionId: string,
  ) {}

  /**
   * 写一条消息并等它真正被消费。
   *
   * grpc-js 的 `write` 是异步序列化 + 排队：只等 `drain` 不够（队列里能堆几十 MB），
   * 也不能在回调之前复用源缓冲（会写坏内容）。等回调既解决背压，也让
   * 「复用一块 1 MiB 读缓冲」变得安全——大文件推送不再制造每片一份的垃圾。
   */
  private write(message: any): Promise<void> {
    return new Promise<void>((resolve, reject) => {
      this.call.write(message, (err?: Error | null) => {
        if (err) reject(err);
        else resolve();
      });
    });
  }

  private async flushChunks(): Promise<void> {
    if (this.chunks.length === 0) return;
    const chunks = this.chunks;
    this.chunks = [];
    this.chunkBytes = 0;
    await this.write({ sessionId: this.sessionId, objects: [], chunks });
  }

  private async flushObjects(): Promise<void> {
    if (this.batch.length === 0) return;
    const objects = this.batch;
    this.batch = [];
    this.bytes = 0;
    await this.write({ sessionId: this.sessionId, objects, chunks: [] });
  }

  /** 小对象（已在内存里）：按「32 个 / 4 MiB」攒批。 */
  async addBuffer(hash: string, data: Buffer): Promise<void> {
    if (data.length > CHUNK_BYTES) {
      await this.flushObjects();
      let index = 0;
      for (let offset = 0; offset < data.length; offset += CHUNK_BYTES) {
        const end = Math.min(offset + CHUNK_BYTES, data.length);
        await this.addChunk(hash, index, data.subarray(offset, end), end === data.length);
        index += 1;
      }
      await this.flushChunks();
      return;
    }
    if (this.batch.length >= PUSH_BATCH_OBJECTS || this.bytes + data.length > PUSH_PAYLOAD_BYTES) {
      await this.flushObjects();
    }
    this.batch.push({ hash, kind: 1, data, compressed: false });
    this.bytes += data.length;
  }

  /** 从磁盘文件推送：读一块发一块，文件多大都不会整份进内存。 */
  async addFile(hash: string, filePath: string): Promise<void> {
    const size = fs.statSync(filePath).size;
    if (size <= CHUNK_BYTES) {
      await this.addBuffer(hash, fs.readFileSync(filePath));
      return;
    }
    await this.flushObjects();
    const fd = fs.openSync(filePath, 'r');
    try {
      const buffer = Buffer.allocUnsafe(CHUNK_BYTES);
      let offset = 0;
      let index = 0;
      while (offset < size) {
        const read = fs.readSync(fd, buffer, 0, CHUNK_BYTES, offset);
        if (read <= 0) break;
        offset += read;
        // grpc-js 会延后序列化，复用的读缓冲不能直接交出去（会被下一片覆盖）：
        // 每片拷一份，配合上面的「写一条等一条」把在途分片数量钉在一两个。
        await this.addChunk(hash, index, Buffer.from(buffer.subarray(0, read)), offset >= size);
        index += 1;
      }
    } finally {
      fs.closeSync(fd);
    }
    await this.flushChunks();
  }

  private async addChunk(hash: string, index: number, data: Buffer, last: boolean): Promise<void> {
    if (this.chunks.length > 0 && this.chunkBytes + data.length > PUSH_PAYLOAD_BYTES) {
      await this.flushChunks();
    }
    this.chunks.push({ hash, kind: 1, index, data, last });
    this.chunkBytes += data.length;
  }

  async finish(): Promise<void> {
    await this.flushObjects();
    await this.flushChunks();
    this.call.end();
  }
}

export class VcsClient {
  private readonly auth: Record<string, Unary>;
  private readonly repo: Record<string, Unary>;
  private readonly admin: Record<string, Unary>;
  private raw: grpc.Client[] = [];
  private token?: string;
  private expiresAt = 0;

  constructor(readonly target: string) {
    const definition = protoLoader.loadSync(protoPath(), {
      keepCase: false,
      longs: Number,
      enums: Number,
      defaults: true,
      oneofs: true,
    });
    const pkg = grpc.loadPackageDefinition(definition) as any;
    const v1 = pkg.vcs.repo.v1;
    const credentials = grpc.credentials.createInsecure();
    // 收发上限与单文件大小无关（大对象会分块），只是给 GetTree 这类大响应留余量。
    const options: grpc.ChannelOptions = {
      'grpc.max_receive_message_length': MAX_MESSAGE_BYTES,
      'grpc.max_send_message_length': MAX_MESSAGE_BYTES,
    };
    const auth = new v1.AuthService(target, credentials, options);
    const repo = new v1.RepoService(target, credentials, options);
    const admin = new v1.RepoAdminService(target, credentials, options);
    this.raw = [auth, repo, admin];
    this.auth = auth as Record<string, Unary>;
    this.repo = repo as Record<string, Unary>;
    this.admin = admin as Record<string, Unary>;
  }

  close(): void {
    for (const client of this.raw) client.close();
    this.raw = [];
  }

  private metadata(): grpc.Metadata {
    const md = new grpc.Metadata();
    if (this.token) md.set('authorization', `Bearer ${this.token}`);
    return md;
  }

  private invoke<T>(
    client: Record<string, Unary>,
    method: string,
    request: unknown,
  ): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      client[method].call(client, request, this.metadata(), (err, response) => {
        if (err) {
          reject(toVcsError(err));
          return;
        }
        resolve(response as T);
      });
    });
  }

  /**
   * `source` 决定走本地口令还是目录 bind；`rememberMe` 让服务端签发 30 天会话，
   * 桌面端登录固定传 true（登录页已无勾选框），CLI / Web 管理端仍是 12 小时。
   * 两者都只影响本次签发，不改变后续请求的鉴权方式。
   */
  async login(
    username: string,
    password: string,
    source: 'LOCAL' | 'LDAP' = 'LOCAL',
    rememberMe = false,
  ): Promise<UserInfo> {
    const resp = await this.invoke<any>(this.auth, 'login', {
      username,
      password,
      source: source === 'LDAP' ? 2 : 1,
      rememberMe,
    });
    assertOk(resp.status);
    this.token = resp.accessToken as string;
    this.expiresAt = Number(resp.expiresAt ?? 0);
    return resp.user as UserInfo;
  }

  /** 已登录身份。用于刷新页面后确认 token 仍有效。 */
  async whoAmI(): Promise<UserInfo> {
    const resp = await this.invoke<any>(this.auth, 'whoAmI', {});
    assertOk(resp.status);
    return resp.user as UserInfo;
  }

  logout(): void {
    this.token = undefined;
    this.expiresAt = 0;
  }

  /** 当前凭证（仅主进程内部用于加密持久化，绝不发给渲染进程）。 */
  get tokenValue(): string | undefined {
    return this.token;
  }

  /** 本次登录签发的到期时间（Unix 秒）；用于保存到本地、显示「免登录至」。 */
  get expiresAtValue(): number {
    return this.expiresAt;
  }

  /** 用已保存的凭证恢复会话；随后调 `whoAmI` 确认它还有效。 */
  useToken(token: string): void {
    this.token = token;
  }

  /** 仓库容量：已用字节与生效配额（含来源）。 */
  async getQuota(repoId: string): Promise<QuotaRow> {
    const resp = await this.invoke<any>(this.repo, 'getQuota', {
      repoId,
      pathPrefix: '',
    });
    assertOk(resp.status);
    const effective = resp.effective ?? {};
    return {
      usedBytes: resp.usedBytes as number,
      limitBytes: (effective.bytes ?? resp.limitBytes) as number,
      source: (effective.source ?? 0) as number,
    };
  }

  /** 可清理目录候选（顶层目录 + 独占大小），按独占字节降序。 */
  async listPurgeCandidates(repoId: string, limit = 10): Promise<CandidateRow[]> {
    const resp = await this.invoke<any>(this.repo, 'listPurgeCandidates', { repoId, limit });
    assertOk(resp.status);
    return (resp.candidates ?? []).map((row: any) => ({
      path: row.path as string,
      logicalBytes: row.logicalBytes as number,
      uniqueBytes: row.uniqueBytes as number,
      firstSeenRev: row.firstSeenRev as number,
      lastModifiedRev: row.lastModifiedRev as number,
    }));
  }

  /** dry-run：预估清理这些路径能释放多少，不修改任何数据。 */
  async estimatePurge(repoId: string, paths: string[]): Promise<PurgeEstimateRow> {
    const resp = await this.invoke<any>(this.repo, 'estimatePurge', { repoId, paths });
    assertOk(resp.status);
    return {
      reclaimableBytes: resp.reclaimableBytes as number,
      affectedRevisions: resp.affectedRevisions as number,
      sharedPaths: (resp.sharedPaths ?? []) as string[],
    };
  }

  /**
   * 清理：删除提交 + 路径裁剪 + GC。
   *
   * `immediate` 跳过 7 天宽限期——磁盘已满时这是唯一有用的选项。
   */
  async purge(
    repoId: string,
    paths: string[],
    immediate: boolean,
    reason: string,
  ): Promise<PurgeResultRow> {
    const resp = await this.invoke<any>(this.repo, 'purge', {
      repoId,
      paths,
      immediate,
      reason,
    });
    assertOk(resp.status);
    return { newRev: resp.newRev as number, gcJobId: resp.gcJobId as string };
  }

  /** 回收站：已执行的清理记录。 */
  async listPurgedPaths(repoId: string): Promise<PurgedPathRow[]> {
    const resp = await this.invoke<any>(this.repo, 'listPurgedPaths', { repoId });
    assertOk(resp.status);
    return (resp.entries ?? []).map((row: any) => ({
      purgeId: row.purgeId as string,
      path: row.path as string,
      state: row.state as string,
      reason: row.reason as string,
      purgedBy: row.purgedBy as string,
      createdAt: row.createdAt as number,
      graceUntil: row.graceUntil as number,
      activatedAt: row.activatedAt as number,
    }));
  }

  /**
   * 撤销清理：把路径内容恢复到 HEAD，并让 GC 不再跳过它们。
   *
   * 内容若已被 GC 物理回收，服务端返回 `CONTENT_PURGED`——这是唯一诚实的答案，
   * 不能伪造一个指向空内容的版本。
   */
  async revertPurge(repoId: string, purgeId: string): Promise<number> {
    const resp = await this.invoke<any>(this.repo, 'revertPurge', { repoId, purgeId });
    assertOk(resp.status);
    return resp.newRev as number;
  }

  /** 全局默认配额（所有未单独设置覆盖的仓库继承它）。 */
  async getQuotaDefaults(): Promise<QuotaDefaultsRow> {
    const resp = await this.invoke<any>(this.admin, 'getQuotaDefaults', {});
    assertOk(resp.status);
    const row = resp.defaults ?? {};
    return {
      bytes: (row.bytes ?? 0) as number,
      updatedAt: (row.updatedAt ?? 0) as number,
      updatedBy: (row.updatedBy ?? '') as string,
    };
  }

  async updateQuotaDefaults(bytes: number): Promise<QuotaDefaultsRow> {
    const resp = await this.invoke<any>(this.admin, 'updateQuotaDefaults', { bytes });
    assertOk(resp.status);
    const row = resp.defaults ?? {};
    return {
      bytes: (row.bytes ?? 0) as number,
      updatedAt: (row.updatedAt ?? 0) as number,
      updatedBy: (row.updatedBy ?? '') as string,
    };
  }

  /** 单仓库配额：覆盖值 + 生效值及来源。 */
  async getRepoQuota(repoId: string): Promise<RepoQuotaRow> {
    const resp = await this.invoke<any>(this.admin, 'getRepoQuota', { repoId });
    assertOk(resp.status);
    const effective = resp.effective ?? {};
    return {
      repoBytes: (resp.repoBytes ?? 0) as number,
      effectiveBytes: (effective.bytes ?? 0) as number,
      source: (effective.source ?? 0) as number,
    };
  }

  /** `inherit=true` 时清零并回到继承全局默认。 */
  async setRepoQuota(repoId: string, bytes: number, inherit: boolean): Promise<RepoQuotaRow> {
    const resp = await this.invoke<any>(this.admin, 'setRepoQuota', {
      repoId,
      bytes,
      inherit,
    });
    assertOk(resp.status);
    const effective = resp.effective ?? {};
    return {
      repoBytes: inherit ? 0 : bytes,
      effectiveBytes: (effective.bytes ?? 0) as number,
      source: (effective.source ?? 0) as number,
    };
  }

  async listRetentionPolicies(repoId: string): Promise<RetentionPolicyRow[]> {
    const resp = await this.invoke<any>(this.repo, 'listRetentionPolicies', { repoId });
    assertOk(resp.status);
    return (resp.policies ?? []).map((row: any) => ({
      policyId: row.policyId as string,
      pathPattern: row.pathPattern as string,
      keepMonths: row.keepMonths as number,
      maxBytes: row.maxBytes as number,
      maxAgeDays: row.maxAgeDays as number,
      enabled: row.enabled as boolean,
      dryRun: row.dryRun as boolean,
      createdAt: row.createdAt as number,
    }));
  }

  async upsertRetentionPolicy(
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
  ): Promise<RetentionPolicyRow> {
    const resp = await this.invoke<any>(this.repo, 'upsertRetentionPolicy', {
      repoId,
      policyId: policy.policyId ?? '',
      pathPattern: policy.pathPattern,
      keepMonths: policy.keepMonths,
      maxBytes: policy.maxBytes,
      maxAgeDays: policy.maxAgeDays,
      enabled: policy.enabled,
      dryRun: policy.dryRun,
    });
    assertOk(resp.status);
    const row = resp.policy ?? {};
    return {
      policyId: row.policyId as string,
      pathPattern: row.pathPattern as string,
      keepMonths: row.keepMonths as number,
      maxBytes: row.maxBytes as number,
      maxAgeDays: row.maxAgeDays as number,
      enabled: row.enabled as boolean,
      dryRun: row.dryRun as boolean,
      createdAt: row.createdAt as number,
    };
  }

  async deleteRetentionPolicy(repoId: string, policyId: string): Promise<void> {
    const resp = await this.invoke<any>(this.repo, 'deleteRetentionPolicy', {
      repoId,
      policyId,
    });
    assertOk(resp.status);
  }

  /** 评估保留策略；`dryRun=false` 时真正执行清理（会生成删除提交）。 */
  async applyRetention(repoId: string, dryRun: boolean, immediate: boolean): Promise<RetentionApplyRow> {
    const resp = await this.invoke<any>(this.repo, 'applyRetention', {
      repoId,
      dryRun,
      immediate,
    });
    assertOk(resp.status);
    return {
      candidates: (resp.candidates ?? []) as string[],
      reclaimableBytes: resp.reclaimableBytes as number,
      newRev: resp.newRev as number,
    };
  }

  async getGcStatus(repoId: string, jobId: string): Promise<GcStatusRow> {
    const resp = await this.invoke<any>(this.repo, 'getGcStatus', {
      repoId,
      jobId,
    });
    assertOk(resp.status);
    return {
      state: resp.state as number,
      progressPercent: resp.progressPercent as number,
      markedObjects: resp.markedObjects as number,
      sweptObjects: resp.sweptObjects as number,
      reclaimedBytes: resp.reclaimedBytes as number,
      error: resp.error as string,
    };
  }

  /** 仓库成员列表（含角色）。 */
  async listMembers(repoId: string): Promise<MemberRow[]> {
    const resp = await this.invoke<any>(this.admin, 'listMembers', { repoId });
    assertOk(resp.status);
    return (resp.members ?? []).map((row: any) => ({
      userId: row.userId as string,
      username: row.username as string,
      displayName: row.displayName as string,
      role: row.role as number,
    }));
  }

  /** 加成员。角色矩阵由服务端判定（maintainer 只能加 writer/reader）。 */
  async addMember(repoId: string, userId: string, role: number): Promise<void> {
    const resp = await this.invoke<any>(this.admin, 'addMember', { repoId, userId, role });
    assertOk(resp.status);
  }

  async setMemberRole(repoId: string, userId: string, role: number): Promise<void> {
    const resp = await this.invoke<any>(this.admin, 'setMemberRole', {
      repoId,
      userId,
      role,
    });
    assertOk(resp.status);
  }

  async removeMember(repoId: string, userId: string): Promise<void> {
    const resp = await this.invoke<any>(this.admin, 'removeMember', { repoId, userId });
    assertOk(resp.status);
  }

  /** 按用户名 / 姓名搜人（只返回 ACTIVE 用户，且只有三项字段）。 */
  async searchUsers(query: string, limit = 20): Promise<UserSummaryRow[]> {
    const resp = await this.invoke<any>(this.admin, 'searchUsers', { query, limit });
    assertOk(resp.status);
    return (resp.users ?? []).map((row: any) => ({
      userId: row.userId as string,
      username: row.username as string,
      displayName: row.displayName as string,
    }));
  }

  /**
   * 自助注册（免认证）。
   *
   * 密码在申请时就设置并交给服务端哈希保存：审批通过后本人直接用它登录，
   * 不再需要一次性链接。
   *
   * 空库第一个账号会被自动批准为管理员，`autoApproved` 为 true，注册完即可登录。
   */
  async register(
    username: string,
    displayName: string,
    password: string,
    reason: string,
  ): Promise<{
    requestId: string;
    state: number;
    autoApproved: boolean;
    grantedRole: number;
  }> {
    const resp = await this.invoke<any>(this.auth, 'register', {
      username,
      displayName,
      password,
      reason,
    });
    assertOk(resp.status);
    return {
      requestId: resp.requestId as string,
      state: resp.state as number,
      autoApproved: Boolean(resp.autoApproved),
      grantedRole: Number(resp.grantedRole ?? 0),
    };
  }

  /** 读取目录配置（不含明文 bind 密码）。仅管理员可调用。 */
  async getLdapConfig(): Promise<{
    config: LdapConfigDto;
    bindPasswordSet: boolean;
  }> {
    const resp = await this.invoke<any>(this.auth, 'getLdapConfig', {});
    assertOk(resp.status);
    return {
      config: normalizeLdapConfig(resp.config),
      bindPasswordSet: Boolean(resp.bindPasswordSet),
    };
  }

  /** 保存目录配置。`bindPassword` 留空表示沿用已保存的密码。 */
  async updateLdapConfig(config: LdapConfigDto, bindPassword: string): Promise<void> {
    const resp = await this.invoke<any>(this.auth, 'updateLdapConfig', {
      config,
      bindPassword,
    });
    assertOk(resp.status);
  }

  /** 连通性测试：服务账号 bind + 抽样搜索，不改动本地账号。 */
  async testLdapConnection(
    config: LdapConfigDto,
    bindPassword: string,
  ): Promise<{ boundDn: string; userCountSample: number }> {
    const resp = await this.invoke<any>(this.auth, 'testLdapConnection', {
      config,
      bindPassword,
    });
    assertOk(resp.status);
    return {
      boundDn: (resp.boundDn as string) ?? '',
      userCountSample: Number(resp.userCountSample ?? 0),
    };
  }

  /** 待审批注册申请。`state` 传 0 表示全部。 */
  async listRegistrations(state = 0): Promise<RegistrationRow[]> {
    const resp = await this.invoke<any>(this.auth, 'listRegistrations', {
      filter: state,
      limit: 50,
    });
    assertOk(resp.status);
    return (resp.requests ?? []).map((row: any) => ({
      requestId: row.requestId as string,
      username: row.username as string,
      displayName: row.displayName as string,
      reason: row.reason as string,
      state: row.state as number,
      submittedAt: row.submittedAt as number,
    }));
  }

  /**
   * 审批通过注册。
   *
   * 审批只做放行、不选身份：通过即普通用户，申请人注册时设好的密码就是登录口令，
   * 服务端不再回传任何凭证。
   */
  async approveRegistration(requestId: string): Promise<{ userId: string }> {
    const resp = await this.invoke<any>(this.auth, 'approveRegistration', { requestId });
    assertOk(resp.status);
    return { userId: resp.userId as string };
  }

  async rejectRegistration(requestId: string, reason: string): Promise<void> {
    const resp = await this.invoke<any>(this.auth, 'rejectRegistration', {
      requestId,
      reason,
    });
    assertOk(resp.status);
  }

  /**
   * 提交仓库创建申请（任何 ACTIVE 用户）。
   *
   * 容量不填：服务端固定按 10 GiB 核定，审批人也不用再配数字。
   */
  async requestRepo(repoName: string, reason: string): Promise<{ requestId: string; state: number }> {
    const resp = await this.invoke<any>(this.admin, 'requestRepo', {
      repoName,
      repoIdHint: '',
      reason,
    });
    assertOk(resp.status);
    return { requestId: resp.requestId as string, state: resp.state as number };
  }

  /**
   * 申请扩容：同样不填容量，服务端固定按 +10 GiB 计算。
   * 是否批准由管理员结合服务器磁盘余量决定。
   */
  async requestRepoExpansion(
    repoId: string,
    reason = '',
  ): Promise<{ requestId: string; state: number }> {
    const resp = await this.invoke<any>(this.admin, 'requestRepoExpansion', { repoId, reason });
    assertOk(resp.status);
    return { requestId: resp.requestId as string, state: resp.state as number };
  }

  /** 我提交的扩容申请（管理员则看全部）。 */
  async listRepoQuotaRequests(state = 0): Promise<RepoExpansionRow[]> {
    const resp = await this.invoke<any>(this.admin, 'listRepoQuotaRequests', {
      filter: state,
      limit: 50,
    });
    assertOk(resp.status);
    return (resp.requests ?? []).map((row: any) => ({
      requestId: row.requestId as string,
      repoId: row.repoId as string,
      repoName: row.repoName as string,
      incrementBytes: row.incrementBytes as number,
      quotaBeforeBytes: row.quotaBeforeBytes as number,
      targetQuotaBytes: row.targetQuotaBytes as number,
      reason: row.reason as string,
      state: row.state as number,
      submittedAt: row.submittedAt as number,
    }));
  }

  async listRepoCreations(state = 0): Promise<RepoRequestRow[]> {
    const resp = await this.invoke<any>(this.admin, 'listRepoCreations', {
      filter: state,
      limit: 50,
    });
    assertOk(resp.status);
    return (resp.requests ?? []).map((row: any) => ({
      requestId: row.requestId as string,
      repoName: row.repoName as string,
      requesterUserId: row.requesterUserId as string,
      reason: row.reason as string,
      requestedQuotaBytes: row.requestedQuotaBytes as number,
      state: row.state as number,
      submittedAt: row.submittedAt as number,
    }));
  }

  /** 审批通过建库申请：容量由服务端固定 10 GiB，审批人只填意见。 */
  async approveRepoCreation(requestId: string, decisionNote = ''): Promise<RepoInfo> {
    const resp = await this.invoke<any>(this.admin, 'approveRepoCreation', {
      requestId,
      decisionNote,
    });
    assertOk(resp.status);
    return resp.repo as RepoInfo;
  }

  async rejectRepoCreation(requestId: string, reason: string): Promise<void> {
    const resp = await this.invoke<any>(this.admin, 'rejectRepoCreation', {
      requestId,
      reason,
    });
    assertOk(resp.status);
  }

  async listRepos(): Promise<RepoInfo[]> {
    const resp = await this.invoke<any>(this.admin, 'listRepos', { filter: 1 });
    assertOk(resp.status);
    return (resp.repos ?? []) as RepoInfo[];
  }

  async createRepo(name: string, quotaBytes = 0): Promise<RepoInfo> {
    const resp = await this.invoke<any>(this.admin, 'createRepoDirect', {
      repoName: name,
      repoIdHint: '',
      quotaBytes,
    });
    assertOk(resp.status);
    return resp.repo as RepoInfo;
  }

  async getHead(repoId: string): Promise<{ rev: number; rootTree: string }> {
    const resp = await this.invoke<any>(this.repo, 'getHead', { repoId });
    assertOk(resp.status);
    return { rev: resp.rev as number, rootTree: resp.rootTree as string };
  }

  /** `paths` 为稀疏路径前缀；空数组表示整库。 */
  async getTree(repoId: string, rev = 0, paths: string[] = []): Promise<TreeEntry[]> {
    const resp = await this.invoke<any>(this.repo, 'getTree', {
      repoId,
      rev,
      paths,
      depth: 1, // DEPTH_INFINITY
    });
    assertOk(resp.status);
    return (resp.entries ?? []).map((e: any) => ({
      path: e.name as string,
      hash: e.hash as string,
      size: e.size as number,
      kind: e.kind as number,
    }));
  }

  /**
   * 把某个版本、某组稀疏路径下的全部 blob 拉到 spool 目录，返回对象个数。
   *
   * 内容直接落盘（CAS 布局），内存里只有一个分片；随后的 `wc.checkout` 用
   * `FileObjectStore` 流式读，所以整库多大都不会进内存。
   */
  async downloadObjectsToSpool(
    repoId: string,
    rev: number,
    paths: string[],
    spoolDir: string,
  ): Promise<number> {
    const call = (this.repo as any).checkout({ repoId, rev, paths, depth: 1 }, this.metadata());
    return receiveToSpool(call, spoolDir);
  }

  /**
   * 调试 / 测试用：把整份内容读成「哈希 → 内容」。
   *
   * **生产路径不要用**——它会把所有对象同时放进内存，正是这次要摆脱的做法。
   */
  async downloadObjects(
    repoId: string,
    rev: number,
    paths: string[] = [],
  ): Promise<Map<string, Buffer>> {
    const spool = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-readall-'));
    try {
      await this.downloadObjectsToSpool(repoId, rev, paths, spool);
      const out = new Map<string, Buffer>();
      const walk = (dir: string) => {
        for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
          const full = path.join(dir, entry.name);
          if (entry.isDirectory()) walk(full);
          else if (entry.name.length === 62) {
            const shard = path.basename(path.dirname(full));
            // 首字节是存储标记，内容从第二字节开始。
            out.set(`${shard}${entry.name}`, fs.readFileSync(full).subarray(1));
          }
        }
      };
      walk(path.join(spool, 'objects'));
      return out;
    } finally {
      fs.rmSync(spool, { recursive: true, force: true });
    }
  }

  async getLog(repoId: string, limit = 20): Promise<RevisionInfo[]> {
    const resp = await this.invoke<any>(this.repo, 'getLog', {
      repoId,
      fromRev: 0,
      toRev: 0,
      pathPrefix: '',
      limit,
    });
    assertOk(resp.status);
    return (resp.revisions ?? []) as RevisionInfo[];
  }

  /** 按哈希补对象到 spool 目录（FetchObjects 双向流）。update 前用，内存不随对象变大。 */
  async fetchObjectsToSpool(hashes: string[], spoolDir: string): Promise<number> {
    if (hashes.length === 0) return 0;
    const call = (this.repo as any).fetchObjects(this.metadata());
    const done = receiveToSpool(call, spoolDir);
    call.write({ sessionId: '', hashes });
    call.end();
    return done;
  }

  /**
   * 拉取 baseRev → targetRev 之间的差异。
   *
   * 服务端从「客户端给的基线」开始算，所以本地混合版本也能用：多算的条目在客户端是幂等的，
   * 漏算的条目才会永远拉不下来。
   */
  async update(
    repoId: string,
    baseRev: number,
    targetRev = 0,
    paths: string[] = [],
  ): Promise<{ rev: number; changes: ChangePayload[] }> {
    const resp = await this.invoke<any>(this.repo, 'update', {
      repoId,
      // 稀疏副本把每条规则都作为一条基线带上：服务端据此只算这些路径的差异。
      // 老语义（整库）用一条 path 为空的基线表示。
      bases: paths.length
        ? paths.map((p) => ({ path: p, baseRev }))
        : [{ path: '', baseRev }],
      targetRev,
    });
    assertOk(resp.status);
    const changes = (resp.changes ?? []).map((change: any) => ({
      path: change.path as string,
      kind: CHANGE_KIND_NAME[change.kind as number] ?? 'MODIFY',
      hash: change.newHash ? (change.newHash as string) : undefined,
      size: 0,
    })) as ChangePayload[];
    return { rev: resp.rev as number, changes };
  }

  /**
   * 两阶段提交的 Begin：只声明变更集，服务端回告诉我们哪些对象还没有。
   * 返回 `missingHashes` 空数组时，说明内容服务端全有（例如同一份文件被别的路径提交过）。
   */
  async beginCommit(
    repoId: string,
    baseRev: number,
    changes: ChangePayload[],
    message: string,
    estimatedNewBytes: number,
  ): Promise<{ sessionId: string; missingHashes: string[] }> {
    const resp = await this.invoke<any>(this.repo, 'beginCommit', {
      repoId,
      baseRev,
      changes: changes.map((change) => ({
        kind: CHANGE_KIND_NUMBER[change.kind],
        path: change.path,
        oldHash: '',
        newHash: change.hash ?? '',
        fromPath: '',
        props: [],
      })),
      message,
      estimatedNewBytes,
    });
    assertOk(resp.status);
    return {
      sessionId: resp.sessionId as string,
      missingHashes: (resp.missingHashes ?? []) as string[],
    };
  }

  /**
   * 客户端流的统一入口：建流、喂对象、收尾，再按 `PushAck` 判定成败。
   *
   * `feed` 负责往上传器里塞对象；等它把每个分片真正写出去（含背压等待）之后才收尾，
   * 所以内存占用由「一个分片」封顶。
   */
  private async pushSession(
    sessionId: string,
    feed: (uploader: ObjectUploader) => Promise<void>,
  ): Promise<number> {
    let resolveAck!: (value: any) => void;
    let rejectAck!: (err: Error) => void;
    const ack = new Promise<any>((resolve, reject) => {
      resolveAck = resolve;
      rejectAck = reject;
    });
    // 失败时由下面的 `await feed` 决定成败，先挂一个 catch 避免「未处理的拒绝」。
    ack.catch(() => undefined);

    const call = (this.repo as any).pushObjects(
      this.metadata(),
      (err: grpc.ServiceError | null, response: any) => {
        if (err) rejectAck(toVcsError(err));
        else resolveAck(response);
      },
    );
    const uploader = new ObjectUploader(call, sessionId);
    try {
      await feed(uploader);
      await uploader.finish();
    } catch (err) {
      call.cancel();
      throw err;
    }
    const response = await ack;
    // PushAck 也带业务 Status：内容与地址不符等错误在这里暴露，而不是等到 Finalize。
    assertOk(response.status);
    return response.receivedObjects as number;
  }

  /**
   * 把内存里的对象推上去。
   *
   * 小对象按「32 个 + 4 MiB」攒批；超过 {@link CHUNK_BYTES} 的切成连续 `ObjectChunk`。
   * 生产路径一般用 {@link pushFiles}：直接从磁盘读，连「整份进内存」都省掉。
   */
  async pushObjects(
    sessionId: string,
    objects: Array<{ hash: string; data: Buffer }>,
  ): Promise<number> {
    return this.pushSession(sessionId, async (uploader) => {
      for (const object of objects) await uploader.addBuffer(object.hash, object.data);
    });
  }

  /**
   * 从磁盘文件推对象：读一块发一块。
   *
   * `files` 给的是「哈希 → 本地路径」，内容由文件流提供，内存里只有当前分片。
   * 服务端会按地址复验摘要，客户端这边不再整份读出来算一遍。
   */
  async pushFiles(
    sessionId: string,
    files: Array<{ hash: string; path: string }>,
  ): Promise<number> {
    return this.pushSession(sessionId, async (uploader) => {
      for (const file of files) await uploader.addFile(file.hash, file.path);
    });
  }

  async finalizeCommit(sessionId: string): Promise<CommitOutcome> {
    const resp = await this.invoke<any>(this.repo, 'finalizeCommit', {
      sessionId,
      message: '',
    });
    assertOk(resp.status);
    return { rev: resp.rev as number, rootTree: resp.rootTree as string };
  }

  async abortCommit(sessionId: string): Promise<void> {
    await this.invoke<any>(this.repo, 'abortCommit', { sessionId });
  }
}

/** 供 flow 复用的内容哈希入口，避免两处各引一份哈希实现。 */
export { hashBytes };
