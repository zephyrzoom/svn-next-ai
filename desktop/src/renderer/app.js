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
 * 主进程用系统加密存储保存凭证（默认 30 天免登录，界面无开关），启动时 tryResume 直接恢复会话。
 *
 * 主界面只有一条主线：**仓库**。左侧是仓库列表，选中哪一个，右侧整个工作区就属于它
 * （工作副本 / 提交历史 / 容量三个 tab）。顶部固定一条上下文条，写着当前仓库名、
 * HEAD 版本与本地工作副本目录——工作副本本质上就是这个仓库在本地的一份拷贝，
 * 把它和仓库并列成两个菜单会让人在两个视图之间丢失「我在哪个仓库」的坐标。
 * 目录与仓库对不上时（拷贝过来的工作副本、换了账号），以目录里 wc.db 记录的 repoId
 * 为准自动切换，宁可变上下文，也不在 A 仓库的界面上改 B 仓库的文件。
 *
 * 治理类的审批（用户与注册、仓库创建、扩容配额、空间治理、目录配置）由服务端
 * Web 管理端承载，桌面端只保留登录、账号自助、仓库使用与工作副本这条主线。
 * 唯一的例外是**仓库拥有者**：自己创建的仓库会多一个「成员」页，可以在客户端
 * 直接增删成员、调整角色——加一个同事不该先让人去开浏览器。这一页只对 owner 出现，
 * 服务端仍按成员门独立复核，界面藏起来不等于权限收窄。
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
  canPersistToken: true,

  repos: [],
  selectedRepoId: null,
  /** 仓库 → 本地工作副本目录的绑定，持久化在 userData/settings.json。 */
  wcDirs: {},
  /** 当前选中仓库绑定的目录；空串表示这个仓库还没有本地工作副本。 */
  wcDir: '',
  wcRev: 0,
  changes: [],
  conflicts: [],
  diff: [],
  conflict: null,
  /** 差异卡片是否展开：默认收起，点变更条目或「查看差异」再打开。 */
  diffOpen: false,
  tab: 'wc',
  /** 当前仓库的成员列表（仅 owner 打开成员页时会加载）。 */
  members: [],
  /** 搜索结果里「可以加进来的人」——已在本仓库的会被过滤掉。 */
  candidates: [],
};

const el = (id) => document.getElementById(id);

/** 主界面提示浮层；未登录时自动转给登录页，避免提示落在看不见的地方。 */
let bannerTimer = 0;

function clearBanner() {
  window.clearTimeout(bannerTimer);
  const node = el('banner');
  node.className = 'banner hidden';
  node.textContent = '';
}

/**
 * 提示是浮在右下角的一小块，不占版面，过几秒自己退场：
 * 成功类 4 秒够看一眼，错误类留 8 秒（并支持点一下手动关掉）。
 * 未登录时所有反馈仍然落回登录卡片，那里是唯一可见的区域。
 */
function banner(message = '', kind = 'info') {
  if (!state.user) {
    authMessage(message, kind);
    return;
  }
  if (!message) {
    clearBanner();
    return;
  }
  window.clearTimeout(bannerTimer);
  const node = el('banner');
  node.className = `banner${kind === 'error' ? ' error' : ''}`;
  node.textContent = message;
  bannerTimer = window.setTimeout(() => clearBanner(), kind === 'error' ? 8000 : 4000);
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

/** 解包信封：失败时抛错并展示，成功返回 data。 */
function unwrap(res, action) {
  if (res && res.ok) return res.data;
  const message = res && res.message ? res.message : '未知错误';
  if (res && AUTH_CODES.has(res.code) && state.user) {
    // 已登录却拿到认证错误：不要只弹个提示，直接回登录页让人重新登录。
    showLogin(`登录状态已失效（${message}），请重新登录。`);
    throw shownError(message);
  }
  const suffix = res && AUTH_CODES.has(res.code) ? '\n请重新登录。' : '';
  banner(`${action}失败：${message}${suffix}`, 'error');
  throw shownError(message);
}

/**
 * 已经展示过的错误。`guard` 靠这个标记决定要不要再补一条提示——
 * 提示是会自动消失的浮层，用「界面上还有没有字」来判断会误判。
 */
function shownError(message) {
  const error = new Error(message);
  error.shown = true;
  return error;
}

async function guard(action, run) {
  try {
    return await run();
  } catch (err) {
    if (!err || !err.shown) {
      banner(`${action}失败：${err.message}`, 'error');
    }
    return undefined;
  }
}

// ---------------- 视图切换 ----------------

/**
 * 顶层视图只有三个：空态（一个仓库都没有）/ 仓库工作区 / 建库申请。
 * 纯界面操作：不触发网络请求。
 */
function setView(name) {
  for (const view of document.querySelectorAll('.view')) {
    view.classList.toggle('hidden', view.dataset.view !== name);
  }
}

/** 仓库工作区里的 tab：工作副本 / 提交历史 / 容量 / 成员（owner 专属）。 */
function setTab(name) {
  // 成员页只属于 owner：即使有人手动触发，非 owner 也退回工作副本。
  const next = name === 'members' && !isRepoOwner(currentRepo()) ? 'wc' : name;
  state.tab = next;
  for (const tab of document.querySelectorAll('.tab')) {
    const active = tab.dataset.tab === next;
    tab.classList.toggle('active', active);
    tab.setAttribute('aria-selected', String(active));
  }
  for (const pane of document.querySelectorAll('.pane')) {
    pane.classList.toggle('hidden', pane.dataset.tab !== next);
  }
  if (next === 'history') void guard('加载历史', loadLog);
  if (next === 'quota') void guard('加载容量', loadQuota);
  if (next === 'members') void guard('加载成员', loadMembers);
}

for (const tab of document.querySelectorAll('.tab')) {
  tab.addEventListener('click', () => setTab(tab.dataset.tab));
}

// ---------------- 登录页 / 主界面切换 ----------------

/** config.js 缺失时的兜底（例如直接打开源码目录里的 html）。 */
const FALLBACK_TARGET = '127.0.0.1:50051';

function effectiveTarget() {
  return state.target || state.defaultTarget || FALLBACK_TARGET;
}

/** 齿轮面板：界面上不展示地址，默认地址来自打包配置，用户可在此覆盖。 */
function openSettings() {
  el('server-target').value = effectiveTarget();
  el('target-note').textContent = `默认地址：${state.defaultTarget}（保存即覆盖）`;
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
  authMessage(override ? '服务器地址已保存，登录时使用该地址。' : '已恢复为打包默认地址。');
}

async function resetTargetSetting() {
  if (state.target) unwrap(await api.setTarget(''), '恢复默认服务器地址');
  state.target = '';
  closeSettings();
  authMessage('已恢复为打包默认地址。');
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
  document.title = 'svn-next';
  clearBanner();
  closeSettings();
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
  if (!username || !password) {
    authMessage('请填写用户名与密码', 'error');
    return;
  }
  // 默认就是 30 天免登录：界面不再有勾选框，主进程统一按长期会话签发并加密保存凭证。
  const session = unwrap(
    await api.connect(target, username, password, state.source),
    '登录',
  );
  el('password').value = '';
  authMessage('');
  await applySession(session, { target });
}

/** 登录成功与「用保存的凭证恢复」共用同一段收尾逻辑。 */
async function applySession(session, { target = '', resumed = false } = {}) {
  const { user } = session;
  state.user = user;
  const usedTarget = target || effectiveTarget();
  state.target = usedTarget;
  // 右上角只这个人的姓名：角色与免登录到期都不在界面上出现。
  el('who').textContent = user.displayName || user.username;
  showApp();
  // 工作副本目录属于「这台机器」的记忆，与账号无关：登录后一次性读回来。
  // 读不到（旧版客户端留下的设置、磁盘只读）不该挡住登录，只是这次要重新选目录。
  const dirs = await api.loadRepoDirs();
  state.wcDirs = dirs && dirs.ok && dirs.data ? dirs.data : {};
  await loadRepos();
  banner(resumed ? `已用保存的凭证恢复会话：${user.displayName || user.username}` : '');
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

// ---------------- 仓库列表：唯一的入口菜单 ----------------

function currentRepo() {
  return state.repos.find((repo) => repo.repoId === state.selectedRepoId) || null;
}

async function loadRepos() {
  const repos = unwrap(await api.listRepos(), '加载仓库');
  state.repos = repos;
  if (repos.length === 0) {
    state.selectedRepoId = null;
    state.wcDir = '';
    state.changes = [];
    state.conflicts = [];
    renderRepoNav();
    renderRepoHead();
    setView('empty');
    return;
  }
  // 选中的仓库被删或不再可见时才回落到第一个，否则保持原位——刷新不该把人踢走。
  const kept = repos.some((repo) => repo.repoId === state.selectedRepoId);
  await selectRepo(kept ? state.selectedRepoId : repos[0].repoId);
}

/** 左侧仓库列表：名字 + HEAD + 「本地有工作副本」的圆点。 */
function renderRepoNav() {
  const list = el('repo-nav');
  list.innerHTML = '';
  if (state.repos.length === 0) {
    list.innerHTML = '<li class="repo-empty">暂无仓库</li>';
    return;
  }
  for (const repo of state.repos) {
    const li = document.createElement('li');
    const dir = state.wcDirs[repo.repoId];
    li.className = `repo-item${repo.repoId === state.selectedRepoId ? ' active' : ''}`;
    li.title = dir ? `${repo.name}\n本地工作副本：${dir}` : repo.name;
    li.innerHTML =
      '<span class="repo-name"></span>' +
      '<span class="repo-dot"></span>' +
      '<span class="repo-rev"></span>';
    li.querySelector('.repo-name').textContent = repo.name;
    li.querySelector('.repo-dot').classList.toggle('on', Boolean(dir));
    li.querySelector('.repo-rev').textContent = `r${repo.headRev}`;
    li.addEventListener('click', () => {
      // 已经选中的仓库再点一次 = 「回到这个仓库的工作区」（比如刚从建库申请页回来）。
      if (repo.repoId === state.selectedRepoId) {
        setView('repo');
        return;
      }
      void guard('切换仓库', () => selectRepo(repo.repoId));
    });
    list.appendChild(li);
  }
}

/**
 * 切换仓库：右侧整个工作区跟着换。工作副本目录来自按仓库记住的绑定，
 * 没有绑定就显示检出引导——而不是让人先去另一个菜单里「选仓库」。
 */
async function selectRepo(repoId) {
  state.selectedRepoId = repoId;
  state.wcDir = state.wcDirs[repoId] || '';
  state.wcRev = 0;
  state.changes = [];
  state.conflicts = [];
  state.diff = [];
  state.conflict = null;
  el('dir').value = state.wcDir;
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('members').innerHTML = '';
  el('member-candidates').innerHTML = '';
  state.members = [];
  state.candidates = [];
  el('diff-path').textContent = '';
  el('diff-path').title = '';
  el('diff').textContent = state.wcDir ? '正在扫描工作副本…' : '点击上方变更查看差异';
  el('conflict-editor').classList.add('hidden');
  el('diff').classList.remove('hidden');
  // 换仓库就是换上下文：差异回到默认收起状态，不把上一个仓库的内容带过来。
  setDiffOpen(false);

  renderRepoNav();
  renderRepoHead();
  setView('repo');

  await Promise.all([guard('加载容量', loadQuota), guard('加载历史', loadLog)]);
  // 停在新仓库的成员页时，成员列表也得跟着换仓库（非 owner 已在 renderMemberAccess 里退回）。
  if (state.tab === 'members') await guard('加载成员', loadMembers);
  if (state.wcDir) await guard('扫描工作副本', refreshStatus);
}

/** 顶部固定上下文条：仓库名 + HEAD + 本地目录 + 待提交/冲突计数。 */
function renderRepoHead() {
  const repo = currentRepo();
  el('repo-title').textContent = repo ? repo.name : '—';
  el('repo-rev').textContent = repo ? `HEAD r${repo.headRev}` : 'HEAD r0';
  document.title = repo ? `${repo.name} — svn-next` : 'svn-next';
  renderWcBar();
  renderMemberAccess();
}

/**
 * 「成员」页只属于仓库拥有者：owner 唯一且不可转让，所以直接比 ownerUserId。
 * 不是 owner 时整页（连同 tab）都不出现——把人领到一个「无权限」的页面上没有意义。
 */
function isRepoOwner(repo) {
  return Boolean(repo && state.user && repo.ownerUserId && repo.ownerUserId === state.user.userId);
}

function renderMemberAccess() {
  const owner = isRepoOwner(currentRepo());
  el('tab-members').classList.toggle('hidden', !owner);
  if (!owner && state.tab === 'members') setTab('wc');
}

function renderWcBar() {
  const dir = state.wcDir;
  el('wc-path').textContent = dir || '未检出到本地';
  el('wc-path').title = dir;
  el('wc-dot').className = `wc-dot${dir ? ' on' : ''}`;
  const conflicts = state.conflicts.length;
  el('wc-status').textContent = !dir
    ? '检出后即可提交与更新'
    : conflicts > 0
      ? `${conflicts} 个冲突待解决`
      : `${state.changes.length} 个待提交变更 @ r${state.wcRev}`;
  el('wc-status').classList.toggle('warn', conflicts > 0);
  for (const id of ['pick-dir', 'refresh-status', 'update']) {
    el(id).disabled = !dir;
  }
  el('checkout-card').classList.toggle('hidden', Boolean(dir));
  el('workbench').classList.toggle('hidden', !dir);
  el('wc-change-count').textContent = dir ? `${state.changes.length} 项` : '';
}

// ---------------- 检出、扫描与更新 ----------------

/** 把目录与仓库绑定起来：一个目录只属于一个仓库，顺手清掉指向同一目录的旧绑定。 */
async function bindWorkingCopy(repoId, dir) {
  for (const [id, value] of Object.entries(state.wcDirs)) {
    if (value === dir && id !== repoId) delete state.wcDirs[id];
  }
  state.wcDirs[repoId] = dir;
  if (state.selectedRepoId === repoId) state.wcDir = dir;
  el('dir').value = dir;
  renderRepoNav();
  renderWcBar();
  // 落盘失败不影响这次使用，只是下次要重新选目录——提示一句，不要拦流程。
  const saved = await api.saveRepoDirs(state.wcDirs);
  if (!saved || !saved.ok) {
    banner('工作副本路径没能保存到本机设置，下次打开需要重新选择目录。', 'error');
  }
}

async function pickCheckoutDir() {
  const dir = unwrap(await api.pickDir(), '选择目录');
  if (dir) el('dir').value = dir;
}

/**
 * 更换目录：先问这个目录「本来是谁的工作副本」，再决定是绑定还是引导检出。
 * 这一步是为了防「在 A 仓库的界面上改 B 仓库的文件」——目录里 wc.db 的 repoId 是唯一权威。
 */
async function pickWorkingCopy() {
  const picked = unwrap(await api.pickDir(), '选择目录');
  if (!picked) return;
  const info = unwrap(await api.wcInfo(picked), '读取工作副本信息');
  if (!info) {
    el('dir').value = picked;
    banner(`「${picked}」还不是工作副本，确认目录后点「检出」即可把当前仓库铺到这里。`);
    el('dir').focus();
    return;
  }
  const owner = state.repos.find((repo) => repo.repoId === info.repoId);
  if (!owner) {
    banner(`这个目录属于仓库 ${info.repoId}，但当前账号看不到它，无法在这里使用。`, 'error');
    return;
  }
  await bindWorkingCopy(owner.repoId, picked);
  if (owner.repoId !== state.selectedRepoId) {
    banner(`这个目录是「${owner.name}」的工作副本，已切换到该仓库。`);
    await selectRepo(owner.repoId);
    return;
  }
  await guard('扫描工作副本', refreshStatus);
  banner(`已绑定工作副本：${picked}（r${info.rev}）。`);
}

async function checkout() {
  const repo = currentRepo();
  if (!repo) {
    banner('请先在左侧选择一个仓库', 'error');
    return;
  }
  const dir = el('dir').value.trim();
  if (!dir) {
    banner('请填写检出目录', 'error');
    return;
  }
  const result = unwrap(await api.checkout(repo.repoId, dir), '检出');
  await bindWorkingCopy(repo.repoId, result.dir);
  banner(`已把「${repo.name}」检出到 ${result.dir}（r${result.rev}，共 ${result.files} 个文件）。`);
  await refreshStatus();
  await loadLog();
}

/**
 * 扫描工作副本。目录里记录的 repoId 与当前仓库不一致时，以目录为准切过去：
 * 宁可换上下文，也不让「提交」把 A 仓库的改动推到 B 仓库。
 */
async function refreshStatus() {
  const dir = state.wcDir;
  if (!dir) {
    banner('这个仓库还没有本地工作副本，请先检出', 'error');
    return;
  }
  const data = unwrap(await api.status(dir), '扫描工作副本');
  if (data.repoId && data.repoId !== state.selectedRepoId) {
    const owner = state.repos.find((repo) => repo.repoId === data.repoId);
    if (owner && owner.repoId !== state.selectedRepoId) {
      await bindWorkingCopy(owner.repoId, dir);
      banner(`「${dir}」属于仓库「${owner.name}」，已切换到该仓库。`);
      await selectRepo(owner.repoId);
      return;
    }
  }
  state.wcRev = data.rev;
  state.changes = data.entries;
  state.conflicts = data.conflicts || [];
  renderWcBar();
  renderChanges();
  closeConflict();
  const diffs = unwrap(await api.diff(dir), '生成差异');
  state.diff = diffs;
  // 重新扫描会重建变更列表，之前查看的那条不一定还在，差异区回到提示态。
  if (state.changes.length === 0) {
    el('diff').textContent = '工作副本干净';
  } else {
    el('diff').textContent = '点击上方变更查看差异';
  }
  el('diff-path').textContent = '';
  el('diff-path').title = '';
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
    li.title = change.path;
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

// ---------------- 提交历史与容量 ----------------

async function loadLog() {
  const repo = currentRepo();
  el('log-repo').textContent = repo ? repo.name : '';
  const list = el('log');
  if (!repo) {
    list.innerHTML = '';
    return;
  }
  const revisions = unwrap(await api.log(repo.repoId, 20), '加载历史');
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

/** 容量是只读信息：跟着当前仓库走，不触发任何治理类调用。 */
async function loadQuota() {
  const repo = currentRepo();
  el('quota-repo').textContent = repo ? repo.name : '';
  if (!repo) return;
  const quota = unwrap(await api.getQuota(repo.repoId), '加载容量');
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

// ---------------- 成员管理（仅仓库拥有者） ----------------

/** 与 proto 的 RepoMemberRole 一致：1 owner / 2 maintainer / 3 writer / 4 reader。 */
const MEMBER_ROLE_LABELS = { 1: '拥有者', 2: '维护者', 3: '写入者', 4: '只读' };
/** 能授予的角色：owner 不可授予他人（唯一），所以这里只列低阶三种。 */
const ASSIGNABLE_ROLES = [2, 3, 4];

function roleLabel(role) {
  return MEMBER_ROLE_LABELS[role] || `未知角色（${role}）`;
}

async function loadMembers() {
  const repo = currentRepo();
  el('members-repo').textContent = repo ? repo.name : '';
  const list = el('members');
  if (!repo || !isRepoOwner(repo)) {
    list.innerHTML = '';
    return;
  }
  const members = unwrap(await api.listMembers(repo.repoId), '加载成员');
  state.members = members;
  renderMembers();
  // 还没搜过人就给一句提示，别让「候选列表」看起来像坏掉了。
  if (state.candidates.length === 0 && !el('member-query').value.trim()) renderCandidates('');
}

/** 成员行：owner 只显示一个「拥有者」标记，其余成员可改角色、可移除。 */
function renderMembers() {
  const list = el('members');
  list.innerHTML = '';
  if (state.members.length === 0) {
    list.innerHTML = '<li class="empty">暂无成员</li>';
    return;
  }
  for (const member of state.members) {
    const li = document.createElement('li');
    li.innerHTML =
      '<span class="member-name"><span class="who-name"></span><span class="who-id"></span></span>' +
      '<span class="member-role"></span>';
    li.querySelector('.who-name').textContent = member.displayName || member.username;
    li.querySelector('.who-id').textContent = `@${member.username}`;
    const slot = li.querySelector('.member-role');

    if (member.role === 1) {
      // owner 唯一且受保护：不给下拉，也不给移除按钮。
      const pill = document.createElement('span');
      pill.className = 'pill';
      pill.textContent = roleLabel(member.role);
      slot.appendChild(pill);
    } else {
      const select = document.createElement('select');
      select.className = 'role-select';
      select.setAttribute('aria-label', `调整 ${member.username} 的角色`);
      for (const role of ASSIGNABLE_ROLES) {
        const option = document.createElement('option');
        option.value = String(role);
        option.textContent = roleLabel(role);
        option.selected = member.role === role;
        select.appendChild(option);
      }
      select.addEventListener('change', () => {
        void guard('调整成员角色', async () => {
          try {
            await changeMemberRole(member, Number(select.value));
          } finally {
            // 服务端拒绝时下拉会停在错误的值上，按真实名单重画一次。
            renderMembers();
          }
        });
      });
      slot.appendChild(select);

      const remove = document.createElement('button');
      remove.type = 'button';
      remove.className = 'ghost';
      remove.textContent = '移除';
      remove.addEventListener('click', () => void guard('移除成员', () => removeMember(member)));
      slot.appendChild(remove);
    }
    list.appendChild(li);
  }
}

async function changeMemberRole(member, role) {
  const repo = currentRepo();
  if (!repo || role === member.role) return;
  unwrap(await api.setMemberRole(repo.repoId, member.userId, role), '调整成员角色');
  banner(`已把「${member.displayName || member.username}」设为${roleLabel(role)}。`);
  await loadMembers();
}

async function removeMember(member) {
  const repo = currentRepo();
  if (!repo) return;
  const name = member.displayName || member.username;
  // 移除是即时生效的：对方手上的提交会话会被中止，所以先问一句。
  const ok = window.confirm(
    `把「${name}」移出仓库「${repo.name}」？\n` +
      '对方会立即失去该仓库的访问权，未完成的提交也会被中止。',
  );
  if (!ok) return;
  unwrap(await api.removeMember(repo.repoId, member.userId), '移除成员');
  banner(`已把「${name}」移出「${repo.name}」。`);
  state.candidates = state.candidates.filter((user) => user.userId !== member.userId);
  renderCandidates(el('member-query').value.trim());
  await loadMembers();
}

async function searchCandidates() {
  const repo = currentRepo();
  if (!repo) return;
  const query = el('member-query').value.trim();
  if (!query) {
    banner('请先输入用户名或姓名', 'error');
    return;
  }
  const users = unwrap(await api.searchUsers(query), '搜索用户');
  const existing = new Set(state.members.map((member) => member.userId));
  state.candidates = users.filter((user) => !existing.has(user.userId));
  renderCandidates(query);
}

function renderCandidates(query = '') {
  const list = el('member-candidates');
  list.innerHTML = '';
  if (state.candidates.length === 0) {
    list.innerHTML = query
      ? '<li class="empty">没有可添加的账号：对方可能已经是成员，或账号还没通过审批。</li>'
      : '<li class="empty">输入用户名或姓名后点「搜索」。</li>';
    return;
  }
  for (const user of state.candidates) {
    const li = document.createElement('li');
    li.innerHTML =
      '<span class="member-name"><span class="who-name"></span><span class="who-id"></span></span>' +
      '<button class="primary" type="button">添加</button>';
    li.querySelector('.who-name').textContent = user.displayName || user.username;
    li.querySelector('.who-id').textContent = `@${user.username}`;
    li.querySelector('button').addEventListener('click', () => void guard('添加成员', () => addMember(user)));
    list.appendChild(li);
  }
}

async function addMember(user) {
  const repo = currentRepo();
  if (!repo) return;
  const role = Number(el('member-role').value);
  unwrap(await api.addMember(repo.repoId, user.userId, role), '添加成员');
  banner(`已把「${user.displayName || user.username}」加入「${repo.name}」，角色：${roleLabel(role)}。`);
  state.candidates = state.candidates.filter((item) => item.userId !== user.userId);
  renderCandidates(el('member-query').value.trim());
  await loadMembers();
}

// ---------------- 建库申请 / 扩容申请 ----------------

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
    `建库申请已提交（${result.requestId}）。通过后容量为 10 GiB，你会自动成为 owner，仓库会出现在左侧列表里。`,
  );
}

/** 扩容申请同样不填容量：一次固定 +10 GiB，由管理员按磁盘余量批准。 */
async function requestExpansion() {
  const repo = currentRepo();
  if (!repo) {
    banner('请先在左侧选择一个仓库', 'error');
    return;
  }
  const result = unwrap(await api.requestRepoExpansion(repo.repoId, ''), '提交扩容申请');
  banner(`已提交「${repo.name}」的扩容申请（${result.requestId}）：通过后上限增加 10 GiB。`);
}

// ---------------- 差异面板：默认收起，按需展开 ----------------

/**
 * 差异独占整行，但不必一直占着版面：默认隐藏，点某条变更（或「查看差异」）才展开。
 * 收起只切显隐，不清内容也不清列表选中态——再展开还是上次看的那条；
 * 冲突编辑器在这张卡片里，它的显隐由 showConflict / closeConflict 自己管。
 */
function setDiffOpen(open) {
  state.diffOpen = Boolean(open);
  el('diff-card').classList.toggle('hidden', !state.diffOpen);
  const toggle = el('toggle-diff');
  toggle.textContent = state.diffOpen ? '收起差异' : '查看差异';
  toggle.setAttribute('aria-expanded', String(state.diffOpen));
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
  const detail = unwrap(await api.conflictDetail(state.wcDir, path), '读取冲突内容');
  state.conflict = { path, kind: detail.kind };
  // 冲突编辑器住在差异卡片里：看冲突就是「需要看差异」，先把卡片展开。
  setDiffOpen(true);
  const label = `${path}（${detail.kind} 冲突）`;
  el('diff-path').textContent = label;
  el('diff-path').title = label;
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
  const { path } = state.conflict;
  const content = choice === 'custom' ? el('conflict-result').value : undefined;
  unwrap(await api.resolveConflict(state.wcDir, path, choice, content), '解决冲突');
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

function showDiff(path, node) {
  highlight(node);
  // 从冲突编辑器切回普通差异：先把三栏收掉，否则 diff 还被 hidden 着。
  closeConflict();
  setDiffOpen(true);
  el('diff-path').textContent = path;
  el('diff-path').title = path;
  const entry = state.diff.find((item) => item.path === path);
  el('diff').textContent = entry ? entry.text : '（无文本差异，可能是二进制文件或新增/删除）';
}

// ---------------- 提交 / 更新 / 还原 ----------------

async function commit() {
  const dir = state.wcDir;
  const message = el('message').value.trim();
  if (!dir) {
    banner('请先检出工作副本', 'error');
    return;
  }
  if (!message) {
    banner('请填写提交信息', 'error');
    return;
  }
  const result = unwrap(await api.commit(dir, message), '提交');
  el('message').value = '';
  banner(
    `已提交 r${result.rev}：新增 ${result.added}，修改 ${result.modified}，删除 ${result.deleted}` +
      `（上传 ${result.pushed} 个对象，服务端已有内容不重复上传）`,
  );
  // 提交会推进仓库 HEAD：列表里的版本号与历史都要跟着刷新。
  await loadRepos();
}

async function update() {
  const dir = state.wcDir;
  if (!dir) {
    banner('请先检出工作副本', 'error');
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
  await loadRepos();
}

async function revertAll() {
  const dir = state.wcDir;
  if (!dir) {
    banner('请先检出工作副本', 'error');
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
  state.wcDir = '';
  state.changes = [];
  state.conflicts = [];
  state.diff = [];
  el('who').textContent = '';
  el('repo-nav').innerHTML = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('members').innerHTML = '';
  el('member-candidates').innerHTML = '';
  state.members = [];
  state.candidates = [];
  setTab('wc');
  setDiffOpen(false);
  el('diff-path').textContent = '';
  el('diff-path').title = '';
  el('diff').textContent = '点击上方变更查看差异';
  el('wc-path').textContent = '';
  el('checkout-card').classList.remove('hidden');
  el('workbench').classList.add('hidden');
  showLogin('已退出登录。');
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
// 建库申请是「创建一个不存在的仓库」，与选中哪一个仓库无关，所以单独成页而不是塞在列表下面。
el('nav-new-repo').addEventListener('click', () => setView('new'));
el('empty-new-repo').addEventListener('click', () => setView('new'));
el('back-from-new').addEventListener('click', () =>
  setView(state.selectedRepoId ? 'repo' : 'empty'),
);
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
el('do-expand').addEventListener('click', () => void guard('提交扩容申请', requestExpansion));
el('disconnect').addEventListener('click', () => void guard('退出登录', disconnect));
el('pick-checkout-dir').addEventListener('click', () => void guard('选择目录', pickCheckoutDir));
el('pick-dir').addEventListener('click', () => void guard('更换目录', pickWorkingCopy));
el('checkout').addEventListener('click', () => void guard('检出', checkout));
el('refresh-status').addEventListener('click', () => void guard('扫描', refreshStatus));
el('update').addEventListener('click', () => void guard('更新', update));
el('revert').addEventListener('click', () => void guard('还原', revertAll));
el('toggle-diff').addEventListener('click', () => setDiffOpen(!state.diffOpen));
el('close-diff').addEventListener('click', () => setDiffOpen(false));
el('commit').addEventListener('click', () => void guard('提交', commit));
el('refresh-log').addEventListener('click', () => void guard('刷新历史', loadLog));
// 成员页（owner 专属）：刷新名单、按名字搜人、回车即搜。
el('refresh-members').addEventListener('click', () => void guard('刷新成员', loadMembers));
el('search-members').addEventListener('click', () => void guard('搜索用户', searchCandidates));
el('member-query').addEventListener('keydown', (event) => {
  if (event.key === 'Enter') {
    event.preventDefault();
    void guard('搜索用户', searchCandidates);
  }
});
// 浮层提示点一下就关，不必等它自己退场。
el('banner').addEventListener('click', clearBanner);

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
    // 没有加密存储能力时无法免登录：提示由 setAuthMode 统一在登录页给出。
    state.canPersistToken = saved.canPersistToken;
  }
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
