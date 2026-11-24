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
 * （工作副本 / 提交历史 / 设置三个 tab；容量、主动清理与注销都在设置页）。顶部固定一条上下文条，写着当前仓库名、
 * HEAD 版本与本地工作副本目录——工作副本本质上就是这个仓库在本地的一份拷贝，
 * 把它和仓库并列成两个菜单会让人在两个视图之间丢失「我在哪个仓库」的坐标。
 * 目录与仓库对不上时（拷贝过来的工作副本、换了账号），以目录里 wc.db 记录的 repoId
 * 为准自动切换，宁可变上下文，也不在 A 仓库的界面上改 B 仓库的文件。
 * 一个仓库允许检出到多个目录、保留多份工作副本，彼此的本地改动互不影响；上下文条
 * 上的下拉用来切换当前操作的那一份，上下文条的「检出」随时可以再铺一份——切换目录
 * 只有这一个入口，不再另放一个「更换目录」按钮。检出卡片里的「选择…」会只读探测
 * `.vcs/wc.db`：选到的目录若本来就是一份工作副本（例如从别处拷来的），直接把它
 * 接进来 / 切过去，绝不重新铺盘——原「更换目录」的绑定能力就收在这个入口里。
 *
 * 列表的顺序是**常用优先**：切到这个仓库、把它检出到本地，都算一次使用；权重按
 * 半衰期衰减（记在当前账号名下），常用的浮到最上面。从没用过的仓库保持服务端顺序
 * 排在后面，彼此不会因为一次重排而换位——列表要稳定，只有真的用过才往上走。
 *
 * 工作副本是磁盘上的事实，不是界面上的一段记忆：`settings.json` 里记下的目录一旦
 * 不存在（用户删掉了整个检出目录、或清掉了 `.vcs/`），刷新仓库页时就把它从列表里摘掉；
 * 扫描遇到这种目录只回报 missing，绝不会把删掉的目录重新检出建回来。
 * 这份记忆按账号隔离（服务端地址 + 用户名），同一台机器上换账号登录看不到别人的目录。
 * 变更同理——改完文件切回窗口、切回工作副本 tab、以及可见时每 8 秒，界面都会自己
 * 对一次盘（见文件末尾「工作副本自动刷新」），所以没有「重新扫描」这个按钮。
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
  /** 仓库 → 该仓库在**当前账号**名下的所有工作副本目录（首个为最近使用的一份），持久化在 settings.json。 */
  wcDirs: {},
  /**
   * 仓库 → 本机使用权重（`weight` 已按半衰期折算，越大越常用）。
   * 左侧列表按它排序，常用的浮到最上面；没有记录的仓库保持服务端顺序排在后面。
   */
  repoUsage: {},
  /** 当前仓库正在操作的那一份工作副本目录；空串表示还没有本地工作副本。 */
  wcDir: '',
  /** 「检出到本地」卡片是否展开：已有工作副本时由「检出」按钮临时打开。 */
  checkoutOpen: false,
  wcRev: 0,
  changes: [],
  conflicts: [],
  diff: [],
  conflict: null,
  /** 差异卡片是否展开：默认收起，点变更条目或「查看差异」再打开。 */
  diffOpen: false,
  /** 变更集合的指纹（版本 + 路径 + 冲突）：一样就不重绘，自动刷新才敢频繁跑。 */
  statusSig: '',
  /** 差异是否已过期：卡片收着时不读盘，点开某条变更时再重新生成。 */
  diffDirty: false,
  /** 差异卡片当前展示的路径：扫描后据此把高亮与内容接续回来。 */
  detailPath: '',
  /** 扫描进行中：自动刷新撞上就跳过，避免同一份目录被并发扫。 */
  scanning: false,
  tab: 'wc',
  /** 当前仓库的成员列表（仅 owner 打开成员页时会加载）。 */
  members: [],
  /** 搜索结果里「可以加进来的人」——已在本仓库的会被过滤掉。 */
  candidates: [],
  /** 下拉里键盘高亮的下标；-1 表示没有高亮（回车取第一条）。 */
  candidateIndex: -1,
  /** 已经选中、等待分配权限的候选账号；为空表示还没选人。 */
  picked: null,
  /** 搜索请求进行中（只影响下拉里的提示文案）。 */
  searching: false,
  /** 搜索序号：只接受最后一次请求的结果，慢响应不许盖掉新输入。 */
  searchSeq: 0,
  searchTimer: 0,
  /**
   * 正在进行的那一次动盘操作：''（空闲）/ 'update' / 'commit' / 'checkout' /
   * 'revert' / 'sparse'。
   *
   * 同一时刻只允许一个。大文件的一次更新要跑几十秒，期间再点一次「更新」，或在同一份
   * 副本上点「提交」，只会让两次写盘交错在同一个 wc.db 与同一批文件上；按钮跟着它一起
   * 禁用并换成「更新中…」，后台自动刷新也绕开它。
   */
  job: '',
  /**
   * 稀疏检出面板。`checked` 是界面里勾中的目录，`rules` 是当前工作副本里
   * 真正生效的规则（从 wc.db 读回）；两者不一致时提示「待应用」。
   */
  sparse: {
    open: false,
    paths: [],
    checked: new Set(),
    expanded: new Set(),
    rules: [],
    repoId: null,
    loading: false,
  },
  /**
   * 设置页的「清理空间」：勾目录 / 文件 → 彻底删除。
   * 与稀疏检出各自独立（两个面板可以同时开着，选择互不影响）。
   */
  cleanup: {
    open: false,
    paths: [],
    checked: new Set(),
    expanded: new Set(),
    repoId: null,
    loading: false,
    busy: false,
  },
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
 * 解包信封，`quiet = true` 时只对认证失效出声：后台自动刷新失败不该每几秒
 * 弹一条提示，下一轮会自己重试；而登录失效必须让人看见，所以照常退回登录页。
 */
function unwrapMaybe(res, action, quiet) {
  if (!quiet) return unwrap(res, action);
  if (res && res.ok) return res.data;
  if (res && AUTH_CODES.has(res.code)) return unwrap(res, action);
  return null;
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

/** 仓库工作区里的 tab：工作副本 / 提交历史 / 设置（容量 + 清理空间 + 注销）/ 成员（owner 专属）。 */
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
  if (next === 'settings') {
    void guard('加载容量', loadQuota);
    renderCleanupSummary();
    if (state.cleanup.open) void guard('读取仓库目录', () => loadCleanupTree());
  }
  if (next === 'members') void guard('加载成员', loadMembers);
  // 回到工作副本页顺手对一次盘：人可能刚在编辑器里改完文件。
  if (next === 'wc') autoScan();
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
  // 工作副本目录是「我用这个账号在这台机器上检出的」，按账号隔离：主进程按当前
  // 登录身份（地址 + 用户名）取，A 在 s1、B 在 s2，切回 A 读回来的还是 s1。
  // 读不到（旧版客户端留下的设置、磁盘只读）不该挡住登录，只是这次要重新选目录。
  const dirs = await api.loadRepoDirs();
  state.wcDirs = dirs && dirs.ok && dirs.data ? dirs.data : {};
  state.checkoutOpen = false;
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
  // 常用权重与本机的仓库列表一起刷新：排序是列表的一部分，不能拿上一轮的顺序去渲染。
  await refreshRepoUsage();
  // 本地目录可能已经被删掉：每次刷新仓库页都先核对一遍绑定关系，
  // 失效的登记就地摘掉——列表里不该留着一份不存在的工作副本。
  await syncWorkingCopies();
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
  // 回落也按「常用」序取第一个：一个都还没用过时就是服务端返回的原顺序。
  const kept = repos.some((repo) => repo.repoId === state.selectedRepoId);
  await selectRepo(kept ? state.selectedRepoId : orderedRepos()[0].repoId);
}

/**
 * 拉一次本机记下的「常用」权重。
 *
 * 读不到（旧版主进程、磁盘只读）就当没有记录：列表退回服务端顺序即可，
 * 排序偏好不该挡住仓库页——它只是好看，不是能不能用的前提。
 */
async function refreshRepoUsage() {
  try {
    const res = await api.repoUsage();
    state.repoUsage = res && res.ok && res.data ? res.data : {};
  } catch {
    state.repoUsage = {};
  }
  renderRepoNav();
}

/**
 * 左侧列表的顺序：**常用的浮到最上面**。
 *
 * 权重由主进程记（每切到一个仓库 +1，检出也算一次，按半衰期衰减），界面只负责排：
 * 权重相同看最近一次使用，再相同就保持服务端返回的原始顺序。这样从没用过的仓库
 * 之间不会因为一次重排互相换位，列表看起来是稳定的，只有「用过的」往上浮。
 */
function orderedRepos() {
  const usage = state.repoUsage || {};
  const weightOf = (hit) => (hit && Number.isFinite(hit.weight) ? hit.weight : 0);
  const lastOf = (hit) => (hit && Number.isFinite(hit.last) ? hit.last : 0);
  return state.repos
    .map((repo, index) => ({ repo, index, hit: usage[repo.repoId] }))
    .sort((a, b) => {
      const diff = weightOf(b.hit) - weightOf(a.hit);
      if (diff !== 0) return diff;
      const recency = lastOf(b.hit) - lastOf(a.hit);
      if (recency !== 0) return recency;
      return a.index - b.index;
    })
    .map((item) => item.repo);
}

/** 左侧仓库列表：名字 + HEAD + 「本地有工作副本」的圆点，顺序按「常用」排。 */
function renderRepoNav() {
  const list = el('repo-nav');
  list.innerHTML = '';
  if (state.repos.length === 0) {
    list.innerHTML = '<li class="repo-empty">暂无仓库</li>';
    return;
  }
  for (const repo of orderedRepos()) {
    const li = document.createElement('li');
    const dirs = dirsFor(repo.repoId);
    li.className = `repo-item${repo.repoId === state.selectedRepoId ? ' active' : ''}`;
    li.title = dirs.length
      ? `${repo.name}\n本地工作副本（${dirs.length} 份）：\n${dirs.join('\n')}`
      : repo.name;
    li.innerHTML =
      '<span class="repo-name"></span>' +
      '<span class="repo-dot"></span>' +
      '<span class="repo-rev"></span>';
    li.querySelector('.repo-name').textContent = repo.name;
    li.querySelector('.repo-dot').classList.toggle('on', dirs.length > 0);
    li.querySelector('.repo-rev').textContent = `r${repo.headRev}`;
    li.addEventListener('click', () => {
      // 操作跑着时不换仓库：进度属于当前这份副本，换了上下文界面就自相矛盾
      // （屏幕上是 B 的列表，后台还在写 A 的目录）。
      if (state.job) {
        banner(`正在${JOB_LABEL[state.job]}，完成后再切换仓库`, 'error');
        return;
      }
      // 已经选中的仓库再点一次 = 「回到这个仓库的工作区」（比如刚从建库申请页回来）。
      // 这也算一次使用，但不必把整个工作区重来一遍（那会收起差异面板）。
      if (repo.repoId === state.selectedRepoId) {
        void touchRepo(repo.repoId);
        setView('repo');
        return;
      }
      void guard('切换仓库', () => openRepo(repo.repoId));
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
  // 换仓库 = 换一份工作副本：稀疏勾选、目录树缓存、已生效规则全部重来，
  // 绝不把上一个仓库的选择带过来（目录清单是按仓库缓存的）。
  state.sparse.repoId = null;
  state.sparse.paths = [];
  state.sparse.checked = new Set();
  state.sparse.expanded = new Set();
  state.sparse.rules = [];
  state.sparse.open = false;
  // 清理选择也是按仓库的：换仓库必须清空，绝不把上一个仓库的勾选带过去。
  state.cleanup.open = false;
  state.cleanup.paths = [];
  state.cleanup.checked = new Set();
  state.cleanup.expanded = new Set();
  state.cleanup.repoId = null;
  state.cleanup.busy = false;
  el('cleanup-picker').classList.add('hidden');
  el('cleanup-open').textContent = '选择目录或文件';
  el('cleanup-result').textContent = '';
  renderCleanupSummary();
  // 每个仓库记住一串目录，第一个是上次用的那份；没检出过就回到检出引导。
  state.wcDir = dirsFor(repoId)[0] || '';
  state.checkoutOpen = !state.wcDir;
  state.wcRev = 0;
  state.changes = [];
  state.conflicts = [];
  state.diff = [];
  state.conflict = null;
  // 换了上下文：扫描指纹、选中路径、差异缓存全部作废，
  // 否则新上下文的指纹万一撞上旧的，界面就不会重绘了。
  state.statusSig = '';
  state.diffDirty = true;
  state.detailPath = '';
  // 输入框只用于「再检出一份」的新目录，绝不预填现有工作副本路径：
  // 预填后误点检出会把那一份的本地改动整盘覆盖掉。
  el('dir').value = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('members').innerHTML = '';
  resetMemberPicker();
  state.members = [];
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
  // 停在新仓库的成员页时，成员列表也得跟着换仓库（非 owner 已在 renderOwnerAccess 里退回）。
  if (state.tab === 'members') await guard('加载成员', loadMembers);
  if (state.wcDir) await guard('扫描工作副本', refreshStatus);
}

/**
 * 打开一个仓库：先记一次「用过」，再切上下文。
 *
 * 记录失败（旧版主进程、磁盘只读）不挡切换——最坏情况只是列表维持原顺序。
 */
async function openRepo(repoId) {
  await touchRepo(repoId);
  await selectRepo(repoId);
}

/** 记一次使用并就地重排：常用的那个立刻浮到列表最上面。 */
async function touchRepo(repoId) {
  try {
    const res = await api.touchRepo(repoId);
    if (res && res.ok && res.data) state.repoUsage = res.data;
  } catch {
    /* 记不上就按原顺序显示 */
  }
  renderRepoNav();
}

/** 顶部固定上下文条：仓库名 + HEAD + 本地目录 + 待提交/冲突计数。 */
function renderRepoHead() {
  const repo = currentRepo();
  el('repo-title').textContent = repo ? repo.name : '—';
  el('repo-rev').textContent = repo ? `HEAD r${repo.headRev}` : 'HEAD r0';
  document.title = repo ? `${repo.name} — svn-next` : 'svn-next';
  renderWcBar();
  renderOwnerAccess();
}

/**
 * owner 专属入口：「成员」页与「设置」页里的注销卡片。
 *
 * owner 唯一且不可转让，所以直接比 ownerUserId；不是 owner 时整页（连同 tab）与
 * 注销卡片都不出现——把人领到一个「无权限」的页面上没有意义。服务端仍独立复核。
 */
function isRepoOwner(repo) {
  return Boolean(repo && state.user && repo.ownerUserId && repo.ownerUserId === state.user.userId);
}

function renderOwnerAccess() {
  const owner = isRepoOwner(currentRepo());
  el('tab-members').classList.toggle('hidden', !owner);
  el('danger-card').classList.toggle('hidden', !owner);
  if (!owner && state.tab === 'members') setTab('wc');
}

/** 取某仓库记住的所有工作副本目录；兼容旧设置里残留的单个字符串。 */
function dirsFor(repoId) {
  if (!repoId) return [];
  const value = state.wcDirs[repoId];
  if (Array.isArray(value)) return value.filter((dir) => typeof dir === 'string' && dir.trim());
  return typeof value === 'string' && value.trim() ? [value] : [];
}

/** 清空与「当前这份工作副本」有关的视图状态，切换目录时不串上一份的变更 / 差异。 */
function resetWorkingCopyState() {
  state.wcRev = 0;
  state.changes = [];
  state.conflicts = [];
  state.diff = [];
  state.conflict = null;
  // 换了上下文：扫描指纹、选中路径、差异缓存全部作废，
  // 否则新上下文的指纹万一撞上旧的，界面就不会重绘了。
  state.statusSig = '';
  state.diffDirty = true;
  state.detailPath = '';
  el('changes').innerHTML = '';
  el('diff-path').textContent = '';
  el('diff-path').title = '';
  el('diff').textContent = '点击上方变更查看差异';
  closeConflict();
  setDiffOpen(false);
}

/** 上下文条里的切换器：多份工作副本时才出现，选项文本是完整路径。 */
function renderWcDirOptions() {
  const select = el('wc-dir-select');
  const dirs = dirsFor(state.selectedRepoId);
  select.innerHTML = '';
  for (const dir of dirs) {
    const option = document.createElement('option');
    option.value = dir;
    option.textContent = dir;
    option.title = dir;
    select.appendChild(option);
  }
  select.value = state.wcDir || '';
  select.classList.toggle('hidden', dirs.length < 2);
  // 操作跑着的时候别让人切到另一份副本：屏幕上会变成「B 的列表 + A 的进度」。
  select.disabled = Boolean(state.job);
}

/** 检出卡片里的目录清单：切换 / 移出列表，两者都不动本地文件。 */
function renderWcDirList() {
  const box = el('wc-dir-list');
  const list = el('wc-dir-items');
  const dirs = dirsFor(state.selectedRepoId);
  list.innerHTML = '';
  for (const dir of dirs) {
    const li = document.createElement('li');
    if (dir === state.wcDir) li.classList.add('active');
    const name = document.createElement('span');
    name.className = 'dir-path';
    name.textContent = dir;
    name.title = dir;
    li.appendChild(name);
    if (dir === state.wcDir) {
      const current = document.createElement('span');
      current.className = 'dir-current';
      current.textContent = '当前';
      li.appendChild(current);
    } else {
      const use = document.createElement('button');
      use.className = 'ghost';
      use.type = 'button';
      use.textContent = '切换';
      use.addEventListener('click', () =>
        void guard('切换工作副本', () => switchWorkingCopy(dir)),
      );
      li.appendChild(use);
    }
    const remove = document.createElement('button');
    remove.className = 'ghost';
    remove.type = 'button';
    remove.textContent = '移出列表';
    remove.addEventListener('click', () =>
      void guard('移出工作副本', () => removeWorkingCopy(dir)),
    );
    li.appendChild(remove);
    list.appendChild(li);
  }
  box.classList.toggle('hidden', dirs.length === 0);
}

/** 动盘操作的名字：按钮上写「更新中…」，提示里写「正在更新」。 */
const JOB_LABEL = {
  update: '更新',
  commit: '提交',
  checkout: '检出',
  revert: '还原',
  sparse: '应用稀疏规则',
};

/** 忙碌时按钮上的文案：让人一眼看出「还在跑」而不是「没点动」。 */
const JOB_TEXT = {
  update: '更新中…',
  commit: '提交中…',
  checkout: '检出中…',
  revert: '还原中…',
  sparse: '应用中…',
};

/**
 * 进入一次动盘操作，返回 false 表示已经有别的操作在跑、这一次点击直接作废。
 *
 * 为什么要有这道锁：拉一个大文件、提交一个大文件都是几十秒甚至几分钟的事。按钮虽然
 * 会禁用，但「点下去到界面真的变了」之间有个空档，手快的人在空档里再点一次、
 * 或者先点更新又点提交，都会把两次写盘排到同一个目录上。作废时**不弹提示**——
 * 按钮本来就是灰的，再弹一句「正在更新」只是噪音。
 */
function beginJob(kind) {
  if (state.job) return false;
  state.job = kind;
  // renderWcBar 顺带把上下文条、提交区、稀疏卡片一起刷新（含按钮禁用与文案）。
  renderWcBar();
  return true;
}

/** 结束动盘操作：只由发起它的那个 finally 调用，避免提前解锁。 */
function endJob() {
  state.job = '';
  renderWcBar();
}

function renderWcBar() {
  const dir = state.wcDir;
  const dirs = dirsFor(state.selectedRepoId);
  const hasRepo = Boolean(currentRepo());
  // 有操作在跑时，所有会动同一份副本的按钮一起禁用：它们互相打架。
  const busy = state.job;
  el('wc-path').textContent = dir || '未检出到本地';
  el('wc-path').title = dir;
  el('wc-dot').className = `wc-dot${dir ? ' on' : ''}`;
  const conflicts = state.conflicts.length;
  el('wc-status').textContent = busy
    ? JOB_TEXT[busy]
    : !dir
      ? '检出后即可提交与更新'
      : conflicts > 0
        ? `${conflicts} 个冲突待解决`
        : `${state.changes.length} 个待提交变更 @ r${state.wcRev}`;
  el('wc-status').classList.toggle('warn', conflicts > 0 && !busy);
  // 已有工作副本照样能再检出一份；切换已登记的目录统一走上下文条的下拉。
  el('open-checkout').disabled = !hasRepo || Boolean(busy);
  el('update').disabled = !dir || Boolean(busy);
  el('update').textContent = busy === 'update' ? JOB_TEXT.update : '更新';
  el('commit').disabled = !dir || Boolean(busy);
  el('commit').textContent = busy === 'commit' ? JOB_TEXT.commit : '提交';
  el('message').disabled = busy === 'commit';
  el('revert').disabled = Boolean(busy);
  el('revert').textContent = busy === 'revert' ? JOB_TEXT.revert : '全部还原';
  el('checkout').disabled = Boolean(busy);
  el('checkout').textContent = busy === 'checkout' ? JOB_TEXT.checkout : '检出';
  el('checkout-card').classList.toggle('hidden', Boolean(dir) && !state.checkoutOpen);
  el('workbench').classList.toggle('hidden', !dir);
  el('wc-change-count').textContent = dir ? `${state.changes.length} 项` : '';
  el('checkout-title').textContent = dir ? '再检出一份' : '检出到本地';
  el('checkout-note').textContent = dir
    ? `这个仓库已有 ${dirs.length} 份本地工作副本`
    : '这个仓库还没有本地工作副本';
  el('checkout-hint').textContent = dir
    ? '填一个不同的目录再点「检出」：新铺一份，已有副本不受影响；用「选择…」选到已有工作副本则直接切换或接进来。'
    : '变更、提交、更新都只作用于检出到的那个目录；路径按仓库记住，下次打开会自动带上。用「选择…」选到已有工作副本时会直接接进来，不会重新铺盘。';
  el('cancel-checkout').classList.toggle('hidden', !dir);
  renderWcDirOptions();
  renderWcDirList();
  renderSparseSummary();
}

// ---------------- 稀疏检出 ----------------

/** 与 Rust / flow 层同一套前缀语义：路径相等或位于该目录下，边界必须是 `/`。 */
function sparseUnder(prefix, path) {
  return path === prefix || path.startsWith(`${prefix}/`);
}

/** 去空、去重、排序，删掉被更短前缀覆盖的冗余项（src 已包含 src/api）。 */
function normalizeSparsePaths(paths) {
  const sorted = [
    ...new Set(
      (paths || [])
        .map((p) => String(p).trim().replace(/^\/+|\/+$/g, ''))
        .filter(Boolean),
    ),
  ].sort();
  return sorted.filter(
    (p) => !sorted.some((q) => q.length < p.length && sparseUnder(q, p)),
  );
}

/** 界面勾选 → 本次要用的稀疏规则；空数组 = 全库。 */
function currentSparseRules() {
  return normalizeSparsePaths([...state.sparse.checked]);
}

/** 仓库 HEAD 的扁平相对路径 → 目录树，文件也作为可勾选的叶子。 */
function buildEntryTree(paths) {
  const root = { path: '', name: '', children: new Map(), file: false };
  for (const raw of paths) {
    const value = String(raw).replace(/\\/g, '/').replace(/^\/+/, '');
    const parts = value.split('/').filter(Boolean);
    if (!parts.length) continue;
    let node = root;
    for (let i = 0; i < parts.length - 1; i += 1) {
      const name = parts[i];
      const full = node.path ? `${node.path}/${name}` : name;
      if (!node.children.has(name)) {
        node.children.set(name, { path: full, name, children: new Map(), file: false });
      }
      node = node.children.get(name);
    }
    const leaf = parts[parts.length - 1];
    node.children.set(leaf, { path: value, name: leaf, children: new Map(), file: true });
  }
  return root;
}

function sparseSort(a, b) {
  return Number(a.file) - Number(b.file) || a.name.localeCompare(b.name);
}

/** 父目录已勾中时子节点自动跟着选中；不能单独取消，要先取消父目录。 */
function sparseCheckboxState(path) {
  const checked = [...state.sparse.checked].some((p) => sparseUnder(p, path));
  return { checked, inherited: checked && !state.sparse.checked.has(path) };
}

function sparseTogglePath(node, checked) {
  if (checked) {
    const next = new Set(
      [...state.sparse.checked].filter((p) => !sparseUnder(node.path, p)),
    );
    next.add(node.path);
    state.sparse.checked = next;
  } else {
    state.sparse.checked.delete(node.path);
  }
  renderSparseTree();
  renderSparseSummary();
}

function sparseNodeElement(node) {
  const wrap = document.createElement('div');
  wrap.className = 'sparse-node';
  const row = document.createElement('div');
  row.className = 'sparse-row';
  const hasChildren = node.children.size > 0;

  const caret = document.createElement('button');
  caret.type = 'button';
  caret.className = 'sparse-caret';
  caret.textContent = hasChildren
    ? state.sparse.expanded.has(node.path)
      ? '▾'
      : '▸'
    : '';
  caret.disabled = !hasChildren;
  caret.addEventListener('click', () => {
    if (state.sparse.expanded.has(node.path)) state.sparse.expanded.delete(node.path);
    else state.sparse.expanded.add(node.path);
    renderSparseTree();
  });
  row.appendChild(caret);

  const label = document.createElement('label');
  const input = document.createElement('input');
  input.type = 'checkbox';
  const mark = sparseCheckboxState(node.path);
  input.checked = mark.checked;
  input.disabled = mark.inherited;
  input.addEventListener('change', () => sparseTogglePath(node, input.checked));
  const name = document.createElement('span');
  name.className = `sparse-name${node.file ? ' file' : ''}`;
  name.textContent = node.file ? node.name : `${node.name}/`;
  name.title = node.path;
  label.append(input, name);
  row.appendChild(label);
  wrap.appendChild(row);

  if (hasChildren && state.sparse.expanded.has(node.path)) {
    const children = document.createElement('div');
    children.className = 'sparse-children';
    for (const child of [...node.children.values()].sort(sparseSort)) {
      children.appendChild(sparseNodeElement(child));
    }
    wrap.appendChild(children);
  }
  return wrap;
}

function renderSparseTree() {
  const box = el('sparse-tree');
  box.innerHTML = '';
  if (state.sparse.loading) {
    box.innerHTML = '<div class="sparse-empty">正在读取仓库目录…</div>';
    return;
  }
  if (!state.sparse.paths.length) {
    box.innerHTML =
      '<div class="sparse-empty">点「刷新目录」读取仓库结构；空仓库直接全库检出即可。</div>';
    return;
  }
  const root = buildEntryTree(state.sparse.paths);
  for (const node of [...root.children.values()].sort(sparseSort)) {
    box.appendChild(sparseNodeElement(node));
  }
}

/** 懒加载仓库 HEAD 的路径清单，只在打开面板或切换仓库后才拉。 */
async function loadSparseTree(force = false) {
  const repo = currentRepo();
  if (!repo) return;
  if (!force && state.sparse.repoId === repo.repoId && state.sparse.paths.length) return;
  state.sparse.loading = true;
  state.sparse.repoId = repo.repoId;
  renderSparseTree();
  const paths = unwrap(await api.tree(repo.repoId), '读取仓库目录');
  state.sparse.paths = Array.isArray(paths) ? paths : [];
  state.sparse.loading = false;
  renderSparseTree();
}

/** 从 wc.db 读回真正生效的规则；切换目录 / 检出 / 更新后都要重新对齐。 */
async function loadSparseRules() {
  if (!state.wcDir) {
    state.sparse.rules = [];
    renderSparseSummary();
    return;
  }
  const info = unwrap(await api.wcInfo(state.wcDir), '读取稀疏规则');
  const rules = normalizeSparsePaths(info && info.sparseRules ? info.sparseRules : []);
  state.sparse.rules = rules;
  state.sparse.checked = new Set(rules);
  renderSparseSummary();
  renderSparseTree();
}

function renderSparseSummary() {
  if (!state.wcDir) state.sparse.rules = [];
  const rules = currentSparseRules();
  const active = state.sparse.rules;
  const pending = rules.join(',') !== active.join(',');
  const label = rules.length ? `已选 ${rules.length} 个目录` : '全部目录';
  el('sparse-summary').textContent = state.wcDir
    ? `${active.length ? `已检出 ${active.length} 个目录` : '全库检出'}${pending ? ` · 待应用：${label}` : ''}`
    : `${label}（检出时生效）`;
  el('sparse-rules').textContent = rules.length
    ? `当前选择：${rules.join('、')}`
    : '当前选择：整个仓库（不勾任何目录）';
  el('sparse-apply').classList.toggle('hidden', !state.wcDir);
  el('sparse-apply').disabled = Boolean(state.job) || !state.wcDir;
  el('sparse-apply').textContent = state.job === 'sparse' ? JOB_TEXT.sparse : '应用规则';
  el('sparse-toggle').textContent = state.sparse.open ? '收起' : '选择目录';
  el('sparse-toggle').setAttribute('aria-expanded', String(state.sparse.open));
  el('sparse-body').classList.toggle('hidden', !state.sparse.open);
  // 稀疏规则跟着「检出」卡片走：平时不占版面，点「检出」把卡片摊开时一起露出来。
  // 检出引导态（还没有工作副本）本来就得选目录，所以照样显示。
  const checkoutVisible = !state.wcDir || state.checkoutOpen;
  el('sparse-card').classList.toggle('hidden', !currentRepo() || !checkoutVisible);
}

/** 已检出的副本调整规则：只补拉新增前缀、移除干净条目，绝不重新铺盘。 */
async function applySparseRules() {
  if (!state.wcDir) return;
  if (!beginJob('sparse')) return;
  try {
    const rules = currentSparseRules();
    const result = unwrap(await api.setSparseRules(state.wcDir, rules), '调整稀疏规则');
    state.sparse.rules = normalizeSparsePaths(result && result.rules ? result.rules : rules);
    state.sparse.checked = new Set(state.sparse.rules);
    // 移走的文件连同空目录一起清；目录里还留着用户自己的文件时，必须说明为什么没删干净。
    const parts = [
      `补拉 ${result ? result.added : 0} 个文件`,
      `移除 ${result ? result.removed : 0} 个文件`,
      `清理 ${result ? result.prunedDirs : 0} 个空目录`,
    ];
    const kept = (result && result.keptDirs) || [];
    const tail = kept.length
      ? `；${kept.slice(0, 3).join('、')}${kept.length > 3 ? ' 等' : ''} 里还有未纳入版本控制的文件，已原样保留。`
      : '。';
    banner(`稀疏规则已更新：${parts.join('，')}${tail}`);
    await guard('扫描工作副本', refreshStatus);
  } finally {
    endJob();
    renderSparseTree();
  }
}

// ---------------- 检出、扫描与更新 ----------------

/**
 * 核对本机记下的工作副本还在不在。
 *
 * 用户可能把检出目录整个删掉，此时 settings.json 里那条登记就成了一条虚空路径：
 * 列表里不该再挂着它，点「扫描」更不该把目录重建出来。判据交给主进程去看
 * `.vcs/wc.db`（渲染进程不碰文件系统），失效的登记当场摘掉并落盘。
 */
async function syncWorkingCopies() {
  const res = await api.verifyRepoDirs();
  // 拿不到校验结果（旧版主进程 / 磁盘只读）：保持原样，别把绑定关系误清成空。
  if (!res || !res.ok || !res.data || !res.data.dirs) return;
  state.wcDirs = res.data.dirs;
  const removed = Array.isArray(res.data.removed) ? res.data.removed : [];
  if (removed.length === 0) return;
  // 正在用的那份也没了：退到同仓库剩下的第一份；一份不剩就回到检出引导。
  if (state.wcDir && removed.some((item) => item.dir === state.wcDir)) {
    state.wcDir = dirsFor(state.selectedRepoId)[0] || '';
    state.checkoutOpen = !state.wcDir;
    resetWorkingCopyState();
  }
  renderRepoNav();
  renderWcBar();
  banner(`本地目录已不存在，已从工作副本列表移除：${removed.map((item) => item.dir).join('、')}`);
  if (state.wcDir) await guard('扫描工作副本', refreshStatus);
}

/** 把当前绑定关系落盘；失败不拦流程，只是下次打开要重新选目录。 */
async function saveWorkingCopies() {
  const clean = {};
  for (const repoId of Object.keys(state.wcDirs)) {
    const list = [...new Set(dirsFor(repoId))];
    if (list.length) clean[repoId] = list;
  }
  state.wcDirs = clean;
  const saved = await api.saveRepoDirs(clean);
  if (!saved || !saved.ok) {
    banner('工作副本路径没能保存到本机设置，下次打开需要重新选择目录。', 'error');
  }
}

/**
 * 把目录登记到某个仓库名下，并设为这个仓库最近使用的一份（列表首项）。
 * 一个目录只属于一个仓库：绑定时顺手把它从其它仓库的列表里摘掉——wc.db 里的
 * repoId 才是权威，界面上留两份归属只会让人在 A 仓库里改到 B 仓库的文件。
 */
async function bindWorkingCopy(repoId, dir) {
  const next = {};
  for (const [id, value] of Object.entries(state.wcDirs)) {
    const list = (Array.isArray(value) ? value : value ? [value] : []).filter(
      (item) => item && !(id !== repoId && item === dir),
    );
    if (list.length) next[id] = [...new Set(list)];
  }
  const own = next[repoId] || [];
  next[repoId] = [dir, ...own.filter((item) => item !== dir)];
  state.wcDirs = next;
  if (state.selectedRepoId === repoId) {
    state.wcDir = dir;
    state.checkoutOpen = false;
    el('dir').value = '';
  }
  renderRepoNav();
  renderWcBar();
  await saveWorkingCopies();
}

/** 切换到同一仓库的另一份工作副本；记住选择，重启后仍停在这份上。 */
async function switchWorkingCopy(dir) {
  if (!dir || dir === state.wcDir || !dirsFor(state.selectedRepoId).includes(dir)) return;
  // 换副本同样要避开进行中的操作：它正握着当前那份目录在读写。
  if (state.job) {
    banner(`正在${JOB_LABEL[state.job]}，完成后再切换工作副本`, 'error');
    return;
  }
  const rest = dirsFor(state.selectedRepoId).filter((item) => item !== dir);
  state.wcDirs[state.selectedRepoId] = [dir, ...rest];
  state.wcDir = dir;
  state.checkoutOpen = false;
  resetWorkingCopyState();
  renderRepoNav();
  renderWcBar();
  await saveWorkingCopies();
  // 先给成功反馈：扫描失败时 guard 的错误提示会盖掉它，而不是反过来把错误吞掉。
  banner(`已切换到工作副本：${dir}`);
  await guard('扫描工作副本', refreshStatus);
}

/** 把一份工作副本移出列表（只是不再记住它，本地文件一个字节都不删）。 */
async function removeWorkingCopy(dir) {
  // 正在这份目录上动盘时先别摘登记：操作还在用这个路径。
  if (state.job) {
    banner(`正在${JOB_LABEL[state.job]}，完成后再移出列表`, 'error');
    return;
  }
  await forgetWorkingCopy(dir, `已把「${dir}」移出列表（本地文件未删除）。`);
}

/**
 * 扫描时发现目录已经不是工作副本（用户删掉了检出目录 / 清掉了 `.vcs/`）：
 * 与「移出列表」一样只忘掉这条路径。磁盘上本来已经什么都没有了，
 * **绝不在这里重新检出**——用户删掉的东西不该被一次「扫描」悄悄建回来。
 */
async function forgetMissingWorkingCopy(dir) {
  await forgetWorkingCopy(
    dir,
    `「${dir}」已不是一个工作副本（本地目录不存在），已从列表移除。`,
  );
}

/** 摘掉一条登记，切到同仓库剩下的那份；一份不剩就回到检出引导。 */
async function forgetWorkingCopy(dir, message) {
  const repoId = state.selectedRepoId;
  const rest = dirsFor(repoId).filter((item) => item !== dir);
  if (rest.length) state.wcDirs[repoId] = rest;
  else delete state.wcDirs[repoId];
  if (state.wcDir === dir) {
    state.wcDir = rest[0] || '';
    state.checkoutOpen = !state.wcDir;
    resetWorkingCopyState();
  }
  renderRepoNav();
  renderWcBar();
  await saveWorkingCopies();
  banner(message);
  // 还有别的副本才需要重新扫描；一份不剩时界面已经回到检出引导。
  if (state.wcDir) await guard('扫描工作副本', refreshStatus);
}

/** 打开「再检出一份」：输入框必须清空，不能预填正在用的那份目录。 */
function openCheckout(dir = '') {
  // 上下文条在历史 / 设置 tab 上也在，回到工作副本 tab 才看得见检出卡片。
  setTab('wc');
  el('dir').value = dir;
  state.checkoutOpen = true;
  renderWcBar();
  el('dir').focus();
}

function closeCheckout() {
  state.checkoutOpen = false;
  el('dir').value = '';
  renderWcBar();
}

async function pickCheckoutDir() {
  const dir = unwrap(await api.pickDir(), '选择目录');
  if (!dir) return;
  // 只读探测：选到的目录里本来就有 .vcs/wc.db 时，直接把这份工作副本接进来，
  // 别把路径填进检出输入框——误点一下「检出」就会把已有文件和本地改动整盘重铺。
  const info = unwrap(await api.wcInfo(dir), '读取工作副本信息');
  if (info) {
    await adoptWorkingCopy(dir, info);
    return;
  }
  el('dir').value = dir;
}

/**
 * 把「已经是一份工作副本」的目录接进来，而不是重新检出铺盘。
 *
 * 「选择…」选到从别处拷来的工作副本、或用户在输入框里手敲了这样一个目录，都走这里：
 * wc.db 里的 repoId 是唯一权威，宁可把上下文切到它真正所属的仓库，也不能让一次
 * 「检出」覆盖掉已有文件与本地改动。这就是原先「更换目录」按钮的能力，收进了唯一
 * 的目录选择入口里，界面上不再多一个按钮。
 */
async function adoptWorkingCopy(dir, info) {
  const owner = state.repos.find((repo) => repo.repoId === info.repoId);
  if (!owner) {
    banner(`这个目录属于仓库 ${info.repoId}，但当前账号看不到它，无法在这里使用。`, 'error');
    return;
  }
  el('dir').value = '';
  // 已经记住的另一份：走切换流程，连变更 / 差异等视图状态一起换干净。
  if (owner.repoId === state.selectedRepoId && dirsFor(owner.repoId).includes(dir)) {
    if (dir === state.wcDir) {
      banner(`已经在「${dir}」这份工作副本上（r${info.rev}）。`);
    } else {
      await switchWorkingCopy(dir);
    }
    return;
  }
  // 先收起检出卡片：接进来之后不该还留着「检出」按钮和一条旧路径。
  state.checkoutOpen = false;
  await bindWorkingCopy(owner.repoId, dir);
  if (owner.repoId !== state.selectedRepoId) {
    banner(`这个目录是「${owner.name}」的工作副本，已绑定到该仓库并切换过去。`);
    await openRepo(owner.repoId);
    return;
  }
  await guard('扫描工作副本', refreshStatus);
  banner(`已接上工作副本：${dir}（r${info.rev}），没有重新铺盘。`);
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
  // 检出整库也要占用一份副本：和更新 / 提交共用同一把锁，连点只会铺一次盘。
  if (!beginJob('checkout')) return;
  try {
    // 目录里已经有一份工作副本（含别家仓库的、从别处拷来的）：只读探测到就接进来，
    // 绝不重新铺盘——那会把已有文件与本地改动整盘覆盖掉。手敲路径也别想绕过这条保护。
    const existing = unwrap(await api.wcInfo(dir), '读取工作副本信息');
    if (existing) {
      await adoptWorkingCopy(dir, existing);
      return;
    }
    // 之前登记过、但 wc.db 已经不在了：宁可让用户先移出列表，也不闷头重铺一份。
    if (dirsFor(repo.repoId).includes(dir)) {
      banner(
        `「${dir}」曾登记为「${repo.name}」的工作副本，但目录里已找不到 .vcs/wc.db。` +
          '请先在目录清单里把它「移出列表」，或换一个新目录再检出。',
        'error',
      );
      return;
    }
    const rules = currentSparseRules();
    const result = unwrap(await api.checkout(repo.repoId, dir, 0, rules), '检出');
    await bindWorkingCopy(repo.repoId, result.dir);
    banner(
      `已把「${repo.name}」检出到 ${result.dir}（r${result.rev}，共 ${result.files} 个文件${rules.length ? `，稀疏目录：${rules.join('、')}` : ''}），` +
        `这个仓库现在有 ${dirsFor(repo.repoId).length} 份本地工作副本。`,
    );
    await refreshStatus();
    await loadLog();
    // 主进程把这次检出也记成了一次使用：把新顺序拉回来，常用的仓库浮到最上面。
    await refreshRepoUsage();
  } finally {
    endJob();
  }
}

/** 变更集合的指纹：版本 + 每条路径的状态 + 冲突集合。指纹没变，界面就不用动。 */
function statusSignature(rev, entries, conflicts) {
  return [
    `r${rev}`,
    ...entries.map((entry) => `${entry.kind}:${entry.path}`),
    '--',
    ...conflicts.map((item) => item.path),
  ].join('\n');
}

/**
 * 扫描工作副本。目录里记录的 repoId 与当前仓库不一致时，以目录为准切过去：
 * 宁可换上下文，也不让「提交」把 A 仓库的改动推到 B 仓库。
 *
 * `{ auto: true }` 供后台自动刷新使用（窗口回前台、切 tab、定时器）：结果指纹
 * 没变就一个 DOM 都不碰；差异卡片收着时不读盘；出错也不弹提示——用户没动手，
 * 界面不该自己出声，也不该自己闪。
 */
async function refreshStatus({ auto = false } = {}) {
  const dir = state.wcDir;
  const repoId = state.selectedRepoId;
  if (!dir) {
    if (!auto) banner('这个仓库还没有本地工作副本，请先检出', 'error');
    return;
  }
  // 自动刷新撞上已经在跑的扫描就跳过：下一轮会把这次的改动一起看到。
  if (auto && state.scanning) return;
  state.scanning = true;
  try {
    const data = unwrapMaybe(await api.status(dir), '扫描工作副本', auto);
    if (!data) return;
    // 扫描期间换了仓库 / 工作副本：这份结果属于上一个上下文，丢掉。
    if (state.wcDir !== dir || state.selectedRepoId !== repoId) return;
    // 目录已经不是一个工作副本（被删了 / `.vcs/` 被清掉）：先把这条登记摘掉，
    // 绝不在这里重新检出——那正是「删掉的目录又长回来」的来源。
    if (data.missing) {
      await forgetMissingWorkingCopy(dir);
      return;
    }
    if (data.repoId && data.repoId !== state.selectedRepoId) {
      const owner = state.repos.find((repo) => repo.repoId === data.repoId);
      if (owner && owner.repoId !== state.selectedRepoId) {
        await bindWorkingCopy(owner.repoId, dir);
        banner(`「${dir}」属于仓库「${owner.name}」，已切换到该仓库。`);
        await selectRepo(owner.repoId);
        return;
      }
    }
    const conflicts = data.conflicts || [];
    const sig = statusSignature(data.rev, data.entries, conflicts);
    const changed = sig !== state.statusSig;
    state.wcRev = data.rev;
    state.changes = data.entries;
    state.conflicts = conflicts;
    state.statusSig = sig;
    if (changed) {
      renderWcBar();
      renderChanges();
    }
    // 差异是整棵树读盘算出来的：卡片收着就先标脏，点开某条变更时再生成。
    // 收起状态下自动刷新完全不碰文件内容，轮询才敢一直开着。
    if (state.diffOpen && !state.conflict) {
      const diffs = unwrapMaybe(await api.diff(dir), '生成差异', auto);
      if (diffs) {
        state.diff = diffs;
        state.diffDirty = false;
      }
    } else {
      state.diffDirty = true;
    }
    // 变更列表重建后，把正在看的那条接续回来，差异不跟着被清空。
    if (changed || state.diffOpen) syncDiffPanel();
    // 规则存在 wc.db 里，只会被本客户端改动：自动刷新不必去读，
    // 也避免把用户正在勾选的目录冲掉。
    if (!auto) await guard('读取稀疏规则', loadSparseRules);
  } catch (error) {
    // 手动扫描的失败交给 guard 提示；自动刷新静默（认证失效已在解包处退回登录页）。
    if (!auto) throw error;
  } finally {
    state.scanning = false;
  }
}

/**
 * 扫描完把右侧差异卡片对齐到新的变更列表：
 * - 正在看的那条冲突还在：三栏原样留着（里面可能是手打了一半的合并结果）。
 * - 看的那条已经不在变更里：退回提示态，列表高亮一起清掉。
 * - 其余按最新的 diff 重绘文本。
 */
function syncDiffPanel() {
  const path = state.detailPath;
  if (state.conflict) {
    if (path && state.conflicts.some((item) => item.path === path)) {
      highlightPath(path);
      return;
    }
    closeConflict();
  }
  if (!path || !state.changes.some((change) => change.path === path)) {
    state.detailPath = '';
    highlightPath('');
    el('diff-path').textContent = '';
    el('diff-path').title = '';
    el('diff').textContent =
      state.changes.length === 0 && state.conflicts.length === 0
        ? '工作副本干净'
        : '点击上方变更查看差异';
    return;
  }
  highlightPath(path);
  if (!state.diffOpen) return;
  el('diff-path').textContent = path;
  el('diff-path').title = path;
  paintDiffText(path);
}

/** 把某条变更的差异文本填进卡片；拿不到时说明为什么是空的。 */
function paintDiffText(path) {
  const entry = state.diff.find((item) => item.path === path);
  el('diff').textContent = entry
    ? entry.text
    : '（无文本差异，可能是二进制文件或新增/删除）';
}

/** 按路径点亮变更列表里的那一条：列表重建之后靠它把选中态接回来。 */
function highlightPath(path) {
  for (const active of el('changes').querySelectorAll('li.active')) {
    active.classList.remove('active');
  }
  if (!path) return;
  for (const li of el('changes').querySelectorAll('li')) {
    if (li.title === path) {
      li.classList.add('active');
      return;
    }
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
    li.title = change.path;
    li.querySelector('.badge').textContent = marker;
    li.querySelector('.name').textContent = change.path;
    li.addEventListener('click', () => {
      if (marker === 'C') {
        void guard('读取冲突', () => showConflict(change.path, li));
      } else {
        void guard('读取差异', () => showDiff(change.path, li));
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

/**
 * 容量是只读信息：跟着当前仓库走，不触发任何治理类调用。
 * 扩容申请只留给仓库拥有者——普通成员（只读 / 读写）只能看用量与上限。
 */
async function loadQuota() {
  const repo = currentRepo();
  el('quota-repo').textContent = repo ? repo.name : '';
  const owner = isRepoOwner(repo);
  el('do-expand').classList.toggle('hidden', !owner);
  el('quota-hint').textContent = owner
    ? '容量信息只读；需要更多空间时可以申请扩容：一次固定加 10 GiB，由管理员结合服务器磁盘余量审批。'
    : '容量只读：这里只显示已用空间与配额上限，成员不能申请扩容。';
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

// ---------------- 清理空间：用户主动彻底删除 ----------------

/**
 * 勾选 → 提交给服务端的路径前缀。
 *
 * 服务端按前缀匹配（`path_under_any`），所以勾了 `202610` 就不必再勾它下面的文件；
 * `normalizeSparsePaths` 正好把「已被父路径覆盖的子路径」去掉，两个面板共用一套语义。
 */
function cleanupPaths() {
  return normalizeSparsePaths([...state.cleanup.checked]);
}

function renderCleanupSummary() {
  const count = cleanupPaths().length;
  el('cleanup-summary').textContent = count ? `已选 ${count} 项` : '';
  const blocked = count === 0 || state.cleanup.busy;
  el('cleanup-estimate').disabled = blocked;
  el('cleanup-purge').disabled = blocked;
  el('cleanup-purge').textContent = state.cleanup.busy ? '删除中…' : '彻底删除';
}

/** 父目录已勾中时子节点跟着显示为选中，并禁止单独取消（要先取消父目录）。 */
function cleanupCheckboxState(path) {
  const checked = [...state.cleanup.checked].some((p) => sparseUnder(p, path));
  return { checked, inherited: checked && !state.cleanup.checked.has(path) };
}

function cleanupTogglePath(node, checked) {
  if (checked) {
    const next = new Set(
      [...state.cleanup.checked].filter((p) => !sparseUnder(node.path, p)),
    );
    next.add(node.path);
    state.cleanup.checked = next;
  } else {
    state.cleanup.checked.delete(node.path);
  }
  // 选择变了，上一次的预估就作废：不能让旧数字给新选择背书。
  el('cleanup-result').textContent = '';
  renderCleanupTree();
  renderCleanupSummary();
}

function cleanupNodeElement(node) {
  const wrap = document.createElement('div');
  wrap.className = 'sparse-node';
  const row = document.createElement('div');
  row.className = 'sparse-row';
  const hasChildren = node.children.size > 0;

  const caret = document.createElement('button');
  caret.type = 'button';
  caret.className = 'sparse-caret';
  caret.textContent = hasChildren
    ? state.cleanup.expanded.has(node.path)
      ? '▾'
      : '▸'
    : '';
  caret.disabled = !hasChildren;
  caret.addEventListener('click', () => {
    if (state.cleanup.expanded.has(node.path)) state.cleanup.expanded.delete(node.path);
    else state.cleanup.expanded.add(node.path);
    renderCleanupTree();
  });
  row.appendChild(caret);

  const label = document.createElement('label');
  const input = document.createElement('input');
  input.type = 'checkbox';
  const mark = cleanupCheckboxState(node.path);
  input.checked = mark.checked;
  input.disabled = mark.inherited;
  input.addEventListener('change', () => cleanupTogglePath(node, input.checked));
  const name = document.createElement('span');
  name.className = `sparse-name${node.file ? ' file' : ''}`;
  name.textContent = node.file ? node.name : `${node.name}/`;
  name.title = node.path;
  label.append(input, name);
  row.appendChild(label);
  wrap.appendChild(row);

  if (hasChildren && state.cleanup.expanded.has(node.path)) {
    const children = document.createElement('div');
    children.className = 'sparse-children';
    for (const child of [...node.children.values()].sort(sparseSort)) {
      children.appendChild(cleanupNodeElement(child));
    }
    wrap.appendChild(children);
  }
  return wrap;
}

function renderCleanupTree() {
  const box = el('cleanup-tree');
  box.innerHTML = '';
  if (state.cleanup.loading) {
    box.innerHTML = '<div class="sparse-empty">正在读取仓库目录…</div>';
    return;
  }
  if (!state.cleanup.paths.length) {
    box.innerHTML = '<div class="sparse-empty">仓库里没有文件，不需要清理。</div>';
    return;
  }
  const root = buildEntryTree(state.cleanup.paths);
  for (const node of [...root.children.values()].sort(sparseSort)) {
    box.appendChild(cleanupNodeElement(node));
  }
}

/** 只在第一次打开面板时拉目录；之后切仓库会清空缓存，强制重读。 */
async function loadCleanupTree(force = false) {
  const repo = currentRepo();
  if (!repo) return;
  if (!force && state.cleanup.repoId === repo.repoId && state.cleanup.paths.length) return;
  state.cleanup.loading = true;
  state.cleanup.repoId = repo.repoId;
  renderCleanupTree();
  const paths = unwrap(await api.tree(repo.repoId), '读取仓库目录');
  state.cleanup.paths = Array.isArray(paths) ? paths : [];
  state.cleanup.loading = false;
  renderCleanupTree();
}

function openCleanupPicker() {
  state.cleanup.open = true;
  el('cleanup-picker').classList.remove('hidden');
  // 删除界面不能拿旧树做决定：每次摊开都重读一次 HEAD（提交 / 更新后目录可能已经变了）。
  el('cleanup-open').textContent = '刷新目录';
  renderCleanupSummary();
  void guard('读取仓库目录', () => loadCleanupTree(true));
}

function closeCleanupPicker() {
  state.cleanup.open = false;
  el('cleanup-picker').classList.add('hidden');
  el('cleanup-open').textContent = '选择目录或文件';
}

/** dry-run 预估：只回报能释放多少，不动任何数据。 */
async function estimateCleanup() {
  const repo = currentRepo();
  const paths = cleanupPaths();
  if (!repo || !paths.length) return;
  const estimate = unwrap(await api.estimatePurge(repo.repoId, paths), '预估释放量');
  const shared = estimate.sharedPaths.length
    ? '；其中部分内容与其它路径或仓库共享，不会被回收'
    : '';
  el('cleanup-result').textContent =
    `预计释放 ${humanBytes(estimate.reclaimableBytes)}，影响 ${estimate.affectedRevisions} 个历史版本${shared}。`;
}

/**
 * 彻底删除：二次确认后提交。成功后配额与目录树都要重读——
 * 删除会推进 HEAD，旧树里已经没有这些路径了。
 */
async function purgeCleanup() {
  const repo = currentRepo();
  const paths = cleanupPaths();
  if (!repo || !paths.length || state.cleanup.busy) return;
  const shown = paths.slice(0, 6).join('\n');
  const more = paths.length > 6 ? `\n…等 ${paths.length} 项` : '';
  const ok = window.confirm(
    `彻底删除以下路径？\n\n${shown}${more}\n\n内容字节会被立即回收，无法撤销；` +
      '本地工作副本里的文件不会被自动删除。',
  );
  if (!ok) return;

  state.cleanup.busy = true;
  renderCleanupSummary();
  try {
    const outcome = unwrap(await api.purge(repo.repoId, paths, '桌面端彻底删除'), '彻底删除');
    state.cleanup.checked = new Set();
    state.cleanup.paths = [];
    el('cleanup-result').textContent = `已彻底删除 ${paths.length} 项，生成 r${outcome.newRev}，空间已回收。`;
    banner('已彻底删除，空间已回收');
    await guard('加载容量', loadQuota);
    await guard('读取仓库目录', () => loadCleanupTree(true));
  } finally {
    state.cleanup.busy = false;
    renderCleanupSummary();
  }
}

// ---------------- 成员管理（仅仓库拥有者） ----------------

/** 与 proto 的 RepoMemberRole 一致：1 owner / 2 maintainer / 3 writer / 4 reader。 */
const MEMBER_ROLE_LABELS = { 1: '拥有者', 2: '维护者', 3: '读写', 4: '只读' };
/**
 * 客户端只放两种权限：读写、只读。维护者是旧版本留下的角色，
 * 名单里遇到时仍要如实显示（否则下拉会把 2 显示成 3，等于骗人），
 * 但不再作为选项发给任何人。
 */
const ASSIGNABLE_ROLES = [3, 4];

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
  // 名单变了，候选里可能有人已经是成员：按最新名单过滤一次再重画。
  pruneCandidates();
  renderMembers();
}

/** 把已经是成员的人从候选里剔掉；如果他正好是当前选中的人，选中态一并作废。 */
function pruneCandidates() {
  const existing = new Set(state.members.map((member) => member.userId));
  state.candidates = state.candidates.filter((user) => !existing.has(user.userId));
  if (state.picked && existing.has(state.picked.userId)) clearPick();
  renderCandidates();
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
      // 旧成员可能还停在「维护者」：把当前值补进选项里，让下拉显示真实角色，
      // 拥有者选中只读/读写后即可把它降下来。
      const options = ASSIGNABLE_ROLES.includes(member.role)
        ? ASSIGNABLE_ROLES
        : [member.role, ...ASSIGNABLE_ROLES];
      for (const role of options) {
        const option = document.createElement('option');
        option.value = String(role);
        option.textContent = ASSIGNABLE_ROLES.includes(role)
          ? roleLabel(role)
          : `${roleLabel(role)}（旧角色，请改为只读或读写）`;
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
  await loadMembers();
}

/**
 * 搜人只负责「找到人」，不带任何权限分类：输入即搜（防抖），结果落在下拉里。
 * 权限是选中之后才决定的事——先按权限筛人，等于逼着人还没认全就先想清楚给什么。
 */
const MEMBER_SEARCH_DELAY = 250;

function scheduleSearch() {
  window.clearTimeout(state.searchTimer);
  const query = el('member-query').value.trim();
  if (!query) {
    state.candidates = [];
    state.candidateIndex = -1;
    state.searching = false;
    state.searchSeq += 1;
    renderCandidates();
    return;
  }
  // 等一小会儿再发请求：连续输入时只查最后一次，别把人名拆成五个请求。
  state.searchTimer = window.setTimeout(
    () => void guard('搜索用户', () => searchMembers(query)),
    MEMBER_SEARCH_DELAY,
  );
}

async function searchMembers(query) {
  const repo = currentRepo();
  if (!repo || !query.trim()) return;
  const seq = ++state.searchSeq;
  state.searching = true;
  state.candidates = [];
  state.candidateIndex = -1;
  renderCandidates();
  let users;
  try {
    users = unwrap(await api.searchUsers(query.trim()), '搜索用户');
  } catch (err) {
    // 失败也要把「正在搜索…」收掉，否则下拉会一直挂在那句话上。
    if (seq === state.searchSeq) {
      state.searching = false;
      renderCandidates();
    }
    throw err;
  }
  // 慢响应回来的顺序不一定是发出去的顺序：只认最后一次输入的结果。
  if (seq !== state.searchSeq) return;
  state.searching = false;
  const existing = new Set(state.members.map((member) => member.userId));
  state.candidates = users.filter((user) => !existing.has(user.userId));
  state.candidateIndex = state.candidates.length > 0 ? 0 : -1;
  renderCandidates();
}

/** 下拉内容：只列可添加的人，点一下（或回车）就是「选中」，不直接加进去。 */
function renderCandidates() {
  const list = el('member-candidates');
  const input = el('member-query');
  const query = input.value.trim();
  const open = Boolean(query) && !state.picked;
  list.innerHTML = '';
  list.classList.toggle('hidden', !open);
  input.setAttribute('aria-expanded', String(open));
  if (!open) {
    state.candidateIndex = -1;
    return;
  }
  if (state.candidates.length === 0) {
    const li = document.createElement('li');
    li.className = 'empty';
    li.textContent = state.searching
      ? '正在搜索…'
      : '没有可添加的账号：对方可能已经是成员，或账号还没通过审批。';
    list.appendChild(li);
    return;
  }
  state.candidates.forEach((user, index) => {
    const li = document.createElement('li');
    li.setAttribute('role', 'option');
    li.setAttribute('aria-selected', String(index === state.candidateIndex));
    li.classList.toggle('active', index === state.candidateIndex);
    li.innerHTML =
      '<span class="member-name"><span class="who-name"></span><span class="who-id"></span></span>' +
      '<span class="pick-hint">选择</span>';
    li.querySelector('.who-name').textContent = user.displayName || user.username;
    li.querySelector('.who-id').textContent = `@${user.username}`;
    // 用 mousedown 挡住输入框失焦，click 才能稳定落到这一行上。
    li.addEventListener('mousedown', (event) => event.preventDefault());
    li.addEventListener('click', () => pickCandidate(user));
    list.appendChild(li);
  });
}

/** 选中一个人：输入框收成他的名字，下拉收起，下面露出「权限 + 添加」。 */
function pickCandidate(user) {
  state.picked = user;
  state.candidates = [];
  state.candidateIndex = -1;
  const name = user.displayName || user.username;
  el('member-query').value = name;
  el('picked-name').textContent = name;
  el('picked-id').textContent = `@${user.username}`;
  el('member-role').value = '3';
  el('member-picked').classList.remove('hidden');
  renderCandidates();
}

/** 取消选中：keepQuery 用于「又继续在输入框里改字」的场景，别把刚打的字抹掉。 */
function clearPick({ keepQuery = false } = {}) {
  state.picked = null;
  el('member-picked').classList.add('hidden');
  el('picked-name').textContent = '';
  el('picked-id').textContent = '';
  if (!keepQuery) el('member-query').value = '';
  renderCandidates();
}

/** 换仓库 / 退出登录：候选、选中态、待发的搜索请求全部作废。 */
function resetMemberPicker() {
  state.candidates = [];
  state.candidateIndex = -1;
  state.searching = false;
  state.picked = null;
  state.searchSeq += 1;
  window.clearTimeout(state.searchTimer);
  el('member-query').value = '';
  el('member-candidates').innerHTML = '';
  el('member-candidates').classList.add('hidden');
  el('member-query').setAttribute('aria-expanded', 'false');
  el('member-picked').classList.add('hidden');
}

/** 添加：权限到这里才选。按钮在请求期间禁用，避免连点加出重复成员。 */
async function addPickedMember() {
  const repo = currentRepo();
  const user = state.picked;
  if (!repo || !user) return;
  const role = Number(el('member-role').value);
  const button = el('add-member');
  if (button.disabled) return;
  button.disabled = true;
  button.textContent = '添加中…';
  try {
    unwrap(await api.addMember(repo.repoId, user.userId, role), '添加成员');
    banner(`已把「${user.displayName || user.username}」加入「${repo.name}」，权限：${roleLabel(role)}。`);
    clearPick();
    await loadMembers();
  } finally {
    button.disabled = false;
    button.textContent = '添加';
  }
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

/** 扩容申请同样不填容量：一次固定 +10 GiB，由管理员按磁盘余量批准；仅拥有者可提交。 */
async function requestExpansion() {
  const repo = currentRepo();
  if (!repo) {
    banner('请先在左侧选择一个仓库', 'error');
    return;
  }
  if (!isRepoOwner(repo)) {
    banner('只有仓库拥有者可以申请扩容，成员只能查看容量。', 'error');
    return;
  }
  const result = unwrap(await api.requestRepoExpansion(repo.repoId, ''), '提交扩容申请');
  banner(`已提交「${repo.name}」的扩容申请（${result.requestId}）：通过后上限增加 10 GiB。`);
}

/**
 * 注销仓库（owner 专属）。
 *
 * 只做软删除：服务端把仓库挪进回收站，保留期内管理员可原样恢复，到期才彻底删除。
 * 本地工作副本一律不动——恢复之后绑定还在，接着用即可；就算彻底删了，
 * 也绝不替用户删本地文件。
 */
async function deleteCurrentRepo() {
  const repo = currentRepo();
  if (!repo) {
    banner('请先在左侧选择一个仓库', 'error');
    return;
  }
  if (state.job) {
    banner(`正在${JOB_LABEL[state.job]}，完成后再注销仓库`, 'error');
    return;
  }
  if (!isRepoOwner(repo)) {
    banner('只有仓库拥有者可以注销仓库。', 'error');
    return;
  }
  const ok = window.confirm(
    `注销「${repo.name}」后仓库立即不可用，所有在途提交作废；` +
      '保留期内可由管理员在管理端恢复，到期会彻底删除并释放空间。\n\n' +
      '本地工作副本不会被删除。确定注销？',
  );
  if (!ok) return;
  const result = unwrap(await api.deleteRepo(repo.repoId, ''), '注销仓库');
  const until = result && result.purgeAfter
    ? new Date(result.purgeAfter * 1000).toLocaleString('zh-CN', { hour12: false })
    : '';
  state.repos = state.repos.filter((item) => item.repoId !== repo.repoId);
  state.selectedRepoId = null;
  banner(
    until
      ? `仓库「${repo.name}」已注销：可在 ${until} 前请管理员恢复，之后彻底删除。`
      : `仓库「${repo.name}」已注销。`,
  );
  await loadRepos();
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
  state.detailPath = path;
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
  // 冲突编辑器收起 = 差异卡片上没有再看具体哪一条了，选中路径一并清掉。
  state.detailPath = '';
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

/**
 * 看差异。差异文本按需生成：自动刷新只把 diff 标脏、不读盘，所以这里可能
 * 要先补拉一次——打开某条变更才付出读整棵树的代价。
 */
async function showDiff(path, node) {
  highlight(node);
  // 从冲突编辑器切回普通差异：先把三栏收掉，否则 diff 还被 hidden 着。
  closeConflict();
  setDiffOpen(true);
  state.detailPath = path;
  el('diff-path').textContent = path;
  el('diff-path').title = path;
  if (state.diffDirty) {
    el('diff').textContent = '正在读取差异…';
    const dir = state.wcDir;
    const diffs = unwrap(await api.diff(dir), '生成差异');
    state.diff = diffs;
    state.diffDirty = false;
    // 读盘期间又点了别的条目：让最后一次点击说了算。
    if (state.detailPath !== path) return;
  }
  paintDiffText(path);
}

// ---------------- 提交 / 更新 / 还原 ----------------

/**
 * 提交。提交信息可以留空——主进程会按本次变更自动生成一条，
 * 界面上不必为了「写一句话」把整次提交挡住。
 *
 * 大文件让这件事从「几秒的网络往返」变成了「几分钟的读盘 + 上传 + 写基线」：
 * 按钮禁用、文案换成「提交中…」、上下文条显示进度，`beginJob` 同时挡住连点。
 */
async function commit() {
  const dir = state.wcDir;
  if (!dir) {
    banner('请先检出工作副本', 'error');
    return;
  }
  if (state.conflicts.length > 0) {
    banner('存在未解决的冲突，请先解决后再提交', 'error');
    return;
  }
  if (state.changes.length === 0) {
    banner('工作副本没有变更，不需要提交', 'error');
    return;
  }
  if (!beginJob('commit')) return;
  const message = el('message').value.trim();
  try {
    const result = unwrap(await api.commit(dir, message), '提交');
    el('message').value = '';
    banner(
      `已提交 r${result.rev}：${result.message}（新增 ${result.added}，修改 ${result.modified}，` +
        `删除 ${result.deleted}；上传 ${result.pushed} 个对象，服务端已有内容不重复上传）`,
    );
    // 提交会推进仓库 HEAD：列表里的版本号与历史都要跟着刷新。
    await loadRepos();
  } finally {
    endJob();
  }
}

/**
 * 拉取服务端更新。
 *
 * 几个 GB 的文件在这里要下载、铺盘、再扫一遍重算哈希，全程几十秒到几分钟；
 * 期间按钮禁用并显示「更新中…」，再点一次不会发出第二个请求（`beginJob` 直接返回）。
 */
async function update() {
  const dir = state.wcDir;
  if (!dir) {
    banner('请先检出工作副本', 'error');
    return;
  }
  if (!beginJob('update')) return;
  try {
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
  } finally {
    endJob();
  }
}

async function revertAll() {
  const dir = state.wcDir;
  if (!dir) {
    banner('请先检出工作副本', 'error');
    return;
  }
  if (!beginJob('revert')) return;
  try {
    const restored = unwrap(await api.revert(dir, []), '还原');
    banner(`已把 ${restored} 个条目还原到基线内容（未纳入版本控制的文件不会被删除）`);
    await refreshStatus();
  } finally {
    endJob();
  }
}

async function disconnect() {
  unwrap(await api.disconnect(), '退出登录');
  state.repos = [];
  state.selectedRepoId = null;
  state.wcDir = '';
  state.changes = [];
  state.conflicts = [];
  state.diff = [];
  state.statusSig = '';
  state.diffDirty = true;
  state.detailPath = '';
  el('who').textContent = '';
  el('repo-nav').innerHTML = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('members').innerHTML = '';
  resetMemberPicker();
  state.members = [];
  // 退出登录时作废进行中的操作标记：新的账号进来不该继承一把「有操作在跑」的锁。
  state.job = '';
  setTab('wc');
  setDiffOpen(false);
  el('diff-path').textContent = '';
  el('diff-path').title = '';
  el('diff').textContent = '点击上方变更查看差异';
  // 目录记忆留在 settings.json 的**这个账号名下**，清的只是内存状态；
  // 下次同一个人登录会重新读回来，换成别人的账号则读不到这一份。
  state.wcDirs = {};
  // 「常用」权重同理：属于账号的本机偏好，退出登录时从内存里清掉。
  state.repoUsage = {};
  state.checkoutOpen = false;
  el('dir').value = '';
  renderWcBar();
  showLogin('已退出登录。');
}

// ---------------- 工作副本自动刷新 ----------------
/**
 * 变更是磁盘上的事实，不是界面里的一段记忆：在外面改完文件切回来，列表就该是新的。
 * 原来要人手动点一下「重新扫描」，那等于把「看新状态」这件事交给用户记着。
 *
 * 三个时机够覆盖全部日常场景：
 * 1. 窗口回到前台（focus / visibilitychange）——改完文件切回来最常见；
 * 2. 切回「工作副本」tab——在历史 / 容量页停了一会儿回来；
 * 3. 可见时每 8 秒一次轮询——窗口一直开着、文件被构建脚本改写。
 *
 * 扫描走 status 的 stat 快路径（mtime + size 没变就不算哈希），结果指纹没变时
 * 一个 DOM 都不碰，差异卡片收着时也不读文件内容，所以轮询开着界面不会自己闪。
 */
const AUTO_SCAN_INTERVAL_MS = 8000;

/** 自动刷新的前置条件：登录了、在仓库页、有工作副本、没有正在动盘或正在合并。 */
function canAutoScan() {
  if (!state.user || document.hidden) return false;
  if (!state.wcDir || !currentRepo()) return false;
  // `job` 覆盖更新 / 提交 / 检出 / 还原 / 稀疏规则：盘正在被改到一半时不去扫。
  if (state.scanning || state.job) return false;
  // 正在手动解决冲突时不打断：三栏里可能是打了一半的合并结果。
  if (state.conflict) return false;
  return true;
}

function autoScan() {
  if (!canAutoScan()) return;
  void refreshStatus({ auto: true });
}

window.addEventListener('focus', autoScan);
document.addEventListener('visibilitychange', () => {
  if (!document.hidden) autoScan();
});
window.setInterval(autoScan, AUTO_SCAN_INTERVAL_MS);

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
el('cleanup-open').addEventListener('click', () => {
  if (state.cleanup.open) void guard('读取仓库目录', () => loadCleanupTree(true));
  else openCleanupPicker();
});
el('cleanup-close').addEventListener('click', closeCleanupPicker);
el('cleanup-estimate').addEventListener('click', () => void guard('预估释放量', estimateCleanup));
el('cleanup-purge').addEventListener('click', () => void guard('彻底删除', purgeCleanup));
el('delete-repo').addEventListener('click', () => void guard('注销仓库', deleteCurrentRepo));
el('disconnect').addEventListener('click', () => void guard('退出登录', disconnect));
el('pick-checkout-dir').addEventListener('click', () => void guard('选择目录', pickCheckoutDir));
// 稀疏检出：目录树按需加载，勾选只改界面状态，已检出后点「应用规则」才落盘。
el('sparse-toggle').addEventListener('click', () => {
  state.sparse.open = !state.sparse.open;
  renderSparseSummary();
  if (state.sparse.open) void guard('读取仓库目录', () => loadSparseTree());
});
el('sparse-reload').addEventListener('click', () =>
  void guard('刷新仓库目录', () => loadSparseTree(true)),
);
el('sparse-all').addEventListener('click', () => {
  state.sparse.checked = new Set();
  renderSparseTree();
  renderSparseSummary();
});
el('sparse-apply').addEventListener('click', () =>
  void guard('应用稀疏规则', applySparseRules),
);
el('open-checkout').addEventListener('click', () => openCheckout());
el('cancel-checkout').addEventListener('click', closeCheckout);
el('wc-dir-select').addEventListener('change', (event) =>
  void guard('切换工作副本', () => switchWorkingCopy(event.target.value)),
);
el('checkout').addEventListener('click', () => void guard('检出', checkout));
el('update').addEventListener('click', () => void guard('更新', update));
el('revert').addEventListener('click', () => void guard('还原', revertAll));
el('toggle-diff').addEventListener('click', () => setDiffOpen(!state.diffOpen));
el('close-diff').addEventListener('click', () => setDiffOpen(false));
el('commit').addEventListener('click', () => void guard('提交', commit));
el('refresh-log').addEventListener('click', () => void guard('刷新历史', loadLog));
// 成员页（owner 专属）：名单跟随仓库与增删自动刷新，搜索下拉输入即搜，选人之后再定权限。
el('member-query').addEventListener('input', () => {
  // 选中之后再改字 = 重新找人：先撤掉选中态，别让人以为加的还是原来那个。
  if (state.picked) clearPick({ keepQuery: true });
  scheduleSearch();
});
el('member-query').addEventListener('keydown', (event) => {
  const open = !el('member-candidates').classList.contains('hidden');
  const size = state.candidates.length;
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    if (!open || size === 0) return;
    event.preventDefault();
    const delta = event.key === 'ArrowDown' ? 1 : -1;
    state.candidateIndex = (Math.max(state.candidateIndex, 0) + delta + size) % size;
    renderCandidates();
    return;
  }
  if (event.key === 'Enter') {
    event.preventDefault();
    if (open && size > 0) {
      pickCandidate(state.candidates[Math.max(state.candidateIndex, 0)]);
      return;
    }
    // 结果还没回来（防抖未触发）时按回车 = 立刻搜一次。
    const query = el('member-query').value.trim();
    if (query) {
      window.clearTimeout(state.searchTimer);
      void guard('搜索用户', () => searchMembers(query));
    }
    return;
  }
  if (event.key === 'Escape') {
    if (open) {
      event.preventDefault();
      state.candidates = [];
      state.candidateIndex = -1;
      renderCandidates();
    } else if (state.picked) {
      clearPick();
    }
  }
});
// 点到别处：下拉收起，但不丢已经选好的人。
document.addEventListener('click', (event) => {
  if (event.target.closest('.combo')) return;
  el('member-candidates').classList.add('hidden');
  el('member-query').setAttribute('aria-expanded', 'false');
});
el('add-member').addEventListener('click', () => void guard('添加成员', addPickedMember));
el('cancel-pick').addEventListener('click', () => clearPick());
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
