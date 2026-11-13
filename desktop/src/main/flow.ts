/**
 * 单人提交闭环的编排层。
 *
 * 只做三件事：检出铺盘、采集变更并两阶段提交、提交后刷新本地基线。
 * 传输细节在 client.ts，磁盘语义在 wc.ts，这里负责把它们按**服务端的规则**串起来：
 *
 *   - 提交前先比对 head：本地落后就拒绝，由用户先 update（对应服务端严格 out-of-date）；
 *   - Begin 之后才知道要推哪些对象（服务端已有的不重传）；
 *   - Finalize 失败一律 Abort，避免留一个 2 小时 TTL 的半截会话。
 */
import * as fs from 'node:fs';
import * as path from 'node:path';

import { ErrorCode, VcsError, type ChangePayload, type VcsClient } from './client';
import * as wc from './wc';

export interface CheckoutResult {
  dir: string;
  rev: number;
  files: number;
}

export interface CommitResult {
  rev: number;
  added: number;
  modified: number;
  deleted: number;
  pushed: number;
  /** 实际写进历史的提交信息：用户留空时是自动生成的那一条。 */
  message: string;
}

/** 检出到目录：先拿树，再拉对象流，最后铺盘并写基线。 */
export async function checkoutRepo(
  client: VcsClient,
  repoId: string,
  dir: string,
  rev = 0,
): Promise<CheckoutResult> {
  const head = await client.getHead(repoId);
  const target = rev === 0 ? head.rev : rev;
  const entries = await client.getTree(repoId, target);
  const contents = await client.downloadObjects(repoId, target);
  const files = wc.checkout(
    dir,
    repoId,
    target,
    entries.map((entry) => ({ path: entry.path, hash: entry.hash, size: entry.size })),
    contents,
  );
  return { dir, rev: target, files };
}

/**
 * 提交信息留空时按变更自动生成一条。
 *
 * 单人提交里，写一句「改了什么」常常只是流程税：变更列表本身已经把事实摆在那里。
 * 界面上允许不填，这里就必须给出可追溯的兜底——历史里看到「修改 3 个文件：新增 1、
 * 修改 2」总好过一条空白记录。只有一个文件时直接写动作 + 路径，信息量最大。
 */
function autoCommitMessage(changes: wc.LocalChange[]): string {
  const label = { ADD: '新增', MODIFY: '修改', DELETE: '删除' } as const;
  const count = (kind: 'ADD' | 'MODIFY' | 'DELETE') =>
    changes.filter((change) => change.kind === kind).length;
  if (changes.length === 1) {
    const only = changes[0];
    return `${label[only.kind] ?? '变更'} ${only.path}`;
  }
  const parts: string[] = [];
  for (const kind of ['ADD', 'MODIFY', 'DELETE'] as const) {
    const n = count(kind);
    if (n > 0) parts.push(`${label[kind]} ${n}`);
  }
  return `提交 ${changes.length} 个文件（${parts.join('、')}）`;
}

/** 提交工作副本：采集 → 校验版本 → Begin → 推缺的对象 → Finalize → 刷新基线。 */
export async function commitWorkingCopy(
  client: VcsClient,
  dir: string,
  message: string,
): Promise<CommitResult> {
  // 冲突未解决一律拒绝：宁可挡住，也不猜用户想留哪一边（与 CLI 的 `vcs commit` 一致）。
  if (wc.hasConflicts(dir)) {
    const conflicts = wc
      .conflicts(dir)
      .map((item) => `${item.path}（${item.kind}）`)
      .join('、');
    throw new Error(`存在未解决的冲突：${conflicts}；请先还原或手工合并`);
  }

  const manifest = wc.openInfo(dir);
  const changes = wc.localChanges(dir);
  if (changes.length === 0) {
    throw new Error('工作副本没有变更');
  }
  // 提交信息可以不填：空串（或只有空白）按本次变更自动生成，界面上不必再拦一次。
  const subject = message.trim() || autoCommitMessage(changes);

  const head = await client.getHead(manifest.repoId);
  if (manifest.rev !== head.rev) {
    // 严格 out-of-date：服务端也会拦，但客户端先拦能给出「请先更新」的可操作提示。
    throw new VcsError(
      ErrorCode.OUT_OF_DATE,
      `本地 r${manifest.rev} 已落后于仓库 r${head.rev}，请先更新再提交`,
    );
  }

  // 内容在这里再读一次并校验哈希：扫描之后用户可能又改了文件。
  const payload: Array<{ hash: string; data: Buffer }> = [];
  let estimated = 0;
  for (const change of changes) {
    if (!change.hash) continue;
    const data = fs.readFileSync(path.join(dir, change.path));
    if (wc.hashBytes(data) !== change.hash) {
      throw new Error(`${change.path} 在扫描后又被修改，请重试`);
    }
    payload.push({ hash: change.hash, data });
    estimated += data.length;
  }

  const begin = await client.beginCommit(
    manifest.repoId,
    manifest.rev,
    changes as ChangePayload[],
    subject,
    estimated,
  );
  try {
    const missing = new Set(begin.missingHashes);
    const toPush = payload.filter((object) => missing.has(object.hash));
    if (toPush.length > 0) {
      await client.pushObjects(begin.sessionId, toPush);
    }
    const outcome = await client.finalizeCommit(begin.sessionId);
    wc.markCommitted(dir, outcome.rev, changes);
    return {
      rev: outcome.rev,
      added: changes.filter((c) => c.kind === 'ADD').length,
      modified: changes.filter((c) => c.kind === 'MODIFY').length,
      deleted: changes.filter((c) => c.kind === 'DELETE').length,
      pushed: toPush.length,
      message: subject,
    };
  } catch (err) {
    // 失败就显式放弃会话，别把半截上传留给 2 小时 TTL。
    await client.abortCommit(begin.sessionId).catch(() => undefined);
    throw err;
  }
}

/**
 * 更新工作副本到最新版本。
 *
 * 顺序很重要：先问服务端差异（Update），再按需拉对象（FetchObjects），最后交给
 * `vcs-core` 的 `apply_update` 合并。干净条目直接覆盖，本地改过的条目只记冲突、
 * 不动用户的文件——这与 CLI 的 `vcs update` 是同一段逻辑。
 */
export async function updateWorkingCopy(
  client: VcsClient,
  dir: string,
): Promise<wc.UpdateOutcome> {
  const info = wc.openInfo(dir);
  const head = await client.getHead(info.repoId);
  // 用「最旧条目版本」而不是 head 版本的副本做比较：revert 过的条目仍停在旧基线，
  // 只看 rev 会得出「已经最新」，那部分差异就永远拉不回来。
  if (head.rev === info.oldestRev) {
    throw new Error(`已是最新版本 r${head.rev}`);
  }
  if (head.rev < info.oldestRev) {
    throw new Error(`本地 r${info.oldestRev} 比服务端 r${head.rev} 还新，请重新检出`);
  }

  const delta = await client.update(info.repoId, info.oldestRev, head.rev);
  const needed = delta.changes.map((change) => change.hash).filter((h): h is string => !!h);
  const objects = await client.fetchObjects([...new Set(needed)]);
  const outcome = wc.applyUpdate(dir, info.repoId, head.rev, delta.changes, objects);
  // 这次变更集覆盖整个仓库（Update 未做路径过滤），所以没出现在差异里的条目
  // 一定与目标版本一致，可以安全地把它们的基线版本推进到 head。
  wc.advanceBase(dir, head.rev);
  return outcome;
}

/** 撤销本地改动（空数组 = 全部），冲突标记一并清除。 */
export function revertWorkingCopy(dir: string, paths: string[] = []): number {
  return wc.revert(dir, paths);
}
