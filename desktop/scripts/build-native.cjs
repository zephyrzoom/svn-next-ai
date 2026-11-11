#!/usr/bin/env node
/**
 * 构建 napi 原生模块并放到 desktop/native/vcs-napi.node。
 *
 * 为什么不引 @napi-rs/cli：我们只需要「产出一个 Node 能 require 的动态库」，
 * cargo build 出来的 cdylib 改个后缀就能用。少一个构建工具链，CI 也少一步。
 */
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const release = process.argv.includes('--release');
const profile = release ? 'release' : 'debug';

const args = ['build', '-p', 'vcs-napi'];
if (release) args.push('--release');
const build = spawnSync('cargo', args, { cwd: ROOT, stdio: 'inherit' });
if (build.status !== 0) {
  process.exit(build.status ?? 1);
}

const ext = process.platform === 'win32' ? 'dll' : process.platform === 'darwin' ? 'dylib' : 'so';
const stem = process.platform === 'win32' ? 'vcs_napi' : 'libvcs_napi';
const source = path.join(ROOT, 'target', profile, `${stem}.${ext}`);
const target = path.join(__dirname, '..', 'native', 'vcs-napi.node');

if (!fs.existsSync(source)) {
  console.error(`找不到构建产物：${source}`);
  process.exit(1);
}
fs.mkdirSync(path.dirname(target), { recursive: true });
fs.copyFileSync(source, target);
console.log(`原生模块已就位：${path.relative(ROOT, target)}`);
