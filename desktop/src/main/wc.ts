/**
 * 本地工作副本（桌面端）。
 *
 * **这一层不实现任何版本语义**，只把 `crates/vcs-napi` 暴露的函数包成 TypeScript 接口。
 * 哈希、`wc.db`、pristine 基线、离线 diff、冲突元数据全部在 `vcs-core` 里，
 * 与 CLI / 服务端跑的是同一份代码——这是「一份语义、多处使用」的落点。
 *
 * 为什么要这么绕（Rust → napi → TS）：JS 里重写一遍 wc 是最容易漂移的地方。
 * `vcs-core::wc` 有 22 个单测盯着「纳秒 mtime 快路径」「冲突三哈希」「COW 更新」，
 * 这些行为在 JS 里复制一遍必然走样。
 *
 * 原生模块由 `npm run build:native` 产出（cargo build + 改后缀），不引 @napi-rs/cli。
 */
import * as path from 'node:path';

export type ChangeKind = 'ADD' | 'MODIFY' | 'DELETE';

export interface WorkingCopyInfo {
  repoId: string;
  /** 工作副本整体记录的版本（最近一次检出 / 更新 / 提交后的版本）。 */
  rev: number;
  /**
   * 最旧的条目基线版本。混合版本下 update 必须从这里取差异：
   * 刚 revert 过的条目仍停在旧基线，只看 `rev` 会以为「已经最新」而漏拉内容。
   */
  oldestRev: number;
}

export interface StatusEntry {
  path: string;
  kind: ChangeKind | 'CONFLICT';
}

export interface LocalChange {
  path: string;
  kind: ChangeKind;
  hash?: string;
  size: number;
}

export interface DiffEntry {
  path: string;
  text: string;
}

export interface CheckoutEntry {
  path: string;
  hash: string;
  size: number;
}

export type ConflictChoice = 'mine' | 'theirs' | 'custom' | 'delete';

/** 三方内容。文本按 UTF-8 解出来给界面；二进制用 `binary` 标记，界面只显示大小。 */
export interface ConflictDetail {
  path: string;
  kind: string;
  base: string | null;
  mine: string | null;
  theirs: string | null;
  baseHash: string | null;
  mineHash: string | null;
  theirsHash: string | null;
  binary: boolean;
}

export interface UpdateOutcome {
  rev: number;
  updated: number;
  added: number;
  removed: number;
  conflicts: number;
  treeConflicts: number;
}

interface NativeBinding {
  hashBytesHex(data: Buffer): string;
  wcInfo(dir: string): string;
  wcCheckout(
    dir: string,
    repoId: string,
    rev: number,
    entriesJson: string,
    hashes: string[],
    blobs: Buffer[],
  ): number;
  wcStatus(dir: string): string;
  wcDiff(dir: string): string;
  wcLocalChanges(dir: string): string;
  wcMarkCommitted(dir: string, rev: number, changesJson: string): void;
  wcApplyUpdate(
    dir: string,
    repoId: string,
    targetRev: number,
    changesJson: string,
    hashes: string[],
    blobs: Buffer[],
  ): string;
  wcAdvanceBase(dir: string, rev: number): number;
  wcRevert(dir: string, pathsJson: string): number;
  wcConflicts(dir: string): string;
  wcConflictDetail(dir: string, path: string): {
    path: string;
    kind: string;
    baseHash: string | null;
    mineHash: string | null;
    theirsHash: string | null;
    base: Buffer | null;
    mine: Buffer | null;
    theirs: Buffer | null;
  };
  wcResolveConflict(dir: string, path: string, choice: string, content?: Buffer): void;
  wcHasConflicts(dir: string): boolean;
}

/**
 * 原生模块的真实路径。
 *
 * 打包后代码在 `app.asar` 里，而 `.node` 必须从磁盘加载（Electron 不会把原生模块
 * 从 asar 里映射出来），electron-builder 会把它解到 `app.asar.unpacked/`。
 * 这里做一次路径改写，开发态与打包态用同一份代码。
 */
function nativeFile(): string {
  const packed = path.join(__dirname, '..', '..', 'native', 'vcs-napi.node');
  return packed.includes('app.asar')
    ? packed.replace('app.asar', 'app.asar.unpacked')
    : packed;
}

function loadNative(): NativeBinding {
  const file = nativeFile();
  try {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    return require(file) as NativeBinding;
  } catch (err) {
    const reason = err instanceof Error ? err.message : String(err);
    throw new Error(
      `原生模块加载失败（${file}）：${reason}\n` +
        '请先构建：npm run build:native（需要 Rust 工具链）',
    );
  }
}

const native = loadNative();

/** BLAKE3-256 十六进制摘要，与服务端同一实现。 */
export function hashBytes(data: Buffer): string {
  return native.hashBytesHex(data);
}

/** 工作副本归属与基线版本。 */
export function openInfo(dir: string): WorkingCopyInfo {
  return JSON.parse(native.wcInfo(dir)) as WorkingCopyInfo;
}

/**
 * 检出：把服务端的 (路径 → 内容哈希) 与对象内容铺到磁盘，并写 pristine 基线。
 *
 * `contents` 以哈希为键，正是 Checkout 流回来的形态；同一份内容被多路径引用时只落一份基线。
 */
export function checkout(
  dir: string,
  repoId: string,
  rev: number,
  entries: CheckoutEntry[],
  contents: Map<string, Buffer>,
): number {
  const hashes: string[] = [];
  const blobs: Buffer[] = [];
  for (const entry of entries) {
    const data = contents.get(entry.hash);
    if (!data) {
      throw new Error(`缺少对象 ${entry.hash}（${entry.path}），检出中断`);
    }
    hashes.push(entry.hash);
    blobs.push(data);
  }
  return native.wcCheckout(dir, repoId, rev, JSON.stringify(entries), hashes, blobs);
}

/** 扫描工作副本（mtime 快路径 + 改动条目重算哈希）。 */
export function status(dir: string): StatusEntry[] {
  return JSON.parse(native.wcStatus(dir)) as StatusEntry[];
}

/** 离线 diff：基线 vs 磁盘。 */
export function diff(dir: string): DiffEntry[] {
  return JSON.parse(native.wcDiff(dir)) as DiffEntry[];
}

/** 待提交变更（含内容地址）。 */
export function localChanges(dir: string): LocalChange[] {
  return JSON.parse(native.wcLocalChanges(dir)) as LocalChange[];
}

/** 提交成功后刷新基线并推进版本。 */
export function markCommitted(dir: string, rev: number, changes: LocalChange[]): void {
  native.wcMarkCommitted(dir, rev, JSON.stringify(changes));
}

/** 应用服务端差异（update）。冲突不覆盖本地内容，只记元数据。 */
export function applyUpdate(
  dir: string,
  repoId: string,
  targetRev: number,
  changes: LocalChange[],
  contents: Map<string, Buffer>,
): UpdateOutcome {
  // 只为真正需要的对象准备内容：删除类变更没有 hash。
  const hashes: string[] = [];
  const blobs: Buffer[] = [];
  for (const change of changes) {
    if (!change.hash) continue;
    const data = contents.get(change.hash);
    if (!data) {
      throw new Error(`缺少对象 ${change.hash}（${change.path}），更新中断`);
    }
    hashes.push(change.hash);
    blobs.push(data);
  }
  const raw = native.wcApplyUpdate(
    dir,
    repoId,
    targetRev,
    JSON.stringify(changes),
    hashes,
    blobs,
  );
  const parsed = JSON.parse(raw) as {
    rev: number;
    updated: number;
    added: number;
    removed: number;
    conflicts: number;
    tree_conflicts: number;
  };
  return {
    rev: parsed.rev,
    updated: parsed.updated,
    added: parsed.added,
    removed: parsed.removed,
    conflicts: parsed.conflicts,
    treeConflicts: parsed.tree_conflicts,
  };
}

/**
 * 全量 update 成功后推进干净条目的基线版本。
 *
 * 只应由 `updateWorkingCopy` 调用：它知道这次变更集覆盖了整个仓库。
 * 不推进的话，那些「在仓库里从未被改过」的条目会永远钉在旧 base_rev 上，
 * 每次 update 都要把同一段差异重算一遍。
 */
export function advanceBase(dir: string, rev: number): number {
  return native.wcAdvanceBase(dir, rev);
}

/** 撤销本地改动（空数组表示全部）。未纳入版本控制的文件不会被删除。 */
export function revert(dir: string, paths: string[] = []): number {
  return native.wcRevert(dir, JSON.stringify(paths));
}

export function conflicts(dir: string): Array<{ path: string; kind: string }> {
  return JSON.parse(native.wcConflicts(dir)) as Array<{ path: string; kind: string }>;
}

/** 二进制判定：与 core 的 diff 一致，出现 NUL 就当二进制，界面不做文本合并。 */
function looksBinary(data: Buffer | null): boolean {
  return data ? data.subarray(0, 8000).includes(0) : false;
}

function decode(data: Buffer | null): string | null {
  return data === null ? null : data.toString('utf8');
}

/**
 * 读一条冲突的三方内容。
 *
 * base / theirs 来自 pristine（冲突发生时已缓存），mine 来自工作目录——
 * 全程不需要联网，这是「冲突不落标记文件」设计的直接收益。
 */
export function conflictDetail(dir: string, path: string): ConflictDetail {
  const detail = native.wcConflictDetail(dir, path);
  return {
    path: detail.path,
    kind: detail.kind,
    base: decode(detail.base),
    mine: decode(detail.mine),
    theirs: decode(detail.theirs),
    baseHash: detail.baseHash,
    mineHash: detail.mineHash,
    theirsHash: detail.theirsHash,
    binary: looksBinary(detail.base) || looksBinary(detail.mine) || looksBinary(detail.theirs),
  };
}

/**
 * 解决一条冲突：
 *   - `mine`   保留磁盘现状（文件已删则等于接受删除）
 *   - `theirs` 用服务端内容覆盖（对方删除该路径时也会删掉本地文件）
 *   - `custom` 写入手工合并结果
 *   - `delete` 删除文件与条目
 */
export function resolveConflict(
  dir: string,
  path: string,
  choice: ConflictChoice,
  content?: string,
): void {
  native.wcResolveConflict(dir, path, choice, content === undefined ? undefined : Buffer.from(content, 'utf8'));
}

export function hasConflicts(dir: string): boolean {
  return native.wcHasConflicts(dir);
}
