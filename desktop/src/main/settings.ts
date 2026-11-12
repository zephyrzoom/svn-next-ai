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
  /** base64(safeStorage 密文)。没有加密能力时不会写入。 */
  sealedToken?: string;
  /** 该 token 的过期时间（Unix 秒），仅「30 天免登录」会话会写入。 */
  sessionExpiresAt?: number;
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
    hasToken: Boolean(file.sealedToken),
    canPersistToken: safeStorage.isEncryptionAvailable(),
    sessionExpiresAt: file.sessionExpiresAt ?? 0,
  };
}

/**
 * 保存登录结果。
 *
 * - 传 `token`：把凭证加密落盘（「记住我」的 30 天会话同时记下 `expiresAt`）；
 * - 不传 `token`：只保留服务端地址与用户名，清掉凭证——用于「本次会话有效」的登录
 *   以及用旧 token 恢复会话时刷新用户名，不覆盖原来的到期时间。
 */
export function saveLogin(
  target: string,
  username: string,
  token?: string,
  expiresAt = 0,
): void {
  const file = read();
  file.target = target;
  file.username = username;
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
