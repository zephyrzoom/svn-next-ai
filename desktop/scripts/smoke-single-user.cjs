#!/usr/bin/env node
/**
 * 单人/双人提交闭环的端到端冒烟（不需要 Electron）。
 *
 * 它做的事和 GUI 完全一样，只是没有界面：
 *   起真实服务端 → 管理员 bootstrap → 登录 → 建库 → 检出
 *   → 改文件 → 扫描 → 离线 diff → 两阶段提交
 *   → 用 CLI 从「另一处」再提交一次（模拟别人先提交）
 *   → 更新（Update + FetchObjects + apply_update）→ 再改同一文件造冲突
 *   → 冲突后提交被拒 → 还原清冲突
 *
 * 为什么值得单独写：`npm run typecheck` 只能证明类型对，Electron 的 UI 又跑不进 CI。
 * 这个脚本把「主进程里的客户端逻辑 + napi 工作副本」放进真实网络与真实文件系统里跑一遍。
 *
 * 用法：node scripts/smoke-single-user.cjs
 */
'use strict';

const { spawn, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const BIN = path.join(ROOT, 'target', 'debug');
const WORK = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-desktop-smoke-'));
const STORE = path.join(WORK, 'store');
const WC = path.join(WORK, 'wc');
const OTHER = path.join(WORK, 'other');
const PORT = 51000 + (process.pid % 1000);
const TARGET = `127.0.0.1:${PORT}`;
const PASSWORD = 'Abcdef1!';

function step(message) {
  process.stdout.write(`\n▶ ${message}\n`);
}

function fail(message) {
  process.stderr.write(`\n✗ ${message}\n`);
  throw new Error(message);
}

/** 用 CLI 直连存储根目录提交一次，模拟「另一个人先提交了」。 */
function cliCommit(message, repo = 'reports', dir = OTHER) {
  const result = spawnSync(
    path.join(BIN, 'vcs'),
    ['import', repo, dir, '-m', message, '--store', STORE],
    { encoding: 'utf8', env: { ...process.env, HOME: path.join(WORK, 'home') } },
  );
  if (result.status !== 0) fail(`CLI 提交失败：${result.stderr || result.stdout}`);
  const match = /已提交 r(\d+)/.exec(result.stdout);
  return match ? Number(match[1]) : 0;
}

function waitForServer(child) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('服务端启动超时')), 15000);
    let buffer = '';
    child.stdout.on('data', (chunk) => {
      buffer += chunk.toString();
      if (buffer.includes('vcs serve 已启动')) {
        clearTimeout(timer);
        resolve();
      }
    });
    child.stderr.on('data', (chunk) => process.stderr.write(chunk));
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`服务端提前退出（code=${code}）`));
    });
  });
}

async function main() {
  step('bootstrap 管理员');
  const init = spawnSync(
    path.join(BIN, 'vcs'),
    ['init-admin', '--store', STORE, '--username', 'alice', '--name', '张爱丽', '--password', PASSWORD],
    { encoding: 'utf8' },
  );
  if (init.status !== 0) fail(`init-admin 失败：${init.stderr || init.stdout}`);

  // CLI 侧需要登录态（同样的 HOME），模拟「另一个人」
  const login = spawnSync(
    path.join(BIN, 'vcs'),
    ['login', '--store', STORE, '--username', 'alice', '--password', PASSWORD],
    { encoding: 'utf8', env: { ...process.env, HOME: path.join(WORK, 'home') } },
  );
  if (login.status !== 0) fail(`CLI 登录失败：${login.stderr || login.stdout}`);

  step(`启动 gRPC 服务 ${TARGET}`);
  const server = spawn(
    path.join(BIN, 'vcs'),
    // 冒烟只验 gRPC / CLI 链路；关掉内置 Web 管理端，避免占用固定 8080 端口。
    ['serve', '--store', STORE, '--listen', TARGET, '--no-admin'],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  await waitForServer(server);

  const { VcsClient } = require('../dist/main/client.js');
  const { hashBytes: vcsHash } = require('../dist/main/wc.js');
  const flow = require('../dist/main/flow.js');
  const wc = require('../dist/main/wc.js');
  const client = new VcsClient(TARGET);

  try {
    step('登录并建库');
    const user = await client.login('alice', PASSWORD);
    if (user.username !== 'alice') fail(`登录返回的用户不对：${JSON.stringify(user)}`);
    const repo = await client.createRepo('reports');
    console.log(`  ${repo.name} = ${repo.repoId}`);

    step('检出 r0（napi 写 wc.db + pristine）');
    const result = await flow.checkoutRepo(client, repo.repoId, WC);
    const info = wc.openInfo(WC);
    if (info.repoId !== repo.repoId || info.rev !== 0) {
      fail(`工作副本归属不对：${JSON.stringify(info)}`);
    }
    console.log(`  检出 r${result.rev}，${result.files} 个文件；wc.db 归属校验通过`);

    step('首次提交');
    fs.mkdirSync(path.join(WC, '202610'), { recursive: true });
    fs.writeFileSync(path.join(WC, '202610', 'summary.txt'), '十月汇总\n第二行\n');
    const status = wc.status(WC);
    if (status.length !== 1 || status[0].kind !== 'ADD') {
      fail(`变更识别不对：${JSON.stringify(status)}`);
    }
    const diffs = wc.diff(WC);
    if (diffs.length !== 1 || !diffs[0].text.includes('+十月汇总')) {
      fail(`diff 不对：${JSON.stringify(diffs)}`);
    }
    console.log(diffs[0].text.trim().split('\n').map((line) => `  ${line}`).join('\n'));
    const first = await flow.commitWorkingCopy(client, WC, '首次提交');
    if (first.rev !== 1 || first.pushed !== 1) fail(`提交结果不对：${JSON.stringify(first)}`);
    if (wc.status(WC).length !== 0) fail('提交后本地仍有变更');
    console.log(`  已提交 r${first.rev}，本地干净`);

    step('另一个人先提交（CLI import）→ 本地落后');
    fs.mkdirSync(path.join(OTHER, '202610'), { recursive: true });
    fs.writeFileSync(path.join(OTHER, '202610', 'summary.txt'), '十月汇总\n第二行\n');
    fs.writeFileSync(path.join(OTHER, '202610', 'extra.txt'), '别人加的文件\n');
    const remoteRev = cliCommit('别人加了个文件');
    console.log(`  服务端已到 r${remoteRev}`);

    let staleRejected = false;
    try {
      fs.writeFileSync(path.join(WC, '202610', 'local.txt'), '本地文件\n');
      await flow.commitWorkingCopy(client, WC, '落后时的提交');
    } catch (err) {
      staleRejected = true;
      if (!String(err.message).includes('落后于仓库')) fail(`错误文案不对：${err.message}`);
      console.log(`  已按预期拒绝：${err.message}`);
    }
    if (!staleRejected) fail('落后于服务端时竟然提交成功');

    step('更新到最新版本');
    // 先把刚才那个本地新增文件删掉，避免它干扰后面的冲突场景
    fs.rmSync(path.join(WC, '202610', 'local.txt'));
    const updated = await flow.updateWorkingCopy(client, WC);
    if (updated.updated + updated.added + updated.removed === 0) {
      fail(`更新没有产生任何变化：${JSON.stringify(updated)}`);
    }
    if (!fs.existsSync(path.join(WC, '202610', 'extra.txt'))) fail('更新后没拿到别人的新文件');
    if (wc.openInfo(WC).rev !== remoteRev) fail('更新后本地版本没有推进');
    console.log(
      `  已更新到 r${updated.rev}：更新 ${updated.updated}，新增 ${updated.added}，删除 ${updated.removed}`,
    );

    step('造冲突：本地与远端同时改同一个文件');
    fs.writeFileSync(path.join(WC, '202610', 'summary.txt'), '本地改的\n');
    fs.writeFileSync(path.join(OTHER, '202610', 'summary.txt'), '远端改的\n');
    const conflictRev = cliCommit('远端改了同一文件');
    const conflicted = await flow.updateWorkingCopy(client, WC);
    if (conflicted.conflicts + conflicted.treeConflicts === 0) {
      fail(`没有产生冲突：${JSON.stringify(conflicted)}`);
    }
    const localContent = fs.readFileSync(path.join(WC, '202610', 'summary.txt'), 'utf8');
    if (localContent !== '本地改的\n') fail('冲突时本地内容被覆盖了');
    const conflictList = wc.conflicts(WC);
    if (conflictList.length === 0) fail('冲突列表为空');
    console.log(
      `  发现 ${conflicted.conflicts} 处内容冲突；本地内容保留；冲突路径 ${conflictList
        .map((item) => item.path)
        .join(',')}（服务端 r${conflictRev}）`,
    );

    step('带冲突提交必须被拒');
    let blocked = false;
    try {
      await flow.commitWorkingCopy(client, WC, '带冲突的提交');
    } catch (err) {
      blocked = true;
      if (!String(err.message).includes('冲突')) fail(`错误文案不对：${err.message}`);
      console.log(`  已按预期拒绝：${err.message}`);
    }
    if (!blocked) fail('带冲突的提交竟然成功');

    step('三栏合并：读 base/mine/theirs → 写入合并结果 → 提交');
    const detail = wc.conflictDetail(WC, '202610/summary.txt');
    if (detail.kind !== 'text') fail(`冲突类型不对：${detail.kind}`);
    if (detail.base !== '十月汇总\n第二行\n') fail(`base 不对：${JSON.stringify(detail.base)}`);
    if (detail.mine !== '本地改的\n') fail(`mine 不对：${JSON.stringify(detail.mine)}`);
    if (detail.theirs !== '远端改的\n') fail(`theirs 不对：${JSON.stringify(detail.theirs)}`);
    console.log('  三方内容都能离线读到（theirs 来自冲突时缓存进 pristine 的那份）');

    wc.resolveConflict(WC, '202610/summary.txt', 'custom', '合并结果：本地+远端\n');
    if (wc.hasConflicts(WC)) fail('解决后仍有冲突');
    const mergedStatus = wc.status(WC);
    if (mergedStatus.length !== 1 || mergedStatus[0].kind !== 'MODIFY') {
      fail(`解决后应显示为待提交的修改：${JSON.stringify(mergedStatus)}`);
    }
    const mergeCommit = await flow.commitWorkingCopy(client, WC, '合并冲突');
    const mergedHash = vcsHash(Buffer.from('合并结果：本地+远端\n'));
    const mergedObjects = await client.downloadObjects(repo.repoId, mergeCommit.rev);
    const mergedRemote = mergedObjects.get(mergedHash);
    if (!mergedRemote || mergedRemote.toString('utf8') !== '合并结果：本地+远端\n') {
      fail('服务端没有收到合并结果');
    }
    console.log(`  已提交 r${mergeCommit.rev}，服务端内容与合并结果一致`);

    step('第二次冲突：验证「还原」路径');
    fs.writeFileSync(path.join(WC, '202610', 'summary.txt'), '本地第二次\n');
    fs.writeFileSync(path.join(OTHER, '202610', 'summary.txt'), '远端第二次\n');
    const secondConflictRev = cliCommit('远端第二次改动');
    const secondConflict = await flow.updateWorkingCopy(client, WC);
    if (secondConflict.conflicts + secondConflict.treeConflicts === 0) {
      fail('第二次没有产生冲突');
    }
    if (wc.hasConflicts(WC) !== true) fail('冲突状态与列表不一致');

    const restored = flow.revertWorkingCopy(WC, []);
    if (restored === 0) fail('还原没有恢复任何条目');
    if (wc.hasConflicts(WC)) fail('还原后仍有冲突');
    const afterRevert = fs.readFileSync(path.join(WC, '202610', 'summary.txt'), 'utf8');
    console.log(`  已还原 ${restored} 个条目；summary.txt = ${JSON.stringify(afterRevert)}`);

    step('还原后再更新：混合版本必须能补回差异');
    // 还原把 summary.txt 拉回了合并提交的基线，而服务端又改过它：
    // 只看 head 会得出「已经最新」，必须用 oldestRev 取差异才能补齐。
    const beforeFix = wc.openInfo(WC);
    if (beforeFix.oldestRev >= secondConflictRev) {
      fail(`还原后 oldestRev 应落后于 head：${JSON.stringify(beforeFix)}`);
    }
    const fixed = await flow.updateWorkingCopy(client, WC);
    const fixedContent = fs.readFileSync(path.join(WC, '202610', 'summary.txt'), 'utf8');
    if (fixedContent !== '远端第二次\n') {
      fail(`还原后 update 没补回服务端内容，实际 ${JSON.stringify(fixedContent)}`);
    }
    const afterFix = wc.openInfo(WC);
    if (afterFix.oldestRev !== secondConflictRev) {
      fail(`补齐后 oldestRev 应等于 head r${secondConflictRev}：${JSON.stringify(afterFix)}`);
    }
    console.log(
      `  已补齐 ${fixed.updated} 个条目；oldestRev 追平到 r${afterFix.oldestRev}`,
    );

    step('恢复干净状态');
    const after = wc.status(WC);
    if (after.length !== 0) fail(`更新后仍有变更：${JSON.stringify(after)}`);

    step('注册（自带密码）→ 审批：非管理员也能自己开通');
    const registered = await client.register('bob', '李波', 'Bobpass1!', '一起维护月度报表');
    console.log(`  已提交注册申请 ${registered.requestId}`);
    // 这个库启动时已用 init-admin 建过管理员，所以 bob 不是首位用户，必须走审批。
    if (registered.autoApproved) fail('已有管理员时注册不应被自动批准');
    const pending = await client.listRegistrations(1);
    if (!pending.some((item) => item.username === 'bob')) fail('审批台里看不到 bob 的申请');
    const bobClient = new VcsClient(TARGET);
    // 未审批前密码无效：不能登录，且错误该是「待审批」而不是「密码错」。
    let pendingBlocked = false;
    try {
      await bobClient.login('bob', 'Bobpass1!');
    } catch (err) {
      pendingBlocked = true;
      if (!String(err.message).includes('待审批')) fail(`未审批登录应提示待审批：${err.message}`);
    }
    if (!pendingBlocked) fail('未审批的账号竟然可以登录');

    await client.approveRegistration(registered.requestId, 2);
    const bob = await bobClient.login('bob', 'Bobpass1!');
    console.log(`  bob 已能登录（role=${bob.role}），用的是注册时设置的密码`);

    try {
      const request = await bobClient.requestRepo('bob-repo', '个人归档');
      const bobReposBefore = await bobClient.listRepos();
      if (bobReposBefore.some((item) => item.name === 'bob-repo')) {
        fail('未审批的仓库不该出现在列表里');
      }
      const requests = await client.listRepoCreations(1);
      if (!requests.some((item) => item.requestId === request.requestId)) {
        fail('审批台里看不到建库申请');
      }
      const created = await client.approveRepoCreation(request.requestId, 0, 0, '同意');
      const bobRepos = await bobClient.listRepos();
      if (!bobRepos.some((item) => item.repoId === created.repoId)) {
        fail('审批通过后申请人看不到自己的仓库');
      }
      const head = await bobClient.getHead(created.repoId);
      console.log(`  仓库 ${created.name} 已创建，bob 可访问（head=r${head.rev}）`);

      step('成员管理：owner 加人 → 改角色 → 移除');
      const aliceCandidate = (await bobClient.searchUsers('alice'))[0];
      if (!aliceCandidate) fail('搜不到 alice');
      // 加人前：非成员读不到仓库
      let deniedBefore = false;
      try {
        await client.getHead(created.repoId);
      } catch (err) {
        deniedBefore = err.code === 28;
      }
      if (!deniedBefore) fail('非成员竟然能读仓库');

      await bobClient.addMember(created.repoId, aliceCandidate.userId, 3); // writer
      const members = await bobClient.listMembers(created.repoId);
      const aliceMember = members.find((item) => item.userId === aliceCandidate.userId);
      if (!aliceMember || aliceMember.role !== 3) fail('加成员后的角色不对');
      await client.getHead(created.repoId); // 加人后 alice 可读
      console.log('  已加 alice 为 writer，alice 可正常读取');

      await bobClient.setMemberRole(created.repoId, aliceCandidate.userId, 4); // reader
      const afterRole = (await bobClient.listMembers(created.repoId)).find(
        (item) => item.userId === aliceCandidate.userId,
      );
      if (!afterRole || afterRole.role !== 4) fail('改角色没生效');
      console.log('  已把 alice 降为 reader');

      // owner 受保护：bob 不能移除自己
      let ownerProtected = false;
      try {
        await bobClient.removeMember(created.repoId, bob.userId);
      } catch (err) {
        ownerProtected = err.code === 31;
      }
      if (!ownerProtected) fail('owner 竟然可以移除自己');

      await bobClient.removeMember(created.repoId, aliceCandidate.userId);
      let deniedAfter = false;
      try {
        await client.getHead(created.repoId);
      } catch (err) {
        deniedAfter = err.code === 28;
      }
      if (!deniedAfter) fail('移除成员后仍能读仓库');
      console.log('  已移除 alice，权限立刻收回；owner 不可移除自己');
    } finally {
      bobClient.close();
    }

    step('校验服务端历史');
    const revisions = await client.getLog(repo.repoId, 10);
    const head = await client.getHead(repo.repoId);
    if (head.rev !== secondConflictRev) {
      fail(`head 应为 r${secondConflictRev}，实际 r${head.rev}`);
    }
    console.log(`  head = r${head.rev}，历史 ${revisions.length} 条`);

    step('提交信息留空：按本次变更自动生成');
    fs.writeFileSync(path.join(WC, '202610', 'auto.txt'), '自动提交信息\n');
    // 界面上允许不填：主进程必须补一条可追溯的信息，而不是把空串写进历史。
    const auto = await flow.commitWorkingCopy(client, WC, '   ');
    const autoLog = await client.getLog(repo.repoId, 1);
    if (!autoLog[0] || !String(autoLog[0].message).includes('auto.txt')) {
      fail(`留空时应写入自动生成的信息：${JSON.stringify(autoLog[0])}`);
    }
    if (!String(auto.message).includes('auto.txt')) {
      fail(`返回值里应带上实际写入的信息：${JSON.stringify(auto)}`);
    }
    if (wc.status(WC).length !== 0) fail('留空提交后本地仍有变更');
    console.log(`  留空时写入的历史：${autoLog[0].message}（r${auto.rev}）`);

    step('空间治理：预估 → 清理 → GC');
    const quotaBefore = await client.getQuota(repo.repoId);
    if (quotaBefore.usedBytes <= 0) fail('对象池占用不该是 0');
    const candidates = await client.listPurgeCandidates(repo.repoId, 10);
    const october = candidates.find((item) => item.path === '202610');
    if (!october) fail(`可清理目录里没有 202610：${JSON.stringify(candidates)}`);
    console.log(
      `  配额 ${quotaBefore.usedBytes} 字节（来源 ${quotaBefore.source}）；` +
        `202610 独占 ${october.uniqueBytes} / 逻辑 ${october.logicalBytes}`,
    );

    const estimate = await client.estimatePurge(repo.repoId, ['202610']);
    if (estimate.reclaimableBytes <= 0) fail('预估可释放为 0，说明独占计算有问题');
    if (estimate.affectedRevisions < 1) fail('影响版本数不该为 0');
    console.log(
      `  预估可释放 ${estimate.reclaimableBytes} 字节，影响 ${estimate.affectedRevisions} 个版本（dry-run 未改动数据）`,
    );

    step('回收站：先放宽限期 → 撤销 → 内容恢复');
    const staged = await client.purge(repo.repoId, ['202610'], false, '先放回收站');
    const bin = await client.listPurgedPaths(repo.repoId);
    const stagedRecord = bin.find((item) => item.purgeId && item.state === 'pending');
    if (!stagedRecord) fail(`回收站里没有宽限期记录：${JSON.stringify(bin)}`);
    const restoredRev = await client.revertPurge(repo.repoId, stagedRecord.purgeId);
    if (restoredRev <= staged.newRev) fail('撤销应产生新的提交');
    const restoredTree = await client.getTree(repo.repoId, 0);
    if (!restoredTree.some((entry) => entry.path === '202610/summary.txt')) {
      fail('撤销后 HEAD 里应重新出现 202610');
    }
    const restoredEntry = restoredTree.find((entry) => entry.path === '202610/summary.txt');
    const restoredObjs = await client.downloadObjects(repo.repoId, 0);
    const restoredData = restoredObjs.get(restoredEntry.hash);
    if (!restoredData || restoredData.toString('utf8') !== '远端第二次\n') {
      fail(`撤销后内容不对：${JSON.stringify(restoredData && restoredData.toString('utf8'))}`);
    }
    const afterRevertBin = await client.listPurgedPaths(repo.repoId);
    if (!afterRevertBin.some((item) => item.state === 'reverted')) {
      fail('撤销后记录状态应为 reverted');
    }
    console.log(`  宽限期清理 → 撤销成 r${restoredRev}，内容与当初一致，记录标为已撤销`);

    step('立即释放后再撤销：必须诚实地失败');
    const purged = await client.purge(repo.repoId, ['202610'], true, '冒烟清理');
    if (purged.newRev <= head.rev) fail('清理应产生新的删除提交');
    const gc = await client.getGcStatus(repo.repoId, purged.gcJobId);
    if (gc.state !== 4) fail(`GC 状态应为 DONE(4)，实际 ${gc.state}`);
    if (gc.reclaimedBytes <= 0) fail('GC 没有真正释放空间');
    console.log(
      `  已生成 r${purged.newRev} 并完成 GC：清扫 ${gc.sweptObjects} 个对象，释放 ${gc.reclaimedBytes} 字节`,
    );

    const tree = await client.getTree(repo.repoId, 0);
    if (tree.some((entry) => entry.path.startsWith('202610/'))) fail('清理后 HEAD 仍含 202610');
    const quotaAfter = await client.getQuota(repo.repoId);
    if (quotaAfter.usedBytes > quotaBefore.usedBytes) fail('清理后占用反而变大了');
    console.log(`  HEAD 已不含 202610；对象池占用 ${quotaBefore.usedBytes} → ${quotaAfter.usedBytes}`);

    const bin2 = await client.listPurgedPaths(repo.repoId);
    const sweptRecord = bin2.find((item) => item.state === 'active');
    if (!sweptRecord) fail('回收站里没有已生效的记录');
    let contentGone = false;
    try {
      await client.revertPurge(repo.repoId, sweptRecord.purgeId);
    } catch (err) {
      contentGone = err.code === 7; // CONTENT_PURGED
      if (!contentGone) fail(`撤销应报 CONTENT_PURGED(7)，实际 ${err.code}：${err.message}`);
      console.log(`  已按预期失败：${err.message}`);
    }
    if (!contentGone) fail('内容已被回收却撤销成功，说明服务端在伪造内容');

    step('配额设置：全局默认 → 单仓库覆盖 → 恢复继承');
    // 包一层块作用域：main() 是个大函数，前面的段落已经用过 quotaBefore 之类的名字。
    {
    const quotaBefore = await client.getQuotaDefaults();
    const quotaSaved = await client.updateQuotaDefaults(2 * 1024 * 1024);
    if (quotaSaved.bytes !== 2 * 1024 * 1024) {
      fail(`全局默认没有被保存：${JSON.stringify(quotaSaved)}`);
    }
    const inheritedQuota = await client.getRepoQuota(repo.repoId);
    if (inheritedQuota.source !== 2) {
      fail(`未核定仓库应继承全局默认(2)，实际 ${inheritedQuota.source}`);
    }
    const overrideQuota = await client.setRepoQuota(repo.repoId, 1024 * 1024, false);
    if (overrideQuota.source !== 1 || overrideQuota.effectiveBytes !== 1024 * 1024) {
      fail(`单仓库覆盖没生效：${JSON.stringify(overrideQuota)}`);
    }
    const backToInherit = await client.setRepoQuota(repo.repoId, 0, true);
    if (backToInherit.source === 1) fail('恢复继承后来源不该还是单仓库覆盖');
    console.log(
      `  全局默认 ${quotaBefore.bytes} → ${quotaSaved.bytes} 字节；` +
        `单仓库覆盖生效后恢复继承（来源 ${inheritedQuota.source} → 1 → ${backToInherit.source}）`,
    );
    }

    step('保留策略：配置 → 预览 → 执行');
    const archive = await client.createRepo('archive');
    const ARCH = path.join(WORK, 'archive-src');
    for (const month of ['202609', '202610', '202611']) {
      fs.mkdirSync(path.join(ARCH, month), { recursive: true });
      fs.writeFileSync(path.join(ARCH, month, 'report.txt'), `${month} 的报告\n`);
    }
    cliCommit('三个月归档', 'archive', ARCH);

    const saved = await client.upsertRetentionPolicy(archive.repoId, {
      pathPattern: '^20\\d{4}$',
      keepMonths: 2,
      maxBytes: 0,
      maxAgeDays: 0,
      enabled: true,
      dryRun: true,
    });
    if (!saved.policyId.startsWith('rt_')) fail('策略号格式不对');
    const listed = await client.listRetentionPolicies(archive.repoId);
    if (listed.length !== 1 || !listed[0].dryRun) fail('策略没有正确保存为仅预览');

    const preview = await client.applyRetention(archive.repoId, true, false);
    if (preview.candidates.join(',') !== '202609') {
      fail(`候选不对：${JSON.stringify(preview.candidates)}`);
    }
    if (preview.newRev !== 0) fail('预览不该产生提交');
    console.log(`  预览候选 ${preview.candidates.join('、')}，预计释放 ${preview.reclaimableBytes} 字节`);

    // 策略本身是「仅预览」：即使请求执行也必须被拒。
    let blockedByDryRun = false;
    try {
      await client.applyRetention(archive.repoId, false, false);
    } catch (err) {
      blockedByDryRun = err.code === 36; // INVALID_ARGUMENT
      if (!blockedByDryRun) fail(`应以 INVALID_ARGUMENT 拒绝：${err.code} ${err.message}`);
    }
    if (!blockedByDryRun) fail('仅预览的策略竟然被执行了');

    const enabled = await client.upsertRetentionPolicy(archive.repoId, {
      policyId: saved.policyId,
      pathPattern: '^20\\d{4}$',
      keepMonths: 2,
      maxBytes: 0,
      maxAgeDays: 0,
      enabled: true,
      dryRun: false,
    });
    if (enabled.dryRun) fail('策略没有被切换为可执行');
    const applied = await client.applyRetention(archive.repoId, false, true);
    if (applied.newRev <= 1) fail('执行应该产生新的删除提交');
    const archiveTree = await client.getTree(archive.repoId, 0);
    if (archiveTree.some((entry) => entry.path.startsWith('202609/'))) {
      fail('清理后 HEAD 里仍有 202609');
    }
    const again = await client.applyRetention(archive.repoId, false, true);
    if (again.candidates.length !== 0) fail('重复执行不该再有候选');
    console.log(`  已执行：生成 r${applied.newRev}，只剩最近两个月份；再次评估无候选`);

    console.log(
      '\n✓ 提交/更新/冲突三栏/还原 + 注册审批 + 建库审批 + 成员管理 + 空间治理 + 回收站 + 保留策略 + 配额设置 冒烟通过',
    );
  } finally {
    client.close();
    server.kill();
    fs.rmSync(WORK, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
