#!/usr/bin/env node
/**
 * 内置 Web 管理端的端到端冒烟（不需要浏览器 / Electron）。
 *
 * 起真实 `vcs serve`，用 HTTP 验证：静态页面可访问、未登录被拒、管理员登录、
 * 全库视图、审批列表、直通建库、成员 / 配额 / 用户治理接口可用。
 * 管理端端口由内核分配，避免与并行任务抢固定 8080。
 *
 * 用法：node scripts/smoke-admin.cjs（需先在仓库根 cargo build -p vcs-cli）
 */
'use strict';

const { spawn, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const BIN = path.join(ROOT, 'target', 'debug', 'vcs');
const WORK = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-admin-smoke-'));
const STORE = path.join(WORK, 'store');
const PASSWORD = 'Abcdef1!';

function step(message) {
  process.stdout.write(`\n▶ ${message}\n`);
}

function fail(message) {
  process.stderr.write(`\n✗ ${message}\n`);
  throw new Error(message);
}

function check(condition, message) {
  if (!condition) fail(message);
}

function freePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(() => resolve(port));
    });
  });
}

function startServer(grpcPort, adminPort) {
  return new Promise((resolve, reject) => {
    const child = spawn(
      BIN,
      ['--store', STORE, 'serve',
        '--listen', `127.0.0.1:${grpcPort}`,
        '--admin-listen', `127.0.0.1:${adminPort}`,
        '--maintenance-interval', '0'],
      { stdio: ['ignore', 'pipe', 'pipe'] },
    );
    const timer = setTimeout(() => reject(new Error('服务端启动超时')), 15000);
    let buffer = '';
    child.stdout.on('data', (chunk) => {
      buffer += chunk.toString();
      if (buffer.includes('Web 管理端已启动')) {
        clearTimeout(timer);
        resolve(child);
      }
    });
    child.stderr.on('data', (chunk) => process.stderr.write(chunk));
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`服务端提前退出（code=${code}）`));
    });
  });
}

async function api(base, route, { method = 'GET', body, token } = {}) {
  const headers = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const res = await fetch(`${base}${route}`, {
    method, headers, body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let data;
  try {
    data = JSON.parse(text);
  } catch {
    data = text;
  }
  return { status: res.status, data };
}

async function main() {
  if (!fs.existsSync(BIN)) fail(`找不到 ${BIN}，请先执行 cargo build -p vcs-cli`);
  const init = spawnSync(
    BIN,
    ['--store', STORE, 'init-admin', '--username', 'alice', '--name', '管理员', '--password', PASSWORD],
    { encoding: 'utf8' },
  );
  if (init.status !== 0) fail(`init-admin 失败：${init.stderr || init.stdout}`);

  const grpcPort = await freePort();
  const adminPort = await freePort();
  const base = `http://127.0.0.1:${adminPort}`;
  step(`启动服务：gRPC ${grpcPort} / 管理端 ${adminPort}`);
  const server = await startServer(grpcPort, adminPort);

  try {
    step('静态页面与鉴权');
    for (const [route, needle] of [['/', 'Web 管理端'], ['/app.js', 'svn-next'], ['/styles.css', ':root']]) {
      const res = await fetch(`${base}${route}`);
      check(res.status === 200, `${route} 返回 ${res.status}`);
      check((await res.text()).includes(needle), `${route} 内容缺失`);
    }
    let res = await api(base, '/api/overview');
    check(res.status === 401, `未登录应返回 401，实际 ${res.status}`);
    res = await api(base, '/api/login', { method: 'POST', body: { username: 'alice', password: 'wrong' } });
    check(res.status === 400, `错误口令应被拒，实际 ${res.status}`);

    step('管理员登录与全库视图');
    res = await api(base, '/api/login', {
      method: 'POST', body: { username: 'alice', password: PASSWORD, source: 'LOCAL' },
    });
    check(res.status === 200 && res.data.ok && res.data.data.token, '管理员登录失败');
    const token = res.data.data.token;
    for (const route of ['/api/session', '/api/overview', '/api/users',
      '/api/registrations?state=pending', '/api/repo-requests?state=pending',
      '/api/repos', '/api/quota-defaults', '/api/ldap']) {
      res = await api(base, route, { token });
      check(res.status === 200 && res.data.ok, `GET ${route} 失败：${JSON.stringify(res.data)}`);
    }

    step('直通建库与仓库治理');
    res = await api(base, '/api/repos', {
      method: 'POST', token, body: { name: 'reports', soft_bytes: 0, hard_bytes: 0 },
    });
    check(res.status === 200 && res.data.ok, '直通新建失败');
    const repoId = res.data.data.repo.repo_id;
    const repo = `/api/repos/${encodeURIComponent(repoId)}`;
    res = await api(base, `${repo}/members`, { token });
    check(res.status === 200 && res.data.data.items.length === 1, '管理端应能查看任意仓库成员');
    res = await api(base, `${repo}/quota`, { method: 'PUT', token, body: { soft_bytes: 0, hard_bytes: 2097152, inherit: false } });
    check(res.status === 200 && res.data.data.effective.hard_bytes === 2097152, '单仓库配额覆盖失败');
    res = await api(base, `${repo}/candidates`, { token });
    check(res.status === 200 && res.data.ok, '空间治理接口不可达');
    res = await api(base, `${repo}/status`, { method: 'POST', token, body: { status: 'Disabled' } });
    check(res.status === 200 && res.data.ok, '仓库状态变更失败');

    step('用户与全局配额');
    res = await api(base, '/api/users', {
      method: 'POST', token,
      body: { username: 'bob', display_name: '鲍勃', password: 'Bobpass1!', role: 'writer' },
    });
    check(res.status === 200 && res.data.ok, '直接建号失败');
    const bob = res.data.data.user.user_id;
    res = await api(base, `/api/users/${encodeURIComponent(bob)}/status`, { method: 'POST', token, body: { active: false } });
    check(res.status === 200 && res.data.ok, '停用账号失败');
    res = await api(base, '/api/quota-defaults', { method: 'PUT', token, body: { soft_bytes: 0, hard_bytes: 4194304 } });
    check(res.status === 200 && res.data.ok, '全局默认配额保存失败');
    res = await api(base, '/api/session', { token: 'bogus-token' });
    check(res.status === 401, '无效 token 应被拒');

    process.stdout.write('\n✓ Web 管理端冒烟通过（页面 / 鉴权 / 审批 / 仓库 / 用户 / 配额 / 治理）\n');
  } finally {
    server.kill();
    fs.rmSync(WORK, { recursive: true, force: true });
  }
}

main().catch((err) => {
  process.stderr.write(`\n✗ ${err.message}\n`);
  process.exitCode = 1;
});
