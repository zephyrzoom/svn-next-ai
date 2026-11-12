#!/usr/bin/env node
/**
 * 把 tsc 不会产出的静态资源搬到位，供开发启动与打包共用。
 *
 * 1) `proto/repo.proto` 在仓库根、属于 monorepo 的共享契约；electron-builder 只会
 *    打包应用目录（desktop/）里的东西，复制一份到 desktop/proto 让它进 asar，
 *    比运行时去猜相对路径可靠得多（asar 内部读文件是支持的）。
 * 2) 渲染层的 index.html / app.js / styles.css 不是 .ts，tsc 不会碰它们，而主进程
 *    的 loadFile 指向 `dist/renderer/index.html`。必须同步到 dist，否则窗口会
 *    因文件不存在而白屏；builder 的 files 规则已包含整个 dist，顺带也覆盖打包。
 * 3) 默认服务器地址在**打包时**注入：读 `desktop/build-config.json` 的
 *    `defaultTarget`（可用环境变量 `VCS_DEFAULT_TARGET` 覆盖），生成
 *    `dist/renderer/config.js`。它随 dist 一起进安装包，登录页不暴露地址输入框；
 *    用户后续在「设置」里改的地址存 userData/settings.json，覆盖这个值。
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const DESKTOP = path.join(ROOT, 'desktop');

// —— 契约文件 ——
const source = path.join(ROOT, 'proto', 'repo.proto');
const targetDir = path.join(DESKTOP, 'proto');
const target = path.join(targetDir, 'repo.proto');

if (!fs.existsSync(source)) {
  console.error(`找不到契约文件：${source}`);
  process.exit(1);
}
fs.mkdirSync(targetDir, { recursive: true });
fs.copyFileSync(source, target);
console.log(`契约已就位：${path.relative(ROOT, target)}`);

// —— 渲染层静态资源 ——
const rendererSrc = path.join(DESKTOP, 'src', 'renderer');
const rendererDst = path.join(DESKTOP, 'dist', 'renderer');

if (!fs.existsSync(path.join(rendererSrc, 'index.html'))) {
  console.error(`找不到界面文件：${path.join(rendererSrc, 'index.html')}`);
  process.exit(1);
}
fs.cpSync(rendererSrc, rendererDst, { recursive: true });
console.log(`界面资源已就位：${path.relative(ROOT, rendererDst)}/`);

// —— 构建期默认服务器地址 ——
const configFile = path.join(DESKTOP, 'build-config.json');
let defaultTarget = '127.0.0.1:50051';
try {
  const parsed = JSON.parse(fs.readFileSync(configFile, 'utf8'));
  if (typeof parsed.defaultTarget === 'string' && parsed.defaultTarget.trim()) {
    defaultTarget = parsed.defaultTarget.trim();
  }
} catch (err) {
  console.error(`读取 ${path.relative(ROOT, configFile)} 失败：${err.message}`);
  process.exit(1);
}
// CI / 分发渠道可以给同一个包打不同地址，不必改源码。
if (process.env.VCS_DEFAULT_TARGET) defaultTarget = process.env.VCS_DEFAULT_TARGET.trim();
// 只允许 host:port（不带协议、不带路径），避免把脏值或脚本拼进 config.js。
if (!/^[A-Za-z0-9._:-]+:\d{1,5}$/.test(defaultTarget)) {
  console.error(`默认服务器地址不合法：${defaultTarget}（应为 host:port，例如 127.0.0.1:50051）`);
  process.exit(1);
}
const configJs =
  '// 由 scripts/stage-assets.cjs 生成，请勿手改；改地址请编辑 desktop/build-config.json。\n' +
  `window.vcsConfig = Object.freeze({ defaultTarget: ${JSON.stringify(defaultTarget)} });\n`;
fs.writeFileSync(path.join(rendererDst, 'config.js'), configJs);
console.log(`默认服务器地址：${defaultTarget}（来源：VCS_DEFAULT_TARGET 或 build-config.json）`);
