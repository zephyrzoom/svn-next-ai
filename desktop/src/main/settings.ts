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
}

export function loadPrefs(): Prefs {
  const file = read();
  return {
    target: file.target ?? '',
    username: file.username ?? '',
    hasToken: Boolean(file.sealedToken),
    canPersistToken: safeStorage.isEncryptionAvailable(),
  };
}

export function saveLogin(target: string, username: string, token: string): void {
  const file = read();
  file.target = target;
  file.username = username;
  if (safeStorage.isEncryptionAvailable()) {
    file.sealedToken = safeStorage.encryptString(token).toString('base64');
  } else {
    // 明文令牌绝不落盘；保留连接信息即可。
    delete file.sealedToken;
  }
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
  if (!file.sealedToken) return;
  delete file.sealedToken;
  write(file);
}
