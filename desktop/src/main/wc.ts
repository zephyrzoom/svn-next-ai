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
 * **原生调用一律异步**：`vcs-napi` 里每个动盘的函数都返回 Promise（内部走 `AsyncTask`），
 * 写入 / 哈希几 GB 的文件只占后台线程，Electron 主进程不会被几十秒的磁盘操作钉住。
 * 代价是「同步调用天然串行」这层保护没了，所以下面按目录排一次队（`serial`）。
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
  /** 稀疏检出路径前缀；空数组 = 全库检出。老副本读不到时按全量处理。 */
  sparseRules: string[];
}

export interface SparseOutcome {
  added: number;
  removed: number;
  /** 被顺带清掉的空目录数。 */
  prunedDirs: number;
  /** 里面还有未纳入版本控制的文件、被原样保留的目录。 */
  keptDirs: string[];
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
  wcInfo(dir: string): Promise<string>;
  hashFileHex(path: string): Promise<string>;
  wcCheckout(
    dir: string,
    repoId: string,
    rev: number,
    entriesJson: string,
    spoolDir: string,
    rulesJson: string,
  ): Promise<number>;
  wcApplySparseRules(
    dir: string,
    rulesJson: string,
    rev: number,
    entriesJson: string,
    spoolDir: string,
  ): Promise<string>;
  wcStatus(dir: string): Promise<string>;
  wcDiff(dir: string): Promise<string>;
  wcLocalChanges(dir: string): Promise<string>;
  wcMarkCommitted(dir: string, rev: number, changesJson: string): Promise<void>;
  wcApplyUpdate(
    dir: string,
    repoId: string,
    targetRev: number,
    changesJson: string,
    spoolDir: string,
  ): Promise<string>;
  wcAdvanceBase(dir: string, rev: number): Promise<number>;
  wcRevert(dir: string, pathsJson: string): Promise<number>;
  wcConflicts(dir: string): Promise<string>;
  wcConflictDetail(dir: string, path: string): Promise<{
    path: string;
    kind: string;
    baseHash: string | null;
    mineHash: string | null;
    theirsHash: string | null;
    base: Buffer | null;
    mine: Buffer | null;
    theirs: Buffer | null;
  }>;
  wcResolveConflict(dir: string, path: string, choice: string, content?: Buffer): Promise<void>;
  /** 唯一保持同步的查询：一次 EXISTS，等线程池往返反而更贵。 */
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

/**
 * 同一份工作副本上的原生调用排队执行。
 *
 * 同步调用时代，主线程本身就是那把锁：一段代码没跑完，下一段没机会进来。改成异步之后，
 * 两段代码可以同时动同一个目录——它们会各自 open 一份 `wc.db`（写事务互相 SQLITE_BUSY），
 * 更糟的是「更新写到一半」时被另一段读到半成品。界面上虽然有「一次只允许一个动盘操作」
 * 的锁挡住重复点击，但后台自动刷新、多窗口、以后新增的调用点都可能绕过去，
 * 所以真正的互斥放在这里：**按目录排队**，不同目录之间互不等待。
 */
const queues = new Map<string, Promise<unknown>>();

async function serial<T>(dir: string, run: () => Promise<T>): Promise<T> {
  // 同一个目录的不同写法（多余的 `..`、结尾斜杠）其实是同一份副本，先归一化再排队。
  const id = path.resolve(dir);
  const prev = queues.get(id) ?? Promise.resolve();
  // 前一个失败了不等于后一个也要失败：成功与失败两条分支都接着往下跑。
  const task = prev.then(run, run);
  const tail = task.then(
    () => undefined,
    () => undefined,
  );
  queues.set(id, tail);
  void tail.then(() => {
    // 队列空了就把这条目录摘掉，长年开着的窗口不该在这里越攒越多。
    if (queues.get(id) === tail) queues.delete(id);
  });
  return task;
}

/** BLAKE3-256 十六进制摘要，与服务端同一实现。 */
export function hashBytes(data: Buffer): string {
  return native.hashBytesHex(data);
}

/**
 * 流式算文件摘要与字节数（原生侧边读边算，Promise）。
 *
 * 提交前的「扫描之后有没有再改过」校验用它：不需要把文件读进 JS 内存，
 * 几个 GB 的文件也一样——读盘那几分钟在后台线程里，界面照常能用。
 */
export async function hashFile(file: string): Promise<{ hash: string; size: number }> {
  return JSON.parse(await native.hashFileHex(file)) as { hash: string; size: number };
}

/** 工作副本归属与基线版本。 */
export async function openInfo(dir: string): Promise<WorkingCopyInfo> {
  return JSON.parse(await serial(dir, () => native.wcInfo(dir))) as WorkingCopyInfo;
}

/**
 * 检出：把服务端的 (路径 → 内容哈希) 与对象内容铺到磁盘，并写 pristine 基线。
 *
 * 内容来自 `spoolDir`（CAS 布局的下载暂存目录），native 侧流式读——内存与仓库大小无关。
 * 同一份内容被多路径引用时只落一份基线。
 */
export async function checkout(
  dir: string,
  repoId: string,
  rev: number,
  entries: CheckoutEntry[],
  spoolDir: string,
  rules: string[] = [],
): Promise<number> {
  return serial(dir, () =>
    native.wcCheckout(
      dir,
      repoId,
      rev,
      JSON.stringify(entries),
      spoolDir,
      JSON.stringify(rules),
    ),
  );
}

/**
 * 调整稀疏规则：把新增前缀下的文件与内容交给 core 增量铺盘 / 移除。
 * 规则之外的干净条目会被删掉；有本地改动时 core 直接报错，一个文件都不会动。
 */
export async function applySparseRules(
  dir: string,
  rules: string[],
  rev: number,
  entries: CheckoutEntry[],
  spoolDir: string,
): Promise<SparseOutcome> {
  const raw = await serial(dir, () =>
    native.wcApplySparseRules(dir, JSON.stringify(rules), rev, JSON.stringify(entries), spoolDir),
  );
  return JSON.parse(raw) as SparseOutcome;
}

/** 扫描工作副本（mtime 快路径 + 改动条目重算哈希）。 */
export async function status(dir: string): Promise<StatusEntry[]> {
  return JSON.parse(await serial(dir, () => native.wcStatus(dir))) as StatusEntry[];
}

/** 离线 diff：基线 vs 磁盘。 */
export async function diff(dir: string): Promise<DiffEntry[]> {
  return JSON.parse(await serial(dir, () => native.wcDiff(dir))) as DiffEntry[];
}

/** 待提交变更（含内容地址）。 */
export async function localChanges(dir: string): Promise<LocalChange[]> {
  return JSON.parse(await serial(dir, () => native.wcLocalChanges(dir))) as LocalChange[];
}

/** 提交成功后刷新基线并推进版本。 */
export async function markCommitted(
  dir: string,
  rev: number,
  changes: LocalChange[],
): Promise<void> {
  await serial(dir, () => native.wcMarkCommitted(dir, rev, JSON.stringify(changes)));
}

/** 应用服务端差异（update）。冲突不覆盖本地内容，只记元数据。 */
export async function applyUpdate(
  dir: string,
  repoId: string,
  targetRev: number,
  changes: LocalChange[],
  spoolDir: string,
): Promise<UpdateOutcome> {
  // 内容已由调用方下载到 spool 目录，这里只给路径：native 流式读。
  const raw = await serial(dir, () =>
    native.wcApplyUpdate(dir, repoId, targetRev, JSON.stringify(changes), spoolDir),
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
export async function advanceBase(dir: string, rev: number): Promise<number> {
  return serial(dir, () => native.wcAdvanceBase(dir, rev));
}

/** 撤销本地改动（空数组表示全部）。未纳入版本控制的文件不会被删除。 */
export async function revert(dir: string, paths: string[] = []): Promise<number> {
  return serial(dir, () => native.wcRevert(dir, JSON.stringify(paths)));
}

export async function conflicts(dir: string): Promise<Array<{ path: string; kind: string }>> {
  return JSON.parse(await serial(dir, () => native.wcConflicts(dir))) as Array<{
    path: string;
    kind: string;
  }>;
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
export async function conflictDetail(dir: string, file: string): Promise<ConflictDetail> {
  const detail = await serial(dir, () => native.wcConflictDetail(dir, file));
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
export async function resolveConflict(
  dir: string,
  file: string,
  choice: ConflictChoice,
  content?: string,
): Promise<void> {
  await serial(dir, () =>
    native.wcResolveConflict(
      dir,
      file,
      choice,
      content === undefined ? undefined : Buffer.from(content, 'utf8'),
    ),
  );
}

export function hasConflicts(dir: string): boolean {
  return native.wcHasConflicts(dir);
}
