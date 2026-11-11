#!/usr/bin/env node
/**
 * 把打包需要的仓库级资源搬进 desktop/。
 *
 * 为什么要搬：electron-builder 只会打包应用目录（desktop/）里的东西，
 * `proto/repo.proto` 在仓库根、属于 monorepo 的共享契约。复制一份到 desktop/proto
 * 让它进 asar，比在运行时去猜相对路径可靠得多（asar 内部读文件是支持的）。
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const source = path.join(ROOT, 'proto', 'repo.proto');
const targetDir = path.join(__dirname, '..', 'proto');
const target = path.join(targetDir, 'repo.proto');

if (!fs.existsSync(source)) {
  console.error(`找不到契约文件：${source}`);
  process.exit(1);
}
fs.mkdirSync(targetDir, { recursive: true });
fs.copyFileSync(source, target);
console.log(`契约已就位：${path.relative(ROOT, target)}`);
