#!/usr/bin/env node
/**
 * 「单文件没有大小上限」的端到端冒烟（不需要 Electron）。
 *
 * 为什么单独一个脚本：`Received message larger than max` 是 gRPC 传输层的经典坑，
 * 只要有人把分块逻辑改回「一个对象塞进一条消息」，大文件检出立刻会在真实环境里炸，
 * 而普通冒烟里的小文件永远发现不了。这里专门提交超过单条消息上限的文件，
 * 并用**不设任何消息上限**的裸通道检出，证明消息大小与文件大小已经解耦。
 *
 * 覆盖：
 *   1. 6 MiB、12 MiB 两个文件提交（服务端按 ObjectChunk 重组入库）；
 *   2. 裸通道（grpc-js 默认 4 MiB）检出，断言每一条消息都远小于文件本身；
 *   3. 桌面端正式路径（VcsClient + downloadObjects）检出，逐字节比对。
 *
 * 用法：node scripts/smoke-bigfile.cjs
 */
'use strict';

const { spawn, spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..', '..');
const BIN = path.join(ROOT, 'target', 'debug');
const WORK = fs.mkdtempSync(path.join(os.tmpdir(), 'vcs-bigfile-smoke-'));
const STORE = path.join(WORK, 'store');
const PORT = 51000 + (process.pid % 1000);
const TARGET = `127.0.0.1:${PORT}`;
const PASSWORD = 'Abcdef1!';
/** 只要大于服务端/客户端的 4 MiB 默认消息上限，就能复现原来的报错。 */
const SIZES = [
  { name: 'six-mib.bin', size: 6 * 1024 * 1024 },
  { name: 'twelve-mib.bin', size: 12 * 1024 * 1024 },
];

function step(message) {
  process.stdout.write(`\n▶ ${message}\n`);
}

function fail(message) {
  process.stderr.write(`\n✗ ${message}\n`);
  throw new Error(message);
}

function payload(size, seed) {
  const buffer = Buffer.alloc(size);
  for (let i = 0; i < size; i += 4096) buffer[i] = (i / 4096 + seed) % 251;
  return buffer;
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

/**
 * 裸通道检出：故意不设 `grpc.max_receive_message_length`（默认 4 MiB）。
 * 如果服务端又回到「一个对象一条消息」，这里会直接抛
 * `Received message larger than max`。
 */
async function checkoutWithDefaultChannel(protoPath, token, repoId) {
  const grpc = require('@grpc/grpc-js');
  const protoLoader = require('@grpc/proto-loader');
  const definition = protoLoader.loadSync(protoPath, {
    keepCase: false,
    longs: Number,
    enums: Number,
    defaults: true,
    oneofs: true,
  });
  const service = grpc.loadPackageDefinition(definition).vcs.repo.v1.RepoService;
  const client = new service(TARGET, grpc.credentials.createInsecure());
  const metadata = new grpc.Metadata();
  metadata.set('authorization', `Bearer ${token}`);
  return new Promise((resolve, reject) => {
    const call = client.checkout({ repoId, rev: 0, paths: [], depth: 1 }, metadata);
    const objects = new Map();
    const partial = new Map();
    let maxMessage = 0;
    call.on('data', (batch) => {
      const bytes =
        (batch.objects ?? []).reduce((sum, object) => sum + object.data.length, 0) +
        (batch.chunks ?? []).reduce((sum, chunk) => sum + chunk.data.length, 0);
      maxMessage = Math.max(maxMessage, bytes);
      for (const object of batch.objects ?? []) {
        objects.set(object.hash, Buffer.from(object.data));
      }
      for (const chunk of batch.chunks ?? []) {
        const pieces = partial.get(chunk.hash) ?? [];
        pieces.push(Buffer.from(chunk.data));
        if (chunk.last) {
          objects.set(chunk.hash, Buffer.concat(pieces));
          partial.delete(chunk.hash);
        } else {
          partial.set(chunk.hash, pieces);
        }
      }
    });
    call.on('error', reject);
    call.on('end', () => {
      client.close();
      resolve({ objects, maxMessage });
    });
  });
}

async function main() {
  step('bootstrap 管理员');
  const init = spawnSync(
    path.join(BIN, 'vcs'),
    ['init-admin', '--store', STORE, '--username', 'alice', '--name', '张爱丽', '--password', PASSWORD],
    { encoding: 'utf8' },
  );
  if (init.status !== 0) fail(`init-admin 失败：${init.stderr || init.stdout}`);

  step(`启动 gRPC 服务 ${TARGET}`);
  const server = spawn(
    path.join(BIN, 'vcs'),
    ['serve', '--store', STORE, '--listen', TARGET, '--no-admin'],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  await waitForServer(server);

  const { VcsClient, hashBytes } = require('../dist/main/client.js');
  const client = new VcsClient(TARGET);

  try {
    step('登录并建库');
    await client.login('alice', PASSWORD);
    const repo = await client.createRepo('big-files');
    console.log(`  ${repo.name} = ${repo.repoId}`);

    step(`提交 ${SIZES.map((f) => `${f.name}(${f.size / 1024 / 1024} MiB)`).join(' + ')}`);
    const files = SIZES.map((file, index) => ({
      path: file.name,
      data: payload(file.size, index),
    }));
    const begin = await client.beginCommit(
      repo.repoId,
      0,
      files.map((file) => ({ path: file.path, kind: 'ADD', hash: hashBytes(file.data) })),
      '提交大文件',
      files.reduce((sum, file) => sum + file.data.length, 0),
    );
    const pushed = await client.pushObjects(
      begin.sessionId,
      files.map((file) => ({ hash: hashBytes(file.data), data: file.data })),
    );
    await client.finalizeCommit(begin.sessionId);
    if (pushed !== files.length) fail(`推送对象数不对：${pushed}`);
    console.log(`  已提交 r1，推送 ${pushed} 个对象`);

    step('裸通道检出（默认 4 MiB 消息上限，不带任何通道参数）');
    const plain = await checkoutWithDefaultChannel(path.join(ROOT, 'proto', 'repo.proto'), client.tokenValue, repo.repoId);
    for (const file of files) {
      const got = plain.objects.get(hashBytes(file.data));
      if (!got || !got.equals(file.data)) fail(`${file.path} 经裸通道检出后内容不一致`);
    }
    if (plain.maxMessage > 4 * 1024 * 1024) {
      fail(`单条消息 ${plain.maxMessage} 字节，超过默认上限，说明分块没生效`);
    }
    console.log(
      `  ${files.length} 个文件内容一致；单条消息最大 ${plain.maxMessage} 字节（文件最大 ${Math.max(...files.map((f) => f.data.length))} 字节）`,
    );

    step('桌面端正式路径检出（VcsClient.downloadObjects）');
    const contents = await client.downloadObjects(repo.repoId, 1);
    for (const file of files) {
      const got = contents.get(hashBytes(file.data));
      if (!got || !got.equals(file.data)) fail(`${file.path} 经客户端检出后内容不一致`);
    }
    console.log(`  ${files.length} 个文件内容一致`);

    process.stdout.write('\n✓ 大文件分块冒烟通过（提交 / 裸通道检出 / 客户端检出）\n');
  } finally {
    client.close();
    server.kill();
  }
}

main().catch((err) => {
  process.stderr.write(`\n大文件冒烟失败：${err && err.stack ? err.stack : err}\n`);
  process.exit(1);
});
