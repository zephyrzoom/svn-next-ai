/**
 * 渲染进程：桌面端「仓库使用」客户端的界面逻辑。
 *
 * 这里没有任何网络或文件系统能力——全部经 `window.vcs`（preload 暴露）转交主进程。
 * 每个调用都返回统一信封 `{ok, data} | {ok:false, code, message}`，业务错误直接展示
 * `message`；已登录状态下遇到认证类错误（11/12/19/20）自动退回登录页。
 *
 * 界面分两层：启动先显示「登录页」（本地账号 / LDAP 账号两个 tab、注册入口、
 * 服务器地址「设置」），成功后才切到「主界面」。默认服务器地址由打包配置
 * （config.js）注入，用户在设置里改的地址覆盖它并持久化。
 * 勾选 30 天免登录时，主进程用系统加密存储保存凭证，启动时 tryResume 直接恢复会话。
 *
 * 管理与审批（成员、配额、空间治理、目录配置、审批台）由服务端 Web 管理端承载，
 * 桌面端只保留登录、账号自助、仓库使用与工作副本这条主线。
 */
const api = window.vcs;

const AUTH_CODES = new Set([11, 12, 19, 20]);

const state = {
  user: null,
  /** 打包时注入的默认地址；登录页不显示地址输入框。 */
  defaultTarget: '127.0.0.1:50051',
  /** 用户在「设置」里保存的覆盖值，空串表示用默认地址。 */
  target: '',
  /** 当前选中的账号类型 tab：LOCAL / LDAP，登录时按它选身份来源。 */
  source: 'LOCAL',
  authMode: 'login',
  sessionExpiresAt: 0,
  canPersistToken: true,
  conflicts: [],
  conflict: null,
  repos: [],
  selectedRepoId: null,
  dir: '',
  changes: [],
  diff: [],
  cwd: '',
};

const el = (id) => document.getElementById(id);

function clearBanner() {
  const node = el('banner');
  node.className = 'banner hidden';
  node.textContent = '';
}

/** 登录页内的提示条（主界面隐藏时所有反馈都落在这里）。 */
function authMessage(message = '', kind = 'info') {
  const node = el('auth-message');
  if (!message) {
    node.className = 'auth-message hidden';
    node.textContent = '';
    return;
  }
  node.className = `auth-message${kind === 'error' ? ' error' : ''}`;
  node.textContent = message;
}

/** 主界面提示条；未登录时自动转给登录页，避免提示落在看不见的地方。 */
function banner(message = '', kind = 'info') {
  if (!state.user) {
    authMessage(message, kind);
    return;
  }
  if (!message) {
    clearBanner();
    return;
  }
  const node = el('banner');
  node.className = `banner${kind === 'error' ? ' error' : ''}`;
  node.textContent = message;
}

/** 解包信封：失败时抛错并展示，成功返回 data。 */
function unwrap(res, action) {
  if (res && res.ok) return res.data;
  const message = res && res.message ? res.message : '未知错误';
  if (res && AUTH_CODES.has(res.code) && state.user) {
    // 已登录却拿到认证错误：不要只弹个提示，直接回登录页让人重新登录。
    showLogin(`登录状态已失效（${message}），请重新登录。`);
    throw new Error(message);
  }
  const suffix = res && AUTH_CODES.has(res.code) ? '\n请重新登录。' : '';
  banner(`${action}失败：${message}${suffix}`, 'error');
  throw new Error(message);
}

async function guard(action, run) {
  try {
    return await run();
  } catch (err) {
    const shown = state.user ? el('banner').textContent : el('auth-message').textContent;
    if (!shown) {
      banner(`${action}失败：${err.message}`, 'error');
    }
    return undefined;
  }
}

// ---------------- 视图切换 ----------------

/**
 * 左侧菜单只切换右侧视图与高亮态，纯界面操作：不触发任何网络请求或副作用。
 */
function setView(name) {
  for (const item of document.querySelectorAll('.nav-item')) {
    item.classList.toggle('active', item.dataset.view === name);
  }
  for (const view of document.querySelectorAll('.view')) {
    view.classList.toggle('hidden', view.dataset.view !== name);
  }
}

for (const item of document.querySelectorAll('.nav-item')) {
  item.addEventListener('click', () => setView(item.dataset.view));
}

// ---------------- 登录页 / 主界面切换 ----------------

/** config.js 缺失时的兜底（例如直接打开源码目录里的 html）。 */
const FALLBACK_TARGET = '127.0.0.1:50051';

function effectiveTarget() {
  return state.target || state.defaultTarget || FALLBACK_TARGET;
}

function isCustomTarget() {
  return Boolean(state.target && state.target !== state.defaultTarget);
}

function renderTarget() {
  el('current-target').textContent =
    `${effectiveTarget()}${isCustomTarget() ? '（自定义）' : '（默认）'}`;
}

/** 「设置」面板：只在用户点开时暴露地址，默认地址来自打包配置。 */
function openSettings() {
  el('server-target').value = effectiveTarget();
  el('target-note').textContent =
    `默认地址：${state.defaultTarget}（打包时由 build-config.json 配置）`;
  el('server-settings').classList.remove('hidden');
  authMessage('');
  el('server-target').focus();
}

function closeSettings() {
  el('server-settings').classList.add('hidden');
}

async function saveTargetSetting() {
  const value = el('server-target').value.trim();
  if (!/^[A-Za-z0-9._:-]+:\d{1,5}$/.test(value)) {
    authMessage('服务器地址格式应为 host:port，例如 127.0.0.1:50051', 'error');
    return;
  }
  // 填回默认地址就等于「没有覆盖」，本机不再存这个值。
  const override = value === state.defaultTarget ? '' : value;
  if (override !== state.target) {
    unwrap(await api.setTarget(override), '保存服务器地址');
    state.target = override;
  }
  closeSettings();
  renderTarget();
  authMessage(override ? `服务器地址已改为 ${value}。` : '已恢复为打包默认地址。');
}

async function resetTargetSetting() {
  if (state.target) unwrap(await api.setTarget(''), '恢复默认服务器地址');
  state.target = '';
  closeSettings();
  renderTarget();
  authMessage(`已恢复默认服务器地址 ${state.defaultTarget}。`);
}

/** 账号类型 tab：切到哪个就按哪种身份登录；LDAP 不需要注册。 */
function setSource(source) {
  state.source = source === 'LDAP' ? 'LDAP' : 'LOCAL';
  for (const tab of document.querySelectorAll('.auth-tab')) {
    const active = tab.dataset.source === state.source;
    tab.classList.toggle('active', active);
    tab.setAttribute('aria-selected', String(active));
  }
  setAuthMode(state.source === 'LDAP' ? 'login' : state.authMode);
}

function formatDate(seconds) {
  const date = new Date(seconds * 1000);
  const pad = (value) => String(value).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
}

/** 会话说明：0 表示本次会话有效，没有勾选 30 天免登录。 */
function sessionText(expiresAt) {
  return expiresAt > 0
    ? `30 天内免登录，至 ${formatDate(expiresAt)}`
    : '仅本次会话有效，关闭应用后需重新登录';
}

/** 登录页里「登录 / 申请注册」两个表单的切换，纯界面操作。 */
function setAuthMode(mode) {
  state.authMode = mode;
  const register = mode === 'register';
  el('login-form').classList.toggle('hidden', register);
  el('register-form').classList.toggle('hidden', !register);
  el('show-register').classList.toggle('hidden', register || state.source !== 'LOCAL');
  el('show-login').classList.toggle('hidden', !register);
  el('auth-title').textContent = register ? '申请注册' : '登录';
  el('auth-sub').textContent = register
    ? '填写用户名、姓名与密码，提交后等待管理员审批。'
    : state.source === 'LDAP'
      ? '使用 LDAP 账号密码登录，无需注册。'
      : '使用本地账号登录。';
  const note = register
    ? '密码在注册时设置，审批通过后直接可用，不需要激活链接。'
    : state.source === 'LDAP'
      ? 'LDAP 账号由目录服务管理，密码不在本机保存；需要账号请联系管理员。'
      : '注册时自己设置密码；管理员审批通过后即可用它登录。';
  el('auth-note').textContent = state.canPersistToken
    ? note
    : '当前系统不支持加密存储凭证，关闭应用后需要重新登录。';
  authMessage('');
}

/** 回到登录页：清掉主界面状态，登录表单回到默认态。 */
function showLogin(message = '') {
  state.user = null;
  el('app-shell').classList.add('hidden');
  el('auth-screen').classList.remove('hidden');
  clearBanner();
  closeSettings();
  renderTarget();
  setAuthMode('login');
  el('password').value = '';
  if (message) authMessage(message);
  else el('username').focus();
}

function showApp() {
  el('auth-screen').classList.add('hidden');
  el('app-shell').classList.remove('hidden');
  authMessage('');
}

// ---------------- 登录、注册与会话 ----------------

async function connect() {
  const target = effectiveTarget();
  const username = el('username').value.trim();
  const password = el('password').value;
  const rememberMe = el('remember').checked;
  if (!username || !password) {
    authMessage('请填写用户名与密码', 'error');
    return;
  }
  const session = unwrap(
    await api.connect(target, username, password, state.source, rememberMe),
    '登录',
  );
  el('password').value = '';
  authMessage('');
  await applySession(session, { target });
}

/** 登录成功与「用保存的凭证恢复」共用同一段收尾逻辑。 */
async function applySession(session, { target = '', resumed = false } = {}) {
  const { user, expiresAt } = session;
  state.user = user;
  const usedTarget = target || effectiveTarget();
  state.target = usedTarget;
  state.sessionExpiresAt = Number(expiresAt || 0);
  el('who').textContent = `${user.displayName || user.username} · ${roleLabel(user.role)}`;
  el('server-label').textContent = usedTarget;
  el('account-name').textContent = `${user.displayName || user.username}（${user.username}）`;
  el('account-role').textContent = roleLabel(user.role);
  el('account-target').textContent = usedTarget;
  el('session-until').textContent = sessionText(state.sessionExpiresAt);
  el('session-hint').textContent = state.sessionExpiresAt > 0
    ? `免登录至 ${formatDate(state.sessionExpiresAt)}`
    : '本次会话';
  showApp();
  await loadRepos();
  setView('workcopy');
  banner(resumed ? `已用保存的凭证恢复会话：${user.username}` : '');
}

function roleLabel(role) {
  return { 1: '只读', 2: '可写', 3: '管理员' }[role] || '未知角色';
}

async function doRegister() {
  const target = effectiveTarget();
  const username = el('reg-username').value.trim();
  const displayName = el('reg-name').value.trim();
  const password = el('reg-password').value;
  const reason = el('reg-reason').value.trim();
  if (!username || !displayName || !password) {
    authMessage('请填写用户名、姓名与密码', 'error');
    return;
  }
  const result = unwrap(
    await api.register(target, username, displayName, password, reason),
    '提交注册申请',
  );
  el('reg-password').value = '';
  setAuthMode('login');
  el('username').value = username;
  if (result.autoApproved) {
    authMessage(
      `你是全库第一个用户，已自动成为管理员（申请号 ${result.requestId}）。\n` +
        '用户名已填好，输入刚才设置的密码即可登录。',
    );
  } else {
    authMessage(
      `注册申请已提交（${result.requestId}）。\n` +
        '账号待审批；管理员通过后，用刚才设置的密码登录即可。',
    );
  }
  el('password').focus();
}

// ---------------- 仓库列表与只读容量 ----------------

async function loadRepos() {
  const repos = unwrap(await api.listRepos(), '加载仓库');
  state.repos = repos;
  renderRepos();
  if (!state.selectedRepoId && repos.length > 0) {
    selectRepo(repos[0].repoId);
  }
}

function renderRepos() {
  const list = el('repos');
  list.innerHTML = '';
  if (state.repos.length === 0) {
    list.innerHTML = '<li class="empty">还没有可用的仓库，可在右侧提交建库申请</li>';
    return;
  }
  for (const repo of state.repos) {
    const li = document.createElement('li');
    li.className = repo.repoId === state.selectedRepoId ? 'active' : '';
    li.innerHTML = `<span class="name"></span><span class="badge">r${repo.headRev}</span>`;
    li.querySelector('.name').textContent = repo.name;
    li.addEventListener('click', () => selectRepo(repo.repoId));
    list.appendChild(li);
  }
}

function selectRepo(repoId) {
  state.selectedRepoId = repoId;
  renderRepos();
  void loadLog();
  void loadQuota(repoId);
}

/** 容量是只读信息：选中仓库后仅刷新这一条，不再触发任何治理类调用。 */
async function loadQuota(repoId) {
  const repo = state.repos.find((item) => item.repoId === repoId);
  el('quota-repo').textContent = repo ? repo.name : '';
  const quota = unwrap(await api.getQuota(repoId), '加载容量');
  renderQuota(quota);
}

function humanBytes(bytes) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} B` : `${value.toFixed(1)} ${units[unit]}`;
}

function quotaSourceLabel(source) {
  return { 1: '单仓库覆盖', 2: '全局默认', 3: '系统兜底' }[source] || '未知来源';
}

function renderQuota(quota) {
  const ratio = quota.limitBytes > 0 ? quota.usedBytes / quota.limitBytes : 0;
  const fill = el('quota-fill');
  fill.style.width = `${Math.min(100, Math.round(ratio * 100))}%`;
  fill.className = ratio >= 0.95 ? 'danger' : ratio >= 0.85 ? 'warn' : '';
  el('quota-text').textContent =
    `已用 ${humanBytes(quota.usedBytes)} / 配额上限 ${humanBytes(quota.limitBytes)}` +
    `（来源：${quotaSourceLabel(quota.source)}）`;
}

// ---------------- 建库申请 ----------------

async function requestRepo() {
  const name = el('request-repo').value.trim();
  const reason = el('request-reason').value.trim();
  if (!name) {
    banner('请填写仓库名', 'error');
    return;
  }
  const result = unwrap(await api.requestRepo(name, reason), '提交建库申请');
  el('request-repo').value = '';
  el('request-reason').value = '';
  banner(
    `建库申请已提交（${result.requestId}）。审批通过后你会自动成为 owner，仓库会出现在列表里。`,
  );
}

// ---------------- 冲突三栏 ----------------

function highlight(node) {
  for (const active of el('changes').querySelectorAll('li.active')) {
    active.classList.remove('active');
  }
  node.classList.add('active');
}

/** 打开一条冲突：三栏展示 base / mine / theirs，编辑框默认填 mine。 */
async function showConflict(path, node) {
  highlight(node);
  const detail = unwrap(
    await api.conflictDetail(el('dir').value.trim(), path),
    '读取冲突内容',
  );
  state.conflict = { path, kind: detail.kind };
  el('diff-path').textContent = `${path}（${detail.kind} 冲突）`;
  el('diff').classList.add('hidden');
  el('conflict-editor').classList.remove('hidden');

  const render = (value, label) => {
    if (detail.binary) return '（二进制内容，无法文本合并）';
    if (value === null) return `（${label} 不存在）`;
    return value === '' ? '（空文件）' : value;
  };
  el('conflict-base').textContent = render(detail.base, 'base');
  el('conflict-mine').textContent = render(detail.mine, 'mine');
  el('conflict-theirs').textContent = render(detail.theirs, 'theirs');
  el('conflict-result').value = detail.mine ?? '';
  el('conflict-result').disabled = detail.binary;
  el('save-merge').disabled = detail.binary;
}

function closeConflict() {
  state.conflict = null;
  el('conflict-editor').classList.add('hidden');
  el('diff').classList.remove('hidden');
}

/**
 * 解决冲突。四种选择对应四种真实意图，界面不做「自动合并」——
 * 那需要在三方之上猜语义，猜错比重来一次代价大得多。
 */
async function resolveConflict(choice) {
  if (!state.conflict) return;
  const dir = el('dir').value.trim();
  const { path } = state.conflict;
  const content = choice === 'custom' ? el('conflict-result').value : undefined;
  unwrap(await api.resolveConflict(dir, path, choice, content), '解决冲突');
  closeConflict();
  const labels = {
    mine: '已保留你的版本',
    theirs: '已采用服务端版本',
    custom: '已保存合并结果',
    delete: '已删除该文件',
  };
  banner(`${path}：${labels[choice]}，冲突已清除。`);
  await refreshStatus();
}

// ---------------- 检出与工作副本 ----------------

async function pickDir() {
  const dir = unwrap(await api.pickDir(), '选择目录');
  if (dir) el('dir').value = dir;
}

async function checkout() {
  const dir = el('dir').value.trim();
  if (!state.selectedRepoId) {
    banner('先选择要检出的仓库', 'error');
    return;
  }
  if (!dir) {
    banner('请填写检出目录', 'error');
    return;
  }
  const result = unwrap(
    await api.checkout(state.selectedRepoId, dir),
    '检出',
  );
  state.dir = result.dir;
  state.cwd = result.dir;
  el('wc-path').textContent = `${result.dir} @ r${result.rev}`;
  banner(`已检出 r${result.rev}，共 ${result.files} 个文件。`);
  await refreshStatus();
  await loadLog();
}

async function refreshStatus() {
  const dir = el('dir').value.trim();
  if (!dir) {
    banner('请先检出或填写工作副本目录', 'error');
    return;
  }
  const data = unwrap(await api.status(dir), '扫描工作副本');
  state.dir = dir;
  state.changes = data.entries;
  state.conflicts = data.conflicts || [];
  state.cwd = dir;
  el('wc-path').textContent =
    `${dir} @ r${data.rev}` + (state.conflicts.length ? ` · ${state.conflicts.length} 个冲突` : '');
  renderChanges();
  closeConflict();
  const diffs = unwrap(await api.diff(dir), '生成差异');
  state.diff = diffs;
  if (state.changes.length === 0) {
    el('diff').textContent = '工作副本干净';
    el('diff-path').textContent = '';
  }
}

function renderChanges() {
  const list = el('changes');
  list.innerHTML = '';
  if (state.changes.length === 0 && state.conflicts.length === 0) {
    list.innerHTML = '<li class="empty">没有变更</li>';
    return;
  }
  const conflicted = new Set(state.conflicts.map((item) => item.path));
  for (const change of state.changes) {
    const li = document.createElement('li');
    const marker =
      change.kind === 'CONFLICT' || conflicted.has(change.path)
        ? 'C'
        : change.kind === 'ADD'
          ? 'A'
          : change.kind === 'MODIFY'
            ? 'M'
            : 'D';
    li.innerHTML = `<span class="badge ${marker}"></span><span class="name"></span>`;
    li.querySelector('.badge').textContent = marker;
    li.querySelector('.name').textContent = change.path;
    li.addEventListener('click', () => {
      if (marker === 'C') {
        void guard('读取冲突', () => showConflict(change.path, li));
      } else {
        showDiff(change.path, li);
      }
    });
    list.appendChild(li);
  }
}

function showDiff(path, node) {
  for (const active of el('changes').querySelectorAll('li.active')) {
    active.classList.remove('active');
  }
  node.classList.add('active');
  el('diff-path').textContent = path;
  const entry = state.diff.find((item) => item.path === path);
  el('diff').textContent = entry ? entry.text : '（无文本差异，可能是二进制文件或新增/删除）';
}

async function commit() {
  const dir = el('dir').value.trim();
  const message = el('message').value.trim();
  if (!dir || !message) {
    banner('请填写工作副本目录与提交信息', 'error');
    return;
  }
  const result = unwrap(await api.commit(dir, message), '提交');
  el('message').value = '';
  banner(
    `已提交 r${result.rev}：新增 ${result.added}，修改 ${result.modified}，删除 ${result.deleted}` +
      `（上传 ${result.pushed} 个对象，服务端已有内容不重复上传）`,
  );
  await refreshStatus();
  await loadRepos();
  await loadLog();
}

async function update() {
  const dir = el('dir').value.trim();
  if (!dir) {
    banner('请先检出或填写工作副本目录', 'error');
    return;
  }
  const result = unwrap(await api.update(dir), '更新');
  const parts = [
    `更新 ${result.updated}`,
    `新增 ${result.added}`,
    `删除 ${result.removed}`,
  ];
  if (result.conflicts || result.treeConflicts) {
    parts.push(`冲突 ${result.conflicts} 处内容 / ${result.treeConflicts} 处树`);
    parts.push('本地改动保持原样，可在变更列表里核对后还原或手工合并');
  }
  banner(`已更新到 r${result.rev}：${parts.join('，')}`);
  await refreshStatus();
  await loadLog();
}

async function revertAll() {
  const dir = el('dir').value.trim();
  if (!dir) {
    banner('请先检出或填写工作副本目录', 'error');
    return;
  }
  const restored = unwrap(await api.revert(dir, []), '还原');
  banner(`已把 ${restored} 个条目还原到基线内容（未纳入版本控制的文件不会被删除）`);
  await refreshStatus();
}

async function disconnect() {
  unwrap(await api.disconnect(), '退出登录');
  state.repos = [];
  state.selectedRepoId = null;
  state.changes = [];
  state.sessionExpiresAt = 0;
  el('who').textContent = '';
  el('session-hint').textContent = '';
  el('repos').innerHTML = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('wc-path').textContent = '';
  showLogin('已退出登录。');
}

// ---------------- 历史 ----------------

async function loadLog() {
  if (!state.selectedRepoId) return;
  const revisions = unwrap(await api.log(state.selectedRepoId, 20), '加载历史');
  const list = el('log');
  list.innerHTML = '';
  if (revisions.length === 0) {
    list.innerHTML = '<li class="empty">还没有提交</li>';
    return;
  }
  for (const revision of revisions) {
    const li = document.createElement('li');
    li.innerHTML = '<span class="rev"></span><span class="msg"></span>';
    li.querySelector('.rev').textContent = `r${revision.rev}`;
    li.querySelector('.msg').textContent = `${revision.message || '(无提交信息)'} — ${
      revision.author
    }`;
    list.appendChild(li);
  }
}

// ---------------- 绑定 ----------------

el('login-form').addEventListener('submit', (event) => {
  event.preventDefault();
  void guard('登录', connect);
});
el('register-form').addEventListener('submit', (event) => {
  event.preventDefault();
  void guard('提交注册申请', doRegister);
});
el('show-register').addEventListener('click', () => setAuthMode('register'));
el('show-login').addEventListener('click', () => setAuthMode('login'));
for (const tab of document.querySelectorAll('.auth-tab')) {
  tab.addEventListener('click', () => setSource(tab.dataset.source));
}
el('open-settings').addEventListener('click', openSettings);
el('close-settings').addEventListener('click', closeSettings);
el('save-target').addEventListener('click', () => void guard('保存服务器地址', saveTargetSetting));
el('reset-target').addEventListener('click', () =>
  void guard('恢复默认服务器地址', resetTargetSetting),
);
el('refresh-repos').addEventListener('click', () => void guard('刷新仓库', loadRepos));
el('take-mine').addEventListener('click', () => void guard('保留我的', () => resolveConflict('mine')));
el('take-theirs').addEventListener('click', () =>
  void guard('采用服务端', () => resolveConflict('theirs')),
);
el('save-merge').addEventListener('click', () =>
  void guard('保存合并结果', () => resolveConflict('custom')),
);
el('delete-file').addEventListener('click', () =>
  void guard('删除文件', () => resolveConflict('delete')),
);
el('do-request').addEventListener('click', () => void guard('提交建库申请', requestRepo));
el('disconnect').addEventListener('click', () => void guard('退出登录', disconnect));
el('account-disconnect').addEventListener('click', () => void guard('退出登录', disconnect));
el('pick-dir').addEventListener('click', () => void guard('选择目录', pickDir));
el('checkout').addEventListener('click', () => void guard('检出', checkout));
el('refresh-status').addEventListener('click', () => void guard('扫描', refreshStatus));
el('update').addEventListener('click', () => void guard('更新', update));
el('revert').addEventListener('click', () => void guard('还原', revertAll));
el('commit').addEventListener('click', () => void guard('提交', commit));
el('refresh-log').addEventListener('click', () => void guard('刷新历史', loadLog));

/**
 * 启动时：把上次的连接信息填进登录页，并尝试用加密保存的凭证恢复 30 天会话。
 * 恢复成功直接进主界面；失败（到期、被撤销、钥匙串重置）静默回到登录页，
 * 只有「上次确实记住过」时才给一句可解释的提示，不弹错误框。
 */
async function bootstrap() {
  clearBanner();
  authMessage('');
  // 默认地址由 npm run build 生成的 config.js 注入（打包时配置）。
  state.defaultTarget = window.vcsConfig?.defaultTarget || FALLBACK_TARGET;
  const prefs = await api.loadPrefs();
  const saved = prefs.ok ? prefs.data : null;
  if (saved) {
    if (saved.target) state.target = saved.target;
    if (saved.username) el('username').value = saved.username;
    state.canPersistToken = saved.canPersistToken;
    if (!saved.canPersistToken) {
      el('remember').checked = false;
      el('remember').disabled = true;
      el('repo-hint').textContent =
        '当前系统不支持加密存储凭证，重启后需要重新登录（连接信息仍会保留）。';
    }
  }
  renderTarget();
  setSource('LOCAL');
  const resumed = await api.tryResume(effectiveTarget());
  if (resumed.ok && resumed.data) {
    await applySession(resumed.data, { target: effectiveTarget(), resumed: true });
    return;
  }
  showLogin(
    saved?.hasToken ? '上次的登录已过期或失效，请重新登录。' : '',
  );
}

void bootstrap();
