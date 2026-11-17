#!/usr/bin/env node
/**
 * 本机设置「按账号隔离」的回归（可选，本地执行）：真的用 Electron 加载主进程产物，
 * 在临时 userData 目录里反复读写，验证 A / B 的工作副本登记互不串台。
 *
 * 为什么需要它：这类 bug 只靠 typecheck 与界面冒烟都发现不了——界面拿到的永远只是
 * 主进程给的那一份数据，串台发生在 `settings.json` 的键上。曾经的做法是「一个全局
 * `repoDirs`，跟着机器走」，于是 A 在 s1、B 在 s2 检出同一个仓库后，切回 A 看到的是 s2。
 *
 * 单文件双模式（与 smoke-ui.cjs 一致）：
 *   `node scripts/smoke-accounts.cjs`        → require('electron') 拿到二进制路径，负责拉起；
 *   `electron scripts/smoke-accounts.cjs`    → require('electron') 拿到 API，负责校验。
 * 需要本机装了 Electron 二进制（CI 设置了 ELECTRON_SKIP_BINARY_DOWNLOAD=1，所以不进 CI）。
 */
'use strict';

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const electronModule = require('electron');

if (typeof electronModule === 'string') {
  launch(electronModule);
} else {
  verify(electronModule);
}

function launch(binary) {
  if (!fs.existsSync(binary)) {
    console.error(`找不到 Electron 二进制：${binary}`);
    console.error('本冒烟只在本地开发机可用（CI 设置了 ELECTRON_SKIP_BINARY_DOWNLOAD=1）。');
    process.exit(1);
  }
  const { spawn } = require('node:child_process');
  const env = { ...process.env };
  // Electron 系 IDE 的内置终端会继承这个变量，令 Electron 退化成纯 Node。
  delete env.ELECTRON_RUN_AS_NODE;
  const flags = process.env.VCS_SMOKE_UI_FLAGS
    ? process.env.VCS_SMOKE_UI_FLAGS.split(/\s+/).filter(Boolean)
    : ['--disable-gpu', '--no-sandbox'];
  const child = spawn(binary, [__filename, ...flags], { stdio: 'inherit', env });
  child.on('error', (err) => {
    console.error(`启动 Electron 失败：${err.message}`);
    process.exit(1);
  });
  child.on('exit', (code, signal) => process.exit(code === null ? (signal ? 1 : 0) : code));
}

function verify({ app }) {
  const DESKTOP = path.resolve(__dirname, '..');
  const settingsEntry = path.join(DESKTOP, 'dist', 'main', 'settings.js');
  const problems = [];

  if (!fs.existsSync(settingsEntry)) {
    console.log(
      `SMOKE_ACCOUNTS_RESULT=${JSON.stringify({
        pass: false,
        problems: ['构建产物缺失，先执行 npm run build'],
      })}`,
    );
    app.exit(1);
    return;
  }

  // 每个用例一个干净的 userData：settings.json 的位置由 app.getPath 决定，
  // 换路径就等于换一台机器。
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-accounts-'));
  const useStore = (name) => {
    const dir = path.join(root, name);
    fs.mkdirSync(dir, { recursive: true });
    app.setPath('userData', dir);
    return dir;
  };
  const settingsPathIn = (dir) => path.join(dir, 'settings.json');
  const readJson = (file) => JSON.parse(fs.readFileSync(file, 'utf8'));
  const writeJson = (file, value) => fs.writeFileSync(file, JSON.stringify(value, null, 2));

  const ALICE = { target: '127.0.0.1:50051', username: 'alice' };
  const BOB = { target: '127.0.0.1:50051', username: 'bob' };
  const S1 = '/tmp/accounts-s1';
  const S2 = '/tmp/accounts-s2';

  const expect = (label, actual, expected) => {
    const a = JSON.stringify(actual);
    const e = JSON.stringify(expected);
    if (a !== e) problems.push(`${label}：期望 ${e}，实际 ${a}`);
  };

  try {
    // 1) 两个账号各自检出到不同目录：互相看不到对方的目录，顺序也不互相顶掉。
    const dir = useStore('isolated');
    const s = require(settingsEntry);
    s.saveRepoDirs(ALICE, { r1: [S1] });
    s.saveRepoDirs(BOB, { r1: [S2] });
    expect('alice 的目录', s.loadRepoDirs(ALICE), { r1: [S1] });
    expect('bob 的目录', s.loadRepoDirs(BOB), { r1: [S2] });

    // 结构落在 accounts[地址][用户名] 上，旧的全局字段不能再出现。
    const file = readJson(settingsPathIn(dir));
    expect('accounts 结构', file.accounts, {
      '127.0.0.1:50051': {
        alice: { repoDirs: { r1: [S1] } },
        bob: { repoDirs: { r1: [S2] } },
      },
    });
    if (Object.prototype.hasOwnProperty.call(file, 'repoDirs')) {
      problems.push('旧的全局 repoDirs 字段还在，换账号时仍会串台');
    }

    // 2) 后一个账号再检出第二份，不会改变前一个账号的首项（回归原 bug 的核心）。
    s.saveRepoDirs(BOB, { r1: [S2, '/tmp/accounts-s2b'] });
    expect('alice 不受 bob 影响', s.loadRepoDirs(ALICE), { r1: [S1] });
    expect('bob 的首项', s.loadRepoDirs(BOB), { r1: [S2, '/tmp/accounts-s2b'] });

    // 3) 同名的不同服务端也是两个账号：换地址不该继承另一台的目录。
    const OTHER_SERVER = { target: '10.0.0.2:50051', username: 'alice' };
    expect('另一台服务器上的同名账号', s.loadRepoDirs(OTHER_SERVER), {});

    // 4) 移出列表到一份不剩：账号条目整个消失，不留空壳。
    s.saveRepoDirs(BOB, {});
    expect('bob 清空', s.loadRepoDirs(BOB), {});
    if (readJson(settingsPathIn(dir)).accounts['127.0.0.1:50051'].bob) {
      problems.push('清空后仍留着 bob 的空壳条目');
    }

    // 5) 没有登录身份：读为空、写报错（一份没有主人的目录记忆只会变成别人的）。
    expect('匿名读取', s.loadRepoDirs(null), {});
    let threw = false;
    try {
      s.saveRepoDirs(null, { r1: [S1] });
    } catch {
      threw = true;
    }
    if (!threw) problems.push('没有登录身份时 saveRepoDirs 仍然写盘了');

    // 6) 老客户端留下的全局 repoDirs：归给上次登录的那个账号，其他人读不到。
    const legacyDir = useStore('legacy');
    writeJson(settingsPathIn(legacyDir), {
      target: ALICE.target,
      username: 'alice',
      repoDirs: { r1: '/tmp/legacy-s1' },
    });
    const legacy = require(settingsEntry);
    expect('迁移后的 alice', legacy.loadRepoDirs(ALICE), { r1: ['/tmp/legacy-s1'] });
    expect('迁移后 bob 仍为空', legacy.loadRepoDirs(BOB), {});
    const migrated = readJson(settingsPathIn(legacyDir));
    if (Object.prototype.hasOwnProperty.call(migrated, 'repoDirs')) {
      problems.push('迁移后没删掉旧的全局 repoDirs，下次还会再迁一次');
    }
    expect('迁移只认上次登录的账号', migrated.accounts[ALICE.target].alice, {
      repoDirs: { r1: ['/tmp/legacy-s1'] },
    });
    // 迁移后老账号再登录一次仍然拿得到，证明写回的是同一份。
    expect('迁移可重复读', legacy.loadRepoDirs(ALICE), { r1: ['/tmp/legacy-s1'] });

    // 7) 老设置里「单字符串」的值照旧升级成列表。
    const singleDir = useStore('single');
    writeJson(settingsPathIn(singleDir), {
      accounts: { [ALICE.target]: { alice: { repoDirs: { r2: '/tmp/single-wc' } } } },
    });
    const single = require(settingsEntry);
    expect('单字符串升级', single.loadRepoDirs(ALICE), { r2: ['/tmp/single-wc'] });

    // 8) 常用权重：越用越大、按账号隔离、久不使用会衰减掉，清空目录时不跟着被删。
    const usageDir = useStore('usage');
    const usage = require(settingsEntry);
    expect('匿名读取权重', usage.loadRepoUsage(null), {});
    usage.bumpRepoUsage(ALICE, 'r1');
    const afterSecond = usage.bumpRepoUsage(ALICE, 'r1');
    const afterOther = usage.bumpRepoUsage(ALICE, 'r2');
    // r1 用了两次、r2 一次：列表要靠这个顺序把 r1 排在前面。
    if (!(afterOther.r1.weight > afterOther.r2.weight)) {
      problems.push(`重复使用没有累加权重：${JSON.stringify(afterOther)}`);
    }
    if (!(afterSecond.r1.weight > 1.9 && afterSecond.r1.weight <= 2)) {
      problems.push(`两次使用应约等于 2 分：${JSON.stringify(afterSecond.r1)}`);
    }
    expect('权重按账号隔离', usage.loadRepoUsage(BOB), {});
    expect('匿名写入权重不落盘', usage.bumpRepoUsage(null, 'r1'), {});
    // 落盘结构：只存「分数 + 时刻」，折算后的权重由读的时候算，避免越读越旧。
    const usageFile = readJson(settingsPathIn(usageDir));
    const stored = usageFile.accounts[ALICE.target].alice.repoUsage;
    for (const id of ['r1', 'r2']) {
      const keys = Object.keys(stored[id] || {}).sort().join(',');
      if (keys !== 'last,score') problems.push(`使用记录的字段应该是 score/last，实际 ${keys}`);
    }
    // 清空工作副本目录：账号条目可以没有 repoDirs，但使用记录不能被顺手删掉。
    usage.saveRepoDirs(ALICE, {});
    if (!readJson(settingsPathIn(usageDir)).accounts[ALICE.target].alice.repoUsage) {
      problems.push('清空工作副本登记时把使用记录一起删了');
    }
    if (usage.loadRepoUsage(ALICE).r1 === undefined) {
      problems.push('清空工作副本登记后读不到原来的使用记录');
    }

    // 9) 三个月没碰过的记录直接失效：它已经不代表「常用」，留着只会占位。
    const oldDir = useStore('stale-usage');
    const day = 24 * 60 * 60 * 1000;
    const now = Date.now();
    writeJson(settingsPathIn(oldDir), {
      accounts: {
        [ALICE.target]: {
          alice: {
            repoUsage: {
              old: { score: 9, last: now - 120 * day },
              fresh: { score: 1, last: now - day },
              future: { score: 1, last: now + 30 * day },
              broken: { score: -1, last: now },
            },
          },
        },
      },
    });
    const stale = require(settingsEntry);
    const weights = stale.loadRepoUsage(ALICE);
    if (weights.old !== undefined) problems.push('120 天没用过的记录没有被丢掉');
    if (weights.broken !== undefined) problems.push('坏数据（负分）没有被丢掉');
    if (!(weights.fresh && weights.fresh.weight > 0.9)) {
      problems.push(`一天前的记录应仍有接近 1 分：${JSON.stringify(weights.fresh)}`);
    }
    // 未来时间（改过系统时钟）按「就是现在」算，不能因此永远排第一。
    if (!(weights.future && weights.future.weight <= 1.0001)) {
      problems.push(`未来的时间戳没有被夹回现在：${JSON.stringify(weights.future)}`);
    }
  } catch (err) {
    problems.push(`执行出错：${err && err.message}`);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }

  const pass = problems.length === 0;
  console.log(`SMOKE_ACCOUNTS_RESULT=${JSON.stringify({ pass, problems })}`);
  app.exit(pass ? 0 : 1);
}
