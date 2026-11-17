#!/usr/bin/env node
/**
 * 「大文件不占内存」的端到端冒烟（不需要 Electron）。
 *
 * 两件事分开验：
 *   1. **线上不炸**：提交 6 MiB / 12 MiB 文件，用**不设任何消息上限**的裸通道检出，
 *      断言每条消息都远小于文件本身（分块没被改回「一个对象一条消息」）。
 *   2. **内存不涨**：提交 64 MiB 文件，走桌面端正式路径（pushFiles 逐块读盘 →
 *      downloadObjectsToSpool 逐块落盘 → wc.checkout 流式铺盘），用
 *      `process.resourceUsage().maxRSS` 断言峰值内存增长远小于文件体积。
 *      旧实现（整份读进内存 / 整库攒成 Map）在这里会涨 100 MiB 以上。
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
const SRC = path.join(WORK, 'src');
const WC = path.join(WORK, 'wc');
const PORT = 51000 + (process.pid % 1000);
const TARGET = `127.0.0.1:${PORT}`;
const PASSWORD = 'Abcdef1!';
/** 超过 4 MiB 默认消息上限，用来复现分块；也覆盖「大 + 小混排」。 */
const SMALL_BIG = [
  { name: 'six-mib.bin', size: 6 * 1024 * 1024 },
  { name: 'twelve-mib.bin', size: 12 * 1024 * 1024 },
];
/** 内存回归用的体积：旧实现光这一份就要占用等量内存好几遍。 */
const HUGE_BYTES = 64 * 1024 * 1024;

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

/** 峰值常驻内存（KiB → MiB）；`maxRSS` 单调递增，差值就是这次操作的峰值增量。 */
function peakRssMb() {
  return process.resourceUsage().maxRSS / 1024;
}

/** 某个进程当前的常驻内存（MiB）；本机不允许 `ps` 时返回 null（跳过服务端采样）。 */
function rssMbOf(pid) {
  const out = spawnSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' });
  const kb = Number((out.stdout || '').trim());
  return Number.isFinite(kb) && kb > 0 ? kb / 1024 : null;
}

/**
 * 在 `fn` 执行期间持续采样常驻内存，返回起点与峰值。
 *
 * 只看「当前 RSS」而不是历史峰值：历史峰值可能早已被别的操作顶高，
 * 那样会让「整份内容进内存」的回归漏网。
 */
async function sampleMemory(read, fn) {
  const base = read();
  let peak = base;
  const timer = setInterval(() => {
    peak = Math.max(peak, read());
  }, 20);
  try {
    const value = await fn();
    return { value, base, peak };
  } finally {
    clearInterval(timer);
  }
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
 * 只用于小体积断言——这个函数会把内容攒在内存里，正是生产路径要避免的做法。
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
  const flow = require('../dist/main/flow.js');
  const wc = require('../dist/main/wc.js');
  const client = new VcsClient(TARGET);

  try {
    step('登录并建库');
    await client.login('alice', PASSWORD);
    const repo = await client.createRepo('big-files');
    console.log(`  ${repo.name} = ${repo.repoId}`);

    step(`提交 ${SMALL_BIG.map((f) => `${f.name}(${f.size / 1024 / 1024} MiB)`).join(' + ')}`);
    fs.mkdirSync(SRC, { recursive: true });
    const files = SMALL_BIG.map((file, index) => {
      const target = path.join(SRC, file.name);
      fs.writeFileSync(target, payload(file.size, index));
      return { path: file.name, file: target, hash: hashBytes(fs.readFileSync(target)) };
    });
    let begin = await client.beginCommit(
      repo.repoId,
      0,
      files.map((file) => ({ path: file.path, kind: 'ADD', hash: file.hash })),
      '提交大文件',
      files.reduce((sum, file) => sum + fs.statSync(file.file).size, 0),
    );
    const pushed = await client.pushFiles(
      begin.sessionId,
      files.map((file) => ({ hash: file.hash, path: file.file })),
    );
    await client.finalizeCommit(begin.sessionId);
    if (pushed !== files.length) fail(`推送对象数不对：${pushed}`);
    console.log(`  已提交 r1，推送 ${pushed} 个对象`);

    step('裸通道检出（默认 4 MiB 消息上限，不带任何通道参数）');
    const plain = await checkoutWithDefaultChannel(
      path.join(ROOT, 'proto', 'repo.proto'),
      client.tokenValue,
      repo.repoId,
    );
    for (const file of files) {
      const got = plain.objects.get(file.hash);
      if (!got || !got.equals(fs.readFileSync(file.file))) fail(`${file.path} 裸通道检出内容不一致`);
    }
    if (plain.maxMessage > 4 * 1024 * 1024) {
      fail(`单条消息 ${plain.maxMessage} 字节，超过默认上限，说明分块没生效`);
    }
    console.log(
      `  ${files.length} 个文件内容一致；单条消息最大 ${plain.maxMessage} 字节（文件最大 ${Math.max(...SMALL_BIG.map((f) => f.size))} 字节）`,
    );

    step(`提交 ${HUGE_BYTES / 1024 / 1024} MiB 文件（内存回归样本）`);
    const hugePath = path.join(SRC, 'huge.bin');
    // 分片写盘、native 流式算摘要：脚本自己不占大内存，后面的内存测量才有意义。
    const hugeFd = fs.openSync(hugePath, 'w');
    const hugeChunk = Buffer.alloc(1024 * 1024);
    for (let written = 0; written < HUGE_BYTES; written += hugeChunk.length) {
      hugeChunk.fill((written / hugeChunk.length) % 251);
      fs.writeSync(hugeFd, hugeChunk);
    }
    fs.closeSync(hugeFd);
    const hugeHash = wc.hashFile(hugePath).hash;
    const head = await client.getHead(repo.repoId);
    begin = await client.beginCommit(
      repo.repoId,
      head.rev,
      [{ path: 'huge.bin', kind: 'ADD', hash: hugeHash }],
      '提交一个很大的文件',
      HUGE_BYTES,
    );
    // 客户端与**服务端**同时采样：两边都不该因为文件变大而吃掉等量内存。
    const serverBase = rssMbOf(server.pid);
    const serverSampler = serverBase === null ? null : setInterval(() => {
      serverPeak = Math.max(serverPeak, rssMbOf(server.pid) ?? 0);
    }, 20);
    let serverPeak = serverBase ?? 0;
    const push = await sampleMemory(
      () => process.memoryUsage().rss / 1024 / 1024,
      () => client.pushFiles(begin.sessionId, [{ hash: hugeHash, path: hugePath }]),
    );
    if (serverSampler) clearInterval(serverSampler);
    const clientPushGrowth = push.peak - push.base;
    await client.finalizeCommit(begin.sessionId);
    console.log(`  已提交 r2（${HUGE_BYTES} 字节）`);
    console.log(
      `  推送期间常驻内存：客户端 +${clientPushGrowth.toFixed(1)} MiB` +
        (serverBase === null
          ? '（本机不允许 ps，跳过服务端采样）'
          : `，服务端 ${serverBase.toFixed(1)} → ${serverPeak.toFixed(1)} MiB`),
    );
    const limit = HUGE_BYTES / 1024 / 1024 / 2;
    if (clientPushGrowth > limit) {
      fail(`推送 ${HUGE_BYTES / 1024 / 1024} MiB 文件时客户端内存涨了 ${clientPushGrowth.toFixed(1)} MiB`);
    }
    if (serverBase !== null && serverPeak - serverBase > limit) {
      fail(
        `推送 ${HUGE_BYTES / 1024 / 1024} MiB 文件时服务端内存涨了 ${(serverPeak - serverBase).toFixed(1)} MiB`,
      );
    }

    step('桌面端正式路径检出（下载落 spool → native 流式铺盘）');
    // 采样「检出期间」的常驻内存：整份内容进内存的话，这里会跳一个文件大小。
    const baseRss = process.memoryUsage().rss / 1024 / 1024;
    let sampledPeak = baseRss;
    const sampler = setInterval(() => {
      sampledPeak = Math.max(sampledPeak, process.memoryUsage().rss / 1024 / 1024);
    }, 20);
    const result = await flow.checkoutRepo(client, repo.repoId, WC);
    clearInterval(sampler);
    const hugeInfo = wc.hashFile(path.join(WC, 'huge.bin'));
    if (hugeInfo.hash !== hugeHash || hugeInfo.size !== HUGE_BYTES) {
      fail(`大文件检出后摘要不一致：${JSON.stringify(hugeInfo)}`);
    }
    for (const file of files) {
      const info = wc.hashFile(path.join(WC, file.path));
      if (info.hash !== file.hash) fail(`${file.path} 检出后摘要不一致`);
    }
    const growth = sampledPeak - baseRss;
    console.log(
      `  检出 r${result.rev}，${result.files} 个文件；` +
        `常驻内存 ${baseRss.toFixed(1)} MiB → 采样峰值 ${sampledPeak.toFixed(1)} MiB（+${growth.toFixed(1)}，` +
        `历史峰值 ${peakRssMb().toFixed(1)} MiB）`,
    );
    if (growth > HUGE_BYTES / 1024 / 1024 / 2) {
      fail(
        `检出 ${HUGE_BYTES / 1024 / 1024} MiB 文件时内存涨了 ${growth.toFixed(1)} MiB，` +
          '说明还有「整份内容进内存」的路径',
      );
    }

    process.stdout.write('\n✓ 大文件冒烟通过（分块传输 / 提交 / 检出 / 内存不随文件增长）\n');
  } finally {
    client.close();
    server.kill();
  }
}

main().catch((err) => {
  process.stderr.write(`\n大文件冒烟失败：${err && err.stack ? err.stack : err}\n`);
  process.exit(1);
});
