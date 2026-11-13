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
  /** 仓库 → 本地工作副本目录。同一条记录，重启后才知道哪个仓库在本地有目录。 */
  repoDirs?: Record<string, string>;
}

function settingsPath(): string {
  return path.join(app.getPath('userData'), 'settings.json');
}

function read(): SettingsFile {
  try {
    return JSON.parse(fs.readFileSync(settingsPath(), 'utf8')) as SettingsFile;
  } catch {
    return {};
  }
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
  /** 只在「记住我」登录时传：0 或缺省不改动原来记下的到期时间。 */
  expiresAt?: number;
  /** 拿到了姓名就刷新缓存；服务端没给（旧版本）时传空以保留旧值。 */
  displayName?: string;
}

/**
 * 保存登录结果。
 *
 * - 传 `token`：把凭证加密落盘（「记住我」的 30 天会话同时记下 `expiresAt`）；
 * - 不传 `token`：只保留连接信息，清掉凭证——用于「本次会话有效」的登录。
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
 * 仓库 → 本地工作副本目录的绑定。
 *
 * 工作副本是「这台机器上的一份拷贝」，所以这份记忆跟着机器走，不跟账号走：换个账号
 * 登录，同一个仓库的目录依然对得上。里面没有任何凭证，只有路径。
 */
export function loadRepoDirs(): Record<string, string> {
  const file = read();
  const dirs = file.repoDirs;
  if (!dirs || typeof dirs !== 'object') return {};
  const clean: Record<string, string> = {};
  for (const [repoId, dir] of Object.entries(dirs)) {
    if (typeof dir === 'string' && dir.trim()) clean[repoId] = dir;
  }
  return clean;
}

/** 保存绑定关系；只收绝对路径，避免把界面上误传的相对路径写进设置。 */
export function saveRepoDirs(dirs: Record<string, string>): void {
  const file = read();
  const clean: Record<string, string> = {};
  if (dirs && typeof dirs === 'object') {
    for (const [repoId, dir] of Object.entries(dirs)) {
      if (typeof dir !== 'string' || !dir.trim()) continue;
      if (!dir.startsWith('/') && !/^[A-Za-z]:[\\/]/.test(dir)) continue;
      clean[repoId] = dir;
    }
  }
  if (Object.keys(clean).length === 0) delete file.repoDirs;
  else file.repoDirs = clean;
  write(file);
}
