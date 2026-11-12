/**
 * 渲染进程：桌面端「仓库使用」客户端的界面逻辑。
 *
 * 这里没有任何网络或文件系统能力——全部经 `window.vcs`（preload 暴露）转交主进程。
 * 每个调用都返回统一信封 `{ok, data} | {ok:false, code, message}`，业务错误直接展示
 * `message`，认证类错误（11/12/19/20）额外提示「请重新连接」。
 *
 * 管理与审批（成员、配额、空间治理、目录配置、审批台）由服务端 Web 管理端承载，
 * 桌面端只保留连接、账号自助、仓库使用与工作副本这条主线。
 */
const api = window.vcs;

const AUTH_CODES = new Set([11, 12, 19, 20]);

const state = {
  user: null,
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

function banner(message, kind = 'info') {
  const node = el('banner');
  if (!message) {
    node.className = 'banner hidden';
    node.textContent = '';
    return;
  }
  node.className = `banner${kind === 'error' ? ' error' : ''}`;
  node.textContent = message;
}

/** 解包信封：失败时抛错并展示，成功返回 data。 */
function unwrap(res, action) {
  if (res && res.ok) return res.data;
  const message = res && res.message ? res.message : '未知错误';
  const suffix = res && AUTH_CODES.has(res.code) ? '\n登录状态可能已失效，请重新连接。' : '';
  banner(`${action}失败：${message}${suffix}`, 'error');
  throw new Error(message);
}

async function guard(action, run) {
  try {
    return await run();
  } catch (err) {
    if (!el('banner').textContent) {
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

// ---------------- 连接与账号 ----------------

async function connect() {
  const target = el('target').value.trim();
  const username = el('username').value.trim();
  const password = el('password').value;
  if (!target || !username || !password) {
    banner('请填写服务端地址、用户名与密码', 'error');
    return;
  }
  const source = el('auth-source').value === 'LDAP' ? 'LDAP' : 'LOCAL';
  const data = unwrap(
    await api.connect(target, username, password, source),
    '连接',
  );
  el('password').value = '';
  await applySession(data.user);
}

/** 登录成功与「用保存的凭证恢复」共用同一段收尾逻辑。 */
async function applySession(user) {
  state.user = user;
  el('who').textContent = `${user.displayName || user.username} · ${roleLabel(user.role)}`;
  el('disconnect').classList.remove('hidden');
  await loadRepos();
  setView('workcopy');
}

function roleLabel(role) {
  return { 1: '只读', 2: '可写', 3: '管理员' }[role] || '未知角色';
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

// ---------------- 注册 ----------------

function toggle(id) {
  el(id).classList.toggle('hidden');
}

async function doRegister() {
  const target = el('target').value.trim();
  const username = el('reg-username').value.trim();
  const displayName = el('reg-name').value.trim();
  const password = el('reg-password').value;
  const reason = el('reg-reason').value.trim();
  if (!target || !username || !displayName || !password) {
    banner('请填写服务端地址、用户名、姓名与密码', 'error');
    return;
  }
  const result = unwrap(
    await api.register(target, username, displayName, password, reason),
    '提交注册申请',
  );
  el('register-form').classList.add('hidden');
  el('reg-password').value = '';
  if (result.autoApproved) {
    // 空库第一个用户：服务端已自动批准为管理员，注册时设置的密码立即可用。
    el('username').value = username;
    banner(
      `你是全库第一个用户，已自动成为管理员（申请号 ${result.requestId}）。\n` +
        '用户名已填到右上角，输入刚才设置的密码点「连接」即可登录。',
    );
    el('password').focus();
    return;
  }
  banner(
    `注册申请已提交（${result.requestId}）。账号处于「待审批」，管理员在服务端 Web 管理端\n` +
      '通过后，直接用你刚才设置的密码登录即可。',
  );
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
  unwrap(await api.disconnect(), '断开连接');
  state.user = null;
  state.repos = [];
  el('who').textContent = '';
  el('disconnect').classList.add('hidden');
  el('repos').innerHTML = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('wc-path').textContent = '';
  banner('已断开连接');
  setView('account');
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

el('connect').addEventListener('click', () => void guard('连接', connect));
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
el('toggle-register').addEventListener('click', () => toggle('register-form'));
el('do-register').addEventListener('click', () => void guard('提交注册申请', doRegister));
el('do-request').addEventListener('click', () => void guard('提交建库申请', requestRepo));
el('disconnect').addEventListener('click', () => void guard('断开连接', disconnect));
el('pick-dir').addEventListener('click', () => void guard('选择目录', pickDir));
el('checkout').addEventListener('click', () => void guard('检出', checkout));
el('refresh-status').addEventListener('click', () => void guard('扫描', refreshStatus));
el('update').addEventListener('click', () => void guard('更新', update));
el('revert').addEventListener('click', () => void guard('还原', revertAll));
el('commit').addEventListener('click', () => void guard('提交', commit));
el('refresh-log').addEventListener('click', () => void guard('刷新历史', loadLog));

/**
 * 启动时：把上次的连接信息填好，并尝试用加密保存的凭证直接恢复会话。
 * 恢复失败不打扰用户（token 过期、被撤销、钥匙串重置都会走到这里），
 * 只是把登录框留在眼前。
 */
async function bootstrap() {
  banner('');
  const prefs = await api.loadPrefs();
  if (prefs.ok) {
    if (prefs.data.target) el('target').value = prefs.data.target;
    if (prefs.data.username) el('username').value = prefs.data.username;
    if (!prefs.data.canPersistToken) {
      el('repo-hint').textContent =
        '当前系统不支持加密存储凭证，重启后需要重新登录（连接信息仍会保留）。';
    }
  }
  const resumed = await api.tryResume();
  if (resumed.ok && resumed.data) {
    await applySession(resumed.data.user);
    banner(`已用保存的凭证恢复会话：${resumed.data.user.username}`);
  }
}

void bootstrap();
