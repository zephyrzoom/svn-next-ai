#!/usr/bin/env node
/**
 * 多账号共存时工作副本目录是否串台的端到端回归（可选，本地执行）。
 *
 * 复现用户报的那条路径，全程用真实服务端、真实主进程与真实界面：
 *   A 在 s1 检出仓库 S → 退出 → B 在 s2 检出 S → 退出 → A 重新登录，
 *   上下文条上的目录必须还是 s1（修复前是 s2，因为登记只有一份、按仓库做键）。
 *
 * 单文件双模式（与 smoke-ui.cjs 一致）：
 *   `node scripts/smoke-account-switch.cjs`      → require('electron') 得到二进制路径，负责拉起；
 *   `electron scripts/smoke-account-switch.cjs`  → require('electron') 得到 API，负责校验。
 *
 * 需要：`cargo build -p vcs-cli`、`npm run build:native`、`npm run build` 都做过，
 * 本机有 Electron 二进制（CI 跳过二进制下载，所以这一步不进 CI）。
 */
'use strict';

const { spawn, spawnSync } = require('node:child_process');
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
  const { spawn: spawnChild } = require('node:child_process');
  const env = { ...process.env };
  // Electron 系 IDE 的内置终端会继承这个变量，令 Electron 退化成纯 Node。
  delete env.ELECTRON_RUN_AS_NODE;
  const flags = process.env.VCS_SMOKE_UI_FLAGS
    ? process.env.VCS_SMOKE_UI_FLAGS.split(/\s+/).filter(Boolean)
    : ['--disable-gpu', '--no-sandbox'];
  const child = spawnChild(binary, [__filename, ...flags], { stdio: 'inherit', env });
  child.on('error', (err) => {
    console.error(`启动 Electron 失败：${err.message}`);
    process.exit(1);
  });
  child.on('exit', (code, signal) => process.exit(code === null ? (signal ? 1 : 0) : code));
}

function verify({ app, BrowserWindow }) {
  const DESKTOP = path.resolve(__dirname, '..');
  const BIN = path.join(DESKTOP, '..', 'target', 'debug', process.platform === 'win32' ? 'vcs.exe' : 'vcs');
  const page = path.join(DESKTOP, 'dist', 'renderer', 'index.html');
  const preload = path.join(DESKTOP, 'dist', 'preload', 'index.js');
  const mainEntry = path.join(DESKTOP, 'dist', 'main', 'index.js');
  const PASSWORD = 'Abcdef1!';

  const WORK = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-account-switch-'));
  const STORE = path.join(WORK, 'store');
  const HOME = path.join(WORK, 'home');
  const S1 = path.join(WORK, 's1');
  const S2 = path.join(WORK, 's2');
  const USER_DATA = path.join(WORK, 'userdata');
  const PORT = 52000 + (process.pid % 900);
  const TARGET = `127.0.0.1:${PORT}`;

  const problems = [];
  const notes = {};
  let server = null;
  let settled = false;

  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

  function finish(pass) {
    if (settled) return;
    settled = true;
    if (server) {
      try {
        server.kill();
      } catch {
        /* 已经退出 */
      }
    }
    const result = { pass: pass && problems.length === 0, problems, notes };
    console.log(`SMOKE_ACCOUNT_SWITCH_RESULT=${JSON.stringify(result)}`);
    if (result.pass) fs.rmSync(WORK, { recursive: true, force: true });
    else console.log(`保留现场：${WORK}`);
    app.exit(result.pass ? 0 : 1);
  }

  function fail(message) {
    problems.push(message);
    throw new Error(message);
  }

  function cli(args) {
    const result = spawnSync(BIN, args, {
      encoding: 'utf8',
      env: { ...process.env, HOME },
    });
    if (result.status !== 0) {
      fail(`CLI ${args[0]} 失败：${(result.stderr || result.stdout || '').trim()}`);
    }
    return result.stdout || '';
  }

  if (![BIN, page, preload, mainEntry].every((file) => fs.existsSync(file))) {
    problems.push('构建产物不全：先 cargo build -p vcs-cli，再 npm run build:native && npm run build');
    finish(false);
    return;
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

  // 主进程的 settings.json 就落在 userData：必须赶在 require 主进程之前换掉。
  fs.mkdirSync(USER_DATA, { recursive: true });
  app.setPath('userData', USER_DATA);

  app
    .whenReady()
    .then(async () => {
      let win = null;
      const evalJs = (expr) => win.webContents.executeJavaScript(expr, true);

      /** 轮询界面直到表达式为真；失败时把当前界面状态一并报出来。 */
      async function until(label, expr, timeout = 20000) {
        const deadline = Date.now() + timeout;
        while (Date.now() < deadline) {
          let ok = false;
          try {
            ok = await evalJs(`Boolean(${expr})`);
          } catch {
            /* 页面正在导航：下一轮再试 */
          }
          if (ok) return;
          await delay(120);
        }
        const snapshot = await evalJs(`(() => ({
          auth: !document.getElementById('auth-screen').classList.contains('hidden'),
          message: document.getElementById('auth-message').textContent,
          banner: document.getElementById('banner').textContent,
          path: document.getElementById('wc-path').textContent,
          dirs: Array.from(document.querySelectorAll('#wc-dir-select option')).map((o) => o.value),
          repos: Array.from(document.querySelectorAll('#repo-nav .repo-name')).map((n) => n.textContent),
        }))()`).catch(() => ({}));
        fail(`等待超时：${label} — ${JSON.stringify(snapshot)}`);
      }

      async function login(username) {
        await evalJs(`(() => {
          document.getElementById('username').value = ${JSON.stringify(username)};
          document.getElementById('password').value = ${JSON.stringify(PASSWORD)};
          document.getElementById('connect').click();
          return true;
        })()`);
        await until(
          `${username} 登录进入主界面`,
          `!document.getElementById('app-shell').classList.contains('hidden')`,
        );
        await until(`${username} 看到仓库列表`, `document.querySelectorAll('#repo-nav .repo-item').length > 0`);
        // 登录后还有容量 / 历史 / 扫描几路请求在跑，等界面落定再断言。
        await delay(400);
      }

      async function logout() {
        await evalJs(`document.getElementById('disconnect').click()`);
        await until(
          '退出登录回到登录页',
          `!document.getElementById('auth-screen').classList.contains('hidden')`,
        );
      }

      async function checkout(dir) {
        await evalJs(`(() => {
          document.getElementById('dir').value = ${JSON.stringify(dir)};
          document.getElementById('checkout').click();
          return true;
        })()`);
        await until(
          `检出到 ${path.basename(dir)}`,
          `document.getElementById('wc-path').textContent.trim() === ${JSON.stringify(dir)}`,
          40000,
        );
      }

      const wcPath = () => evalJs(`document.getElementById('wc-path').textContent.trim()`);
      const navDirs = () =>
        evalJs(`Array.from(document.querySelectorAll('#wc-dir-select option')).map((o) => o.value)`);

      try {
        // ---- 准备服务端：管理员 alice、普通用户 bob、仓库 S（bob 可写）----
        cli(['init-admin', '--store', STORE, '--username', 'alice', '--name', '张爱丽', '--password', PASSWORD]);
        cli(['login', '--store', STORE, '--username', 'alice', '--password', PASSWORD]);
        cli(['user-create', '--store', STORE, '--username', 'bob', '--name', '李波', '--password', PASSWORD]);
        cli(['repo-create', '--store', STORE, 'reports']);
        cli(['member-add', '--store', STORE, 'reports', '--username', 'bob', '--role', 'writer']);

        server = spawn(BIN, ['serve', '--store', STORE, '--listen', TARGET, '--no-admin'], {
          stdio: ['ignore', 'pipe', 'pipe'],
        });
        await waitForServer(server);

        // ---- 拉起真正的主进程与界面 ----
        require(mainEntry);
        const deadline = Date.now() + 20000;
        while (!win && Date.now() < deadline) {
          win = BrowserWindow.getAllWindows()[0] || null;
          if (!win) await delay(100);
        }
        if (!win) fail('主进程没有创建窗口');
        if (win.webContents.isLoading()) {
          await new Promise((resolve) => win.webContents.once('did-finish-load', resolve));
        }
        await until('登录页就绪', `document.getElementById('login-form')`);

        // 登录页齿轮里改地址：默认地址是打包注入的，冒烟要连本次拉起的临时服务端。
        await evalJs(`(() => {
          document.getElementById('open-settings').click();
          document.getElementById('server-target').value = ${JSON.stringify(TARGET)};
          document.getElementById('save-target').click();
          return true;
        })()`);
        await delay(200);

        // ---- A：检出到 s1 ----
        await login('alice');
        await checkout(S1);
        notes.alice = await wcPath();
        await logout();

        // ---- B：同一个仓库，检出到 s2 ----
        await login('bob');
        const bobBefore = await wcPath();
        if (bobBefore !== '未检出到本地') {
          fail(`bob 不该看到 alice 的目录，实际显示：${bobBefore}`);
        }
        await checkout(S2);
        notes.bob = await wcPath();
        await logout();

        // ---- 切回 A：必须还是 s1（修复前这里是 s2）----
        await login('alice');
        const aliceAgain = await wcPath();
        notes.aliceAgain = aliceAgain;
        if (aliceAgain !== S1) {
          fail(`切回 alice 后显示的目录不对：期望 ${S1}，实际 ${aliceAgain}`);
        }
        const aliceDirs = await navDirs();
        notes.aliceDirs = aliceDirs;
        if (aliceDirs.length !== 1 || aliceDirs[0] !== S1) {
          fail(`alice 的目录清单里混进了别人的目录：${JSON.stringify(aliceDirs)}`);
        }
        // 目录下拉不应该出现 s2：那是 bob 的那份。
        if (aliceDirs.includes(S2)) fail('alice 的下拉里出现了 bob 的 s2');

        // 落盘结构也要按账号分开，否则重启后又会串。
        const file = JSON.parse(fs.readFileSync(path.join(USER_DATA, 'settings.json'), 'utf8'));
        notes.accounts = file.accounts;
        if (file.repoDirs) fail('settings.json 里还留着全局 repoDirs，换账号仍会串台');
        const aliceSlot = ((file.accounts || {})[TARGET] || {}).alice || {};
        const bobSlot = ((file.accounts || {})[TARGET] || {}).bob || {};
        const dirsOf = (slot) => Object.values(slot.repoDirs || {}).flat();
        if (!dirsOf(aliceSlot).includes(S1) || dirsOf(aliceSlot).includes(S2)) {
          fail(`alice 的落盘登记不对：${JSON.stringify(aliceSlot)}`);
        }
        if (!dirsOf(bobSlot).includes(S2) || dirsOf(bobSlot).includes(S1)) {
          fail(`bob 的落盘登记不对：${JSON.stringify(bobSlot)}`);
        }

        // ---- 再切回 B：也要还原成自己的 s2 ----
        await logout();
        await login('bob');
        const bobAgain = await wcPath();
        notes.bobAgain = bobAgain;
        if (bobAgain !== S2) fail(`切回 bob 后显示的目录不对：期望 ${S2}，实际 ${bobAgain}`);
        await logout();

        finish(true);
      } catch (err) {
        if (problems.length === 0) problems.push(String((err && err.message) || err));
        finish(false);
      }
    })
    .catch((err) => {
      problems.push(`启动失败：${(err && err.message) || err}`);
      finish(false);
    });
}
