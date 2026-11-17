/**
 * 客户端本地设置与凭证持久化。
 *
 * 两件事分开处理，因为它们的敏感度完全不同：
 *   - **连接信息**（服务端地址、上次登录的用户名）明文存在 `userData/settings.json`，
 *     方便下次打开就是上次的状态；
 *   - **token** 用 Electron 的 `safeStorage`（macOS 钥匙串 / Windows DPAPI / Linux libsecret）
 *     加密后再落盘。加密不可用时宁可不存——把凭证明文写进磁盘是最不该做的妥协。
 *
 * 注意这里从不向渲染进程返回 token：渲染进程只需要知道「连没连上、我是谁」。
 *
 * 另一条分界线是**人**：连接信息与长期会话是「这台机器上次用的账号」，只有一份；
 * 而工作副本登记属于账号（`accounts[地址][用户名]`），同一台机器上换账号登录不会
 * 互相看到对方的目录。
 */
import { app, safeStorage } from 'electron';
import * as fs from 'node:fs';
import * as path from 'node:path';

interface SettingsFile {
  target?: string;
  username?: string;
  /** 上次登录拿到的姓名。旧服务端的 WhoAmI 不返回姓名，用它给身份区兜底。 */
  displayName?: string;
  /** base64(safeStorage 密文)。没有加密能力时不会写入。 */
  sealedToken?: string;
  /** 该 token 的过期时间（Unix 秒），仅「30 天免登录」会话会写入。 */
  sessionExpiresAt?: number;
  /**
   * **旧版本**的仓库 → 目录映射：机器级、不分账号，谁登录都看得见。
   * 现在只在新旧过渡时读一次（见 `migrateLegacyDirs`），迁移完立刻删掉。
   */
  repoDirs?: Record<string, string | string[]>;
  /** 按账号隔离的本机登记：`accounts[服务端地址][用户名]`。 */
  accounts?: AccountBook;
}

/** 一个账号在本机留下的、与具体仓库无关的设置。 */
interface AccountSettings {
  /** 仓库 → 本地工作副本目录列表（首个是最近使用的那份）。 */
  repoDirs?: RepoDirMap;
  /** 仓库 → 本机使用记录（决定左侧列表里「常用」的顺序）。 */
  repoUsage?: RepoUsage;
}

/** 服务端地址 → 用户名 → 该账号的本机设置。 */
type AccountBook = Record<string, Record<string, AccountSettings>>;

function settingsPath(): string {
  return path.join(app.getPath('userData'), 'settings.json');
}

function readRaw(): SettingsFile {
  try {
    return JSON.parse(fs.readFileSync(settingsPath(), 'utf8')) as SettingsFile;
  } catch {
    return {};
  }
}

function read(): SettingsFile {
  const file = readRaw();
  // 迁移只做一次（做完全局字段就没了）。写盘失败也不能挡住读：内存里已经是新结构，
  // 本次会话照常工作，下次读到时再迁一次。
  if (migrateLegacyDirs(file)) {
    try {
      write(file);
    } catch {
      /* 磁盘只读之类：本次会话内照常使用 */
    }
  }
  return file;
}

function write(next: SettingsFile): void {
  const file = settingsPath();
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.part`;
  fs.writeFileSync(tmp, JSON.stringify(next, null, 2), { mode: 0o600 });
  fs.renameSync(tmp, file);
}

/** 渲染进程需要的部分：只有地址与用户名，不含任何凭证。 */
export interface Prefs {
  target: string;
  username: string;
  /** 上次登录的姓名（可能为空：旧客户端留下的设置里没有这一项）。 */
  displayName: string;
  /** 是否保存了可用的凭证（用于界面提示「已记住登录」）。 */
  hasToken: boolean;
  /** 当前平台是否支持加密存储。不支持时界面要提示「重启后需重新登录」。 */
  canPersistToken: boolean;
  /** 已保存会话的过期时间（Unix 秒）；0 表示没有长期会话。 */
  sessionExpiresAt: number;
}

export function loadPrefs(): Prefs {
  const file = read();
  return {
    target: file.target ?? '',
    username: file.username ?? '',
    displayName: file.displayName ?? '',
    hasToken: Boolean(file.sealedToken),
    canPersistToken: safeStorage.isEncryptionAvailable(),
    sessionExpiresAt: file.sessionExpiresAt ?? 0,
  };
}

/** 一次登录 / 会话恢复要落盘的东西，缺省项表示「保持原样，不覆盖」。 */
export interface LoginSnapshot {
  token?: string;
  /** 只在 30 天免登录会话传：0 或缺省不改动原来记下的到期时间。 */
  expiresAt?: number;
  /** 拿到了姓名就刷新缓存；服务端没给（旧版本）时传空以保留旧值。 */
  displayName?: string;
}

/**
 * 保存登录结果。
 *
 * - 传 `token`：把凭证加密落盘（30 天免登录会话同时记下 `expiresAt`）；
 * - 不传 `token`：只保留连接信息，清掉凭证——没有拿到 token 或无加密能力时走这条。
 *
 * `displayName` 是纯展示用的兜底：`WhoAmI` 在旧服务端上不返回姓名，恢复会话时
 * 就用上次登录缓存的姓名，避免右上角退回显示账号。
 */
export function saveLogin(
  target: string,
  username: string,
  snapshot: LoginSnapshot = {},
): void {
  const file = read();
  file.target = target;
  file.username = username;
  if (snapshot.displayName !== undefined) {
    const name = snapshot.displayName.trim();
    if (name) file.displayName = name;
    else delete file.displayName;
  }
  const { token, expiresAt = 0 } = snapshot;
  if (token && safeStorage.isEncryptionAvailable()) {
    file.sealedToken = safeStorage.encryptString(token).toString('base64');
    if (expiresAt > 0) file.sessionExpiresAt = expiresAt;
  } else {
    // 明文令牌绝不落盘；保留连接信息即可。
    delete file.sealedToken;
    delete file.sessionExpiresAt;
  }
  write(file);
}

/** 保存用户在登录页「设置」里自定义的服务端地址。
 *
 * 打包默认地址在渲染层的 config.js 里，主进程不读它：这里存的是**覆盖值**，
 * 传空字符串表示恢复默认。换地址后旧凭证一律作废，避免把 A 服务器的 token
 * 拿到 B 服务器去用。
 */
export function saveTarget(target: string): void {
  const file = read();
  if (target) file.target = target;
  else delete file.target;
  delete file.sealedToken;
  delete file.sessionExpiresAt;
  // 换了服务端，上个账号的姓名也不该带过去。
  delete file.displayName;
  write(file);
}

/** 取回上次的 token；解密失败（换机器、钥匙串重置）就当作没有。 */
export function loadToken(): string | null {
  const file = read();
  if (!file.sealedToken || !safeStorage.isEncryptionAvailable()) return null;
  try {
    return safeStorage.decryptString(Buffer.from(file.sealedToken, 'base64'));
  } catch {
    return null;
  }
}

export function clearToken(): void {
  const file = read();
  if (!file.sealedToken && !file.sessionExpiresAt) return;
  delete file.sealedToken;
  delete file.sessionExpiresAt;
  write(file);
}

/**
 * 仓库 → 该仓库在本机的**所有**工作副本目录；第一个是最近使用的一份。
 *
 * 一个仓库可以检出多份（不同目录、各自的本地改动互不影响），所以这里是一对多。
 * 这份登记**按账号隔离**：工作副本目录是「我用这个账号在这台机器上检出的目录」，
 * 不是这台机器的公共资产。A 在 s1、B 在 s2 检出同一个仓库，切回 A 必须还是 s1——
 * 一台机器上换账号登录（共用工作站很常见）不该把别人的目录、更不该把别人的
 * 本地改动带过来。
 */
export type RepoDirMap = Record<string, string[]>;

/** 一份登录身份：服务端地址 + 服务端确认过的用户名。 */
export interface Account {
  target: string;
  username: string;
}

/** 只收绝对路径：界面可能传来误填的相对路径，写进设置下次就对不上了。 */
function absoluteDir(value: unknown): string | null {
  if (typeof value !== 'string') return null;
  const dir = value.trim();
  if (!dir || (!dir.startsWith('/') && !/^[A-Za-z]:[\\/]/.test(dir))) return null;
  return dir;
}

/**
 * 归一化绑定关系：兼容旧设置的单字符串、去掉重复与相对路径。
 * 「一个目录只属于一个仓库」在这里兜底——设置文件可能被手工改过，界面层也会自行去重。
 */
function normalizeRepoDirs(dirs: unknown): RepoDirMap {
  if (!dirs || typeof dirs !== 'object' || Array.isArray(dirs)) return {};
  const clean: RepoDirMap = {};
  const seen = new Set<string>();
  for (const [repoId, raw] of Object.entries(dirs)) {
    const values: unknown[] = Array.isArray(raw) ? raw : [raw];
    const list: string[] = [];
    for (const value of values) {
      const dir = absoluteDir(value);
      // 先出现的仓库优先：同一路径绝不挂在两个仓库下。
      if (!dir || seen.has(dir) || list.includes(dir)) continue;
      seen.add(dir);
      list.push(dir);
    }
    if (list.length > 0) clean[repoId] = list;
  }
  return clean;
}

/** 账号名与地址都可能来自界面输入，先去掉首尾空格；缺一半就当作「没有身份」。 */
function normalizeAccount(account: Account | null | undefined): Account | null {
  if (!account) return null;
  const target = (account.target ?? '').trim();
  const username = (account.username ?? '').trim();
  if (!target || !username) return null;
  return { target, username };
}

function readAccount(file: SettingsFile, account: Account): AccountSettings | null {
  const users = file.accounts?.[account.target];
  return (users && users[account.username]) || null;
}

function ensureAccount(file: SettingsFile, account: Account): AccountSettings {
  const book = (file.accounts ??= {});
  const users = (book[account.target] ??= {});
  return (users[account.username] ??= {});
}

/**
 * 旧版本把工作副本登记当成机器级的：一个全局 `repoDirs`，谁登录都看得见——这正是
 * 「A 在 s1、B 在 s2，切回 A 却看到 s2」的原因。
 *
 * 老数据只有一份，只能归给**上次登录的那个账号**：`target` / `username` 就是那次
 * 登录写下的。归不出来时（设置文件被手工改过）宁可丢掉——把上一个账号的目录塞给
 * 下一个登录的人，正是要修的问题。
 */
function migrateLegacyDirs(file: SettingsFile): boolean {
  const legacy = file.repoDirs;
  if (!legacy) return false;
  delete file.repoDirs;
  const account = normalizeAccount({ target: file.target ?? '', username: file.username ?? '' });
  const dirs = normalizeRepoDirs(legacy);
  if (account && Object.keys(dirs).length > 0) {
    const slot = ensureAccount(file, account);
    // 已经有账号级登记时不覆盖：那份是更可信的新数据。
    if (!slot.repoDirs) slot.repoDirs = dirs;
  }
  return true;
}

/** 当前账号在本机记住的工作副本登记；没有登录身份时返回空表（什么都不给看）。 */
export function loadRepoDirs(account: Account | null): RepoDirMap {
  const who = normalizeAccount(account);
  if (!who) return {};
  const slot = readAccount(read(), who);
  return normalizeRepoDirs(slot ? slot.repoDirs : undefined);
}

/**
 * 保存**当前账号**的登记；列表顺序即最近使用顺序，重启后仍停在上次那份工作副本上。
 *
 * 没有身份就不写：一份没有主人的目录记忆，下次只会变成别人的。
 */
export function saveRepoDirs(account: Account | null, dirs: RepoDirMap): void {
  const who = normalizeAccount(account);
  if (!who) throw new Error('尚未登录，工作副本登记无处可存');
  const file = read();
  const clean = normalizeRepoDirs(dirs);
  const slot = ensureAccount(file, who);
  if (Object.keys(clean).length === 0) delete slot.repoDirs;
  else slot.repoDirs = clean;
  // 一个目录都不剩时把账号条目收掉，别在设置里留一个空壳——但使用记录还留着的话，
  // 这份条目就还有内容，不能连它一起删。
  if (!slot.repoDirs && !slot.repoUsage) {
    const users = file.accounts?.[who.target];
    if (users) {
      delete users[who.username];
      if (Object.keys(users).length === 0) delete file.accounts![who.target];
    }
  }
  write(file);
}

// ---------------- 仓库使用记录（左侧列表的「常用」顺序） ----------------

/**
 * 一个仓库在本机的使用记录。
 *
 * `score` 是**衰减过的使用次数**：每用一次 +1，同时把旧分数往前折算，所以
 * 「三个月前连用 50 次」不会永远压着「上周用了 3 次」。折算发生在写入时，
 * `score` 的含义是「截至 `last` 那一刻的等效次数」；要比两个仓库谁更常用，
 * 得先用 `decayed()` 折算到同一个时刻。
 */
interface RepoUsageEntry {
  score: number;
  last: number;
}

type RepoUsage = Record<string, RepoUsageEntry>;

/** 给渲染进程的顺序权重：`weight` 已经折算到此刻，直接比大小即可。 */
export interface RepoWeight {
  weight: number;
  /** 最后一次使用的时刻（Unix 毫秒），用于权重相同时的次级比较。 */
  last: number;
}

/** 半衰期：两周没用，权重折半。「常用」要跟着当前的习惯走，不是历史总次数。 */
const USAGE_HALF_LIFE_MS = 14 * 24 * 60 * 60 * 1000;

/** 权重低于它（约两个多月没用）就不再参与排序，也不必继续存在设置里。 */
const USAGE_FLOOR = 1 / 64;

/** 保留期：三个月没碰过的仓库直接忘掉——「常用」是当前的习惯，不是一份履历。 */
const USAGE_RETENTION_MS = 90 * 24 * 60 * 60 * 1000;

/** 最多记这么多个仓库：设置文件不该因为仓库多而无上限长大。 */
const USAGE_MAX = 200;

/** 把「截至 `last` 的分数」折算到 `now`。 */
function decayed(entry: RepoUsageEntry, now: number): number {
  const idle = Math.max(0, now - entry.last);
  return entry.score * Math.pow(0.5, idle / USAGE_HALF_LIFE_MS);
}

/** 归一化：丢掉坏值（手工改过的设置里什么都可能有）与早已过期的记录，再按权重截断。 */
function normalizeUsage(raw: unknown): RepoUsage {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return {};
  const now = Date.now();
  const clean: RepoUsage = {};
  for (const [repoId, value] of Object.entries(raw as Record<string, unknown>)) {
    if (!repoId || !value || typeof value !== 'object') continue;
    const entry = value as Partial<RepoUsageEntry>;
    const score = Number(entry.score);
    const last = Number(entry.last);
    if (!Number.isFinite(score) || !Number.isFinite(last)) continue;
    if (score <= 0 || last <= 0) continue;
    // 未来时间（改过系统时钟）按「就是现在」处理，别让它永远排在第一位。
    const kept: RepoUsageEntry = { score, last: Math.min(last, now) };
    if (now - kept.last > USAGE_RETENTION_MS) continue;
    if (decayed(kept, now) < USAGE_FLOOR) continue;
    clean[repoId] = kept;
  }
  const ids = Object.keys(clean);
  if (ids.length <= USAGE_MAX) return clean;
  ids.sort((a, b) => decayed(clean[b], now) - decayed(clean[a], now));
  const trimmed: RepoUsage = {};
  for (const id of ids.slice(0, USAGE_MAX)) trimmed[id] = clean[id];
  return trimmed;
}

function weightsOf(usage: RepoUsage): Record<string, RepoWeight> {
  const now = Date.now();
  const out: Record<string, RepoWeight> = {};
  for (const [repoId, entry] of Object.entries(usage)) {
    out[repoId] = { weight: decayed(entry, now), last: entry.last };
  }
  return out;
}

/**
 * 当前账号的仓库使用权重。没有登录身份时返回空表——使用记录是本机偏好，
 * 不是服务器上的仓库属性，不该在登录页 / 换账号时被看到。
 */
export function loadRepoUsage(account: Account | null): Record<string, RepoWeight> {
  const who = normalizeAccount(account);
  if (!who) return {};
  const slot = readAccount(read(), who);
  return weightsOf(normalizeUsage(slot ? slot.repoUsage : undefined));
}

/**
 * 记一次使用（切到某个仓库 / 把它检出到本地），并回传折算后的新权重表。
 *
 * 只由**用户真的用了这个仓库**的动作触发：自动选中第一个仓库不算——那会让
 * 「上次排在第一个的」每次启动都再得一分，永远钉在顶部。
 */
export function bumpRepoUsage(
  account: Account | null,
  repoId: string,
): Record<string, RepoWeight> {
  const who = normalizeAccount(account);
  const id = (repoId ?? '').trim();
  if (!who) return {};
  if (!id) return loadRepoUsage(who);
  const file = read();
  const slot = ensureAccount(file, who);
  const usage = normalizeUsage(slot.repoUsage);
  const now = Date.now();
  const prev = usage[id];
  usage[id] = { score: (prev ? decayed(prev, now) : 0) + 1, last: now };
  slot.repoUsage = usage;
  try {
    write(file);
  } catch {
    // 磁盘只读之类：本次会话内的顺序照常（返回的就是算好的新权重），下次启动从头记。
    // 「常用」只是排序偏好，绝不能因为存不下就挡住检出 / 切换仓库。
  }
  return weightsOf(usage);
}
