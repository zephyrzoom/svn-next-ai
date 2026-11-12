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
