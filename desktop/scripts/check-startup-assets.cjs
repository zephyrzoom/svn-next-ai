#!/usr/bin/env node
/**
 * 桌面端「启动契约」检查：编译产物齐全，不代表窗口能显示出来。
 *
 * 背景：主进程 loadFile(dist/renderer/index.html)，但 tsc 只编译 .ts，静态页面
 * 靠 stage-assets.cjs 复制。过去的 CI 只做 typecheck + 纯 Node 冒烟（后者只 require
 * dist/main/client.js 等业务逻辑），既不启动 Electron，也不检查资源装配，于是
 * 「构建成功 + 白屏」能一路混过去。
 *
 * 本脚本纯 Node、无 GUI、无网络，即使 CI 跳过 Electron 二进制下载也能跑：
 *   1. package.json 的 main 指向的产物存在；
 *   2. 主进程产物里 path.join(__dirname, …) 引用的文件（preload、页面）都存在；
 *      一个都找不到就主动报错，避免检查规则悄悄与源码脱节；
 *   3. 页面里所有本地 script / link 引用都存在；
 *   4. dist/renderer 与 src/renderer 内容一致，避免改了源码却用旧产物启动。
 *
 * 用法：node scripts/check-startup-assets.cjs（由 npm run build 末尾自动调用）
 */
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const DESKTOP = path.resolve(__dirname, '..');
const DIST = path.join(DESKTOP, 'dist');
const errors = [];
const passed = [];

function requireFile(label, file) {
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
    errors.push(`${label} 不存在：${path.relative(DESKTOP, file)}`);
    return false;
  }
  passed.push(label);
  return true;
}

// 1) 入口产物
const pkg = JSON.parse(fs.readFileSync(path.join(DESKTOP, 'package.json'), 'utf8'));
const mainFile = path.join(DESKTOP, pkg.main || '');

// 2) 主进程引用的相对路径（跟着编译产物走，主进程改了路径也能拦住）
if (requireFile('主进程入口', mainFile)) {
  const mainSrc = fs.readFileSync(mainFile, 'utf8');
  const refs = [
    ...mainSrc.matchAll(/path\.join\(__dirname,\s*(['"])([^'"]+)\1/g),
  ].map((match) => match[2]);
  if (refs.length === 0) {
    errors.push(
      '主进程产物里找不到 path.join(__dirname, …) 引用；检查规则可能与源码脱节，请更新本脚本',
    );
  }
  for (const rel of new Set(refs)) {
    requireFile(`主进程引用 ${rel}`, path.resolve(path.dirname(mainFile), rel));
  }
}

// 3) 页面里的本地资源引用
const page = path.join(DIST, 'renderer', 'index.html');
if (requireFile('渲染页面', page)) {
  const html = fs.readFileSync(page, 'utf8');
  const localRefs = [
    ...html.matchAll(/(?:src|href)\s*=\s*(['"])([^'"]+)\1/g),
  ]
    .map((match) => match[2])
    .filter((ref) => !/^(?:[a-z]+:|\/\/|#)/i.test(ref))
    .map((ref) => ref.split(/[?#]/)[0]);
  if (localRefs.length === 0) {
    errors.push('页面里找不到本地 script / link 引用；检查规则可能与页面脱节');
  }
  for (const ref of new Set(localRefs)) {
    requireFile(`页面引用 ${ref}`, path.resolve(path.dirname(page), ref));
  }
}

// 4) 源码与产物一致性（改了 html/js/css 却忘了重新构建，不该静默用旧界面）
const srcDir = path.join(DESKTOP, 'src', 'renderer');
const outDir = path.join(DIST, 'renderer');
if (!fs.existsSync(srcDir)) {
  errors.push(`找不到渲染层源码目录：${path.relative(DESKTOP, srcDir)}`);
} else {
  for (const name of fs.readdirSync(srcDir).sort()) {
    const source = path.join(srcDir, name);
    if (!fs.statSync(source).isFile()) continue;
    const target = path.join(outDir, name);
    if (
      !fs.existsSync(target) ||
      !fs.readFileSync(source).equals(fs.readFileSync(target))
    ) {
      errors.push(`界面资源未同步或已过期：${name}（重新执行 npm run build）`);
    } else {
      passed.push(`界面资源 ${name} 与源码一致`);
    }
  }
}

if (errors.length > 0) {
  for (const message of errors) console.error(`  ✗ ${message}`);
  console.error(`\n启动契约检查失败：${errors.length} 项。先执行 npm run build 生成完整 dist/。`);
  process.exit(1);
}
console.log(`启动契约检查通过：${passed.length} 项（入口 / 页面 / 引用 / 源码一致性）`);
