#!/usr/bin/env node
/**
 * 界面层运行时冒烟（可选，本地执行）：真的启动 Electron，加载真实页面与 preload。
 *
 * 和 check-startup-assets.cjs 的分工：
 *   - 后者是纯静态的「启动契约」检查，CI 里必跑，抓的是资源漏拷 / 路径漂移；
 *   - 本脚本抓静态检查抓不到的：CSP 挡住脚本、preload 桥接失败、app.js 启动即抛错。
 * 需要本机装了 Electron 二进制且有图形环境；CI 的 desktop job 刻意跳过二进制下载，
 * 所以 CI 不跑这一步。Linux 无 DISPLAY 时请用 `xvfb-run npm run smoke:ui`。
 *
 * 单文件双模式：
 *   `node scripts/smoke-ui.cjs`  → require('electron') 得到二进制路径，负责拉起 Electron；
 *   `electron scripts/smoke-ui.cjs` → require('electron') 得到 API，负责执行校验。
 */
'use strict';

const fs = require('node:fs');
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
  // 容器 / 受限沙箱里 Chromium 起不了 GPU 与网络子进程，默认关掉 Chromium 层的
  // sandbox（webPreferences.sandbox 仍在，preload 的受限上下文照样被验证）。
  // 需要原样复现桌面环境时，用 VCS_SMOKE_UI_FLAGS 覆盖整组开关。
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

function verify({ app, BrowserWindow, ipcMain }) {
  const DESKTOP = path.resolve(__dirname, '..');
  const page = path.join(DESKTOP, 'dist', 'renderer', 'index.html');
  const preload = path.join(DESKTOP, 'dist', 'preload', 'index.js');
  const mainEntry = path.join(DESKTOP, 'dist', 'main', 'index.js');
  const PREFS = {
    target: 'smoke.example:50051',
    username: 'smoke',
    hasToken: false,
    canPersistToken: true,
    sessionExpiresAt: 0,
  };
  const problems = [];
  let settled = false;

  function finish(info) {
    if (settled) return;
    settled = true;
    const pass = problems.length === 0 && Boolean(info && info.pass);
    console.log(`SMOKE_UI_RESULT=${JSON.stringify({ pass, problems, info })}`);
    app.exit(pass ? 0 : 1);
  }

  setTimeout(() => finish({ pass: false, fatal: '超时未完成（15s）' }), 15000).unref();

  if (![page, preload, mainEntry].every((file) => fs.existsSync(file))) {
    finish({ pass: false, fatal: '构建产物不全，先执行 npm run build' });
    return;
  }

  // 从真实主进程产物里抽出所有 IPC 通道名并打桩：不连服务端，但要让 app.js 的
  // bootstrap（loadPrefs / tryResume）能跑完，才能证明 preload 桥是通的。
  const channels = new Set(
    [
      ...fs
        .readFileSync(mainEntry, 'utf8')
        .matchAll(/ipcMain\.handle\(\s*['"](vcs:[^'"]+)['"]/g),
    ].map((match) => match[1]),
  );
  if (channels.size === 0) {
    problems.push('未能从主进程产物提取 IPC 通道，检查规则可能与源码脱节');
  }
  for (const channel of channels) {
    ipcMain.handle(channel, async () => ({
      ok: true,
      data: channel === 'vcs:loadPrefs' ? PREFS : null,
    }));
  }

  app
    .whenReady()
    .then(async () => {
      const win = new BrowserWindow({
        show: false,
        webPreferences: {
          preload,
          contextIsolation: true,
          nodeIntegration: false,
          sandbox: true,
        },
      });
      win.webContents.on('did-fail-load', (_event, code, desc) => {
        problems.push(`页面加载失败：${code} ${desc}`);
      });
      win.webContents.on('preload-error', (_event, file, err) => {
        problems.push(`preload 加载失败：${path.basename(file)} — ${err && err.message}`);
      });
      win.webContents.on('console-message', (_event, level, message) => {
        const isError =
          typeof level === 'number' ? level >= 2 : /error/i.test(String(level));
        if (isError) problems.push(`渲染进程 console：${message}`);
      });
      win.webContents.on('render-process-gone', (_event, details) => {
        problems.push(`渲染进程退出：${details.reason}`);
      });

      try {
        await win.loadFile(page);
      } catch (err) {
        problems.push(`loadFile 抛错：${err && err.message}`);
      }

      let info;
      try {
        info = await win.webContents.executeJavaScript(`(() => ({
          title: document.title,
          brand: document.querySelector('.brand') && document.querySelector('.brand').textContent,
          css: getComputedStyle(document.body).backgroundColor,
          apiType: typeof window.vcs,
          apiKeys: window.vcs ? Object.keys(window.vcs).length : 0,
          target: document.getElementById('target') && document.getElementById('target').value,
          domIds: document.querySelectorAll('[id]').length,
          loginVisible: !document.getElementById('auth-screen').classList.contains('hidden'),
          appHidden: document.getElementById('app-shell').classList.contains('hidden'),
          rememberChecked: document.getElementById('remember').checked,
        }))()`);
      } catch (err) {
        problems.push(`页面求值失败：${err && err.message}`);
        finish({ pass: false });
        return;
      }

      // target 被 stub 的 PREFS 改写，说明 app.js 真的执行了 bootstrap，
      // 且 invoke → ipcMain → preload → contextBridge 整条链路是通的。
      const checks = {
        页面标题: info.title === 'svn-next',
        // 桌面端剥离管理 UI 后 id 数下降，阈值按「仓库使用」客户端的实际结构设定。
        页面结构: info.brand === 'svn-next' && info.domIds > 25,
        样式已加载: /^rgb\(/.test(info.css),
        preload_桥接: info.apiType === 'object' && info.apiKeys > 20,
        渲染脚本已启动: info.target === PREFS.target,
        // 启动时必须先看到登录页；只有有效的 30 天会话才直接进主界面。
        登录页默认可见: info.loginVisible === true && info.appHidden === true,
        免登录默认勾选: info.rememberChecked === true,
      };
      for (const [name, ok] of Object.entries(checks)) {
        if (!ok) problems.push(`断言失败：${name}`);
      }
      finish({ pass: Object.values(checks).every(Boolean), ...info });
    })
    .catch((err) => finish({ pass: false, fatal: `whenReady 失败：${err && err.message}` }));
}
