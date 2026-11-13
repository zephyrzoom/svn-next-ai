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
    displayName: '张爱丽',
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
  // 地址不再显示在界面上，只能靠 IPC 记录证明「保存 / 恢复默认」真的落到了主进程。
  const setTargetCalls = [];
  for (const channel of channels) {
    ipcMain.handle(channel, async (_event, ...args) => {
      if (channel === 'vcs:setTarget') setTargetCalls.push(args[0]);
      return { ok: true, data: channel === 'vcs:loadPrefs' ? PREFS : null };
    });
  }
  // 免登录改成默认行为后，界面上已经没有开关可查：直接钉住主进程契约，
  // 防止某次重构把桌面端登录又退回 12 小时会话。
  const mainSrc = fs.readFileSync(mainEntry, 'utf8');
  const fixedLongSession =
    /const DESKTOP_REMEMBER_ME\s*=\s*true/.test(mainSrc) &&
    /login\(username, password, source, DESKTOP_REMEMBER_ME\)/.test(mainSrc);

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
        info = await win.webContents.executeJavaScript(`(() => {
          const settingsButton = document.getElementById('open-settings');
          const authText = document.getElementById('auth-screen').textContent;
          return {
            title: document.title,
            brand: document.querySelector('.brand') && document.querySelector('.brand').textContent,
            css: getComputedStyle(document.body).backgroundColor,
            apiType: typeof window.vcs,
            apiKeys: window.vcs ? Object.keys(window.vcs).length : 0,
            defaultTarget: window.vcsConfig && window.vcsConfig.defaultTarget,
            domIds: document.querySelectorAll('[id]').length,
            loginVisible: !document.getElementById('auth-screen').classList.contains('hidden'),
            appHidden: document.getElementById('app-shell').classList.contains('hidden'),
            hasRememberCheckbox: Boolean(document.getElementById('remember')),
            loginInputs: Array.from(document.querySelectorAll('#login-form input')).map((i) => i.id),
            hasTargetInput: Boolean(document.getElementById('target')),
            settingsHidden: document.getElementById('server-settings').classList.contains('hidden'),
            hasSettingsButton: Boolean(settingsButton),
            settingsIsIcon:
              Boolean(settingsButton) &&
              settingsButton.textContent.trim() === '' &&
              Boolean(settingsButton.querySelector('svg')) &&
              settingsButton.title === '服务器设置' &&
              settingsButton.getAttribute('aria-label') === '服务器设置',
            authShowsTarget:
              authText.includes('${PREFS.target}') ||
              /\\d{1,3}(\\.\\d{1,3}){3}:\\d+/.test(authText) ||
              authText.includes('服务器：'),
            hasServerLabel: Boolean(document.getElementById('server-label')),
            localTabActive: document.getElementById('tab-local').classList.contains('active'),
            ldapLabel: document.getElementById('tab-ldap').textContent.trim(),
            hasSourceSelect: Boolean(document.getElementById('auth-source')),
          };
        })()`);
      } catch (err) {
        problems.push(`页面求值失败：${err && err.message}`);
        finish({ pass: false });
        return;
      }

      // 真点一遍：tab 切换要改登录身份与注册入口，齿轮面板要能覆盖并恢复默认地址。
      // IPC 已被 stub，所以这里的 click 不会连服务端，只验证界面逻辑与桥接链路。
      let interaction;
      try {
        interaction = await win.webContents.executeJavaScript(`(async () => {
          const wait = () => new Promise((resolve) => setTimeout(resolve, 150));
          const tabLdap = document.getElementById('tab-ldap');
          tabLdap.click();
          const ldap = {
            active: tabLdap.classList.contains('active'),
            registerHidden: document.getElementById('show-register').classList.contains('hidden'),
            subtitle: document.getElementById('auth-sub').textContent,
          };
          document.getElementById('tab-local').click();
          const local = {
            active: document.getElementById('tab-local').classList.contains('active'),
            registerVisible: !document.getElementById('show-register').classList.contains('hidden'),
          };
          const settings = document.getElementById('server-settings');
          const input = document.getElementById('server-target');
          const gear = document.getElementById('open-settings');
          const authText = () => document.getElementById('auth-screen').textContent;
          gear.click();
          const opened = !settings.classList.contains('hidden');
          const prefilled = input.value;
          input.value = '10.0.0.2:50051';
          document.getElementById('save-target').click();
          await wait();
          const savedPanelClosed = settings.classList.contains('hidden');
          const savedMessage = document.getElementById('auth-message').textContent;
          // 保存之后地址也不该出现在登录页正文里：界面只给一句确认。
          const addressHidden = !authText().includes('10.0.0.2:50051');
          gear.click();
          const savedAgain = input.value;
          document.getElementById('reset-target').click();
          await wait();
          const resetMessage = document.getElementById('auth-message').textContent;
          gear.click();
          const reset = input.value;
          return {
            ldap,
            local,
            opened,
            prefilled,
            savedPanelClosed,
            savedMessage,
            addressHidden,
            savedAgain,
            reset,
            resetMessage,
          };
        })()`);
      } catch (err) {
        problems.push(`交互求值失败：${err && err.message}`);
        finish({ pass: false });
        return;
      }

      // 身份区收尾：登录 / 免登录恢复都走 applySession，左下角必须是姓名。
      // 同时钉住主界面布局：没有顶部条 / 品牌字样，也没有原来的左侧说明文字。
      let identity = {};
      try {
        identity = await win.webContents.executeJavaScript(`(async () => {
          try {
            await applySession({
              user: { userId: 'u_smoke', username: 'smoke', displayName: '张爱丽', role: 3 },
              expiresAt: 0,
            }, { target: 'smoke.example:50051' });
          } catch (_) {
            // 仓库列表的 IPC 被打桩成 null，收尾会抛错；#who 在这之前已经写好。
          }
          const who = document.getElementById('who');
          const out = document.getElementById('disconnect');
          const box = document.querySelector('.nav-user');
          const nav = document.getElementById('nav');
          return {
            whoName: who ? who.textContent.trim() : '',
            inLeftFoot: Boolean(box && who && out && box.contains(who) && box.contains(out)),
            hasTopbar: Boolean(document.querySelector('#app-shell .topbar')),
            hasAppBrand: Boolean(document.querySelector('#app-shell .brand')),
            oldHintGone:
              !document.querySelector('.nav-foot') &&
              !nav.textContent.includes('选中仓库后') &&
              !nav.textContent.includes('服务端 Web 管理端'),
          };
        })()`);
      } catch (err) {
        problems.push(`身份区求值失败：${err && err.message}`);
      }

      const checks = {
        页面标题: info.title === 'svn-next',
        // 桌面端剥离管理 UI 后 id 数下降，阈值按「仓库使用」客户端的实际结构设定。
        页面结构: info.brand === 'svn-next' && info.domIds > 25,
        样式已加载: /^rgb\(/.test(info.css),
        preload_桥接: info.apiType === 'object' && info.apiKeys > 20,
        // PREFS 里保存的覆盖值已生效：不显示在界面上，但打开齿轮面板能读到它。
        渲染脚本已启动: interaction.prefilled === PREFS.target,
        // 启动契约静态检查之外，这里再确认打包注入的默认地址真的到了渲染层。
        默认地址已注入: /^[A-Za-z0-9._:-]+:\d{1,5}$/.test(info.defaultTarget || ''),
        登录页默认可见: info.loginVisible === true && info.appHidden === true,
        // 免登录改为默认行为：登录页不再有勾选框，表单只留用户名与密码。
        登录页无免登录勾选框: info.hasRememberCheckbox === false,
        登录表单只有用户名与密码:
          Array.isArray(info.loginInputs) &&
          info.loginInputs.join(',') === 'username,password',
        桌面端固定_30_天长期会话: fixedLongSession,
        左下角显示姓名: identity.whoName === '张爱丽' && identity.inLeftFoot === true,
        顶部条与左下角说明已移除:
          identity.hasTopbar === false &&
          identity.hasAppBrand === false &&
          identity.oldHintGone === true,
        // 地址一律不在界面上出现：登录页正文没有，主界面也没有。
        界面不显示服务器地址:
          info.authShowsTarget === false &&
          info.hasServerLabel === false &&
          info.hasTargetInput === false &&
          interaction.addressHidden === true,
        设置入口是齿轮图标:
          info.hasSettingsButton === true && info.settingsIsIcon === true && info.settingsHidden === true,
        账号类型_tab:
          info.localTabActive === true && info.ldapLabel === 'LDAP 账号' && info.hasSourceSelect === false,
        tab_切换生效:
          interaction.ldap.active === true &&
          interaction.ldap.registerHidden === true &&
          interaction.ldap.subtitle.includes('LDAP 账号') &&
          interaction.local.active === true &&
          interaction.local.registerVisible === true,
        设置覆盖与恢复默认:
          interaction.opened === true &&
          interaction.savedPanelClosed === true &&
          interaction.savedMessage.includes('已保存') &&
          interaction.savedAgain === '10.0.0.2:50051' &&
          interaction.reset === info.defaultTarget &&
          interaction.resetMessage.includes('默认地址') &&
          setTargetCalls.includes('10.0.0.2:50051') &&
          setTargetCalls.includes(''),
      };
      for (const [name, ok] of Object.entries(checks)) {
        if (!ok) problems.push(`断言失败：${name}`);
      }
      finish({ pass: Object.values(checks).every(Boolean), ...info, interaction, identity });
    })
    .catch((err) => finish({ pass: false, fatal: `whenReady 失败：${err && err.message}` }));
}
