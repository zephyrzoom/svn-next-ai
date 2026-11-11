/**
 * 渲染进程：单人提交闭环的界面逻辑。
 *
 * 这里没有任何网络或文件系统能力——全部经 `window.vcs`（preload 暴露）转交主进程。
 * 每个调用都返回统一信封 `{ok, data} | {ok:false, code, message}`，业务错误直接展示
 * `message`，认证类错误（11/12/19/20）额外提示「请重新连接」。
 */
const api = window.vcs;

const AUTH_CODES = new Set([11, 12, 19, 20]);

const state = {
  user: null,
  conflicts: [],
  repos: [],
  selectedRepoId: null,
  dir: '',
  changes: [],
  diff: [],
  cwd: '',
  registrations: [],
  repoRequests: [],
  members: [],
  memberCandidates: [],
  purgeCandidates: [],
  purged: [],
  policies: [],
};

const QUOTA_SOURCE_LABEL = { 1: '单仓库覆盖', 2: '全局默认', 3: '系统兜底' };

/** 与 proto 的 RepoMemberRole 一致。 */
const MEMBER_ROLE = { OWNER: 1, MAINTAINER: 2, WRITER: 3, READER: 4 };

const ROLE_ADMIN = 3;

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
    return await run();
  } catch (err) {
    if (!el('banner').textContent) {
      banner(`${action}失败：${err.message}`, 'error');
    }
    return undefined;
  }
}

// ---------------- 连接与仓库 ----------------

async function connect() {
  const target = el('target').value.trim();
  const username = el('username').value.trim();
  const password = el('password').value;
  if (!target || !username || !password) {
    banner('请填写服务端地址、用户名与密码', 'error');
    return;
  }
  const data = unwrap(await api.connect(target, username, password), '连接');
  el('password').value = '';
  await applySession(data.user);
}

/** 登录成功与「用保存的凭证恢复」共用同一段收尾逻辑。 */
async function applySession(user) {
  state.user = user;
  el('who').textContent = `${user.displayName || user.username} · ${roleLabel(user.role)}`;
  el('disconnect').classList.remove('hidden');
  applyRole(user.role);
  await loadRepos();
  if (user.role === ROLE_ADMIN) await loadAdmin();
}

function roleLabel(role) {
  return { 1: '只读', 2: '可写', 3: '管理员' }[role] || '未知角色';
}

/**
 * 角色决定能看到哪些入口：管理员有「直通新建」和审批台，普通用户只有「提交建库申请」。
 * 这只是界面层的显隐，服务端每个入口都有独立的权限校验，不依赖这里的判断。
 */
function applyRole(role) {
  const isAdmin = role === ROLE_ADMIN;
  el('repo-create-row').classList.toggle('hidden', !isAdmin);
  el('repo-request-row').classList.toggle('hidden', isAdmin);
  el('admin-panel').classList.toggle('hidden', !isAdmin);
  el('repo-hint').textContent = isAdmin
    ? '管理员可用「直通新建」跳过审批；普通用户的申请会出现在审批台里。'
    : '普通用户的仓库需要管理员审批后才能使用；审批通过后会自动出现在上面的列表里。';
}

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
    list.innerHTML = '<li class="empty">还没有仓库，先在下面新建一个</li>';
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
  void loadMembers(repoId);
  void loadSpace(repoId);
}

async function createRepo() {
  const name = el('new-repo').value.trim();
  if (!name) {
    banner('请填写仓库名', 'error');
    return;
  }
  const repo = unwrap(await api.createRepo(name), '新建仓库');
  el('new-repo').value = '';
  banner(`仓库 ${repo.name} 已创建（${repo.repoId}）。审批通过的仓库会自动出现在列表里。`);
  await loadRepos();
  selectRepo(repo.repoId);
}

// ---------------- 成员管理 ----------------

function memberRoleLabel(role) {
  return { 1: 'owner', 2: 'maintainer', 3: 'writer', 4: 'reader' }[role] || '?';
}

async function loadMembers(repoId) {
  const repo = state.repos.find((item) => item.repoId === repoId);
  el('members-repo').textContent = repo ? repo.name : '';
  state.members = unwrap(await api.listMembers(repoId), '加载成员');
  renderMembers();
}

function renderMembers() {
  const list = el('members');
  list.innerHTML = '';
  if (state.members.length === 0) {
    list.innerHTML = '<li class="empty">没有成员</li>';
    return;
  }
  for (const item of state.members) {
    const li = document.createElement('li');
    li.innerHTML = '<span class="who"></span>';
    li.querySelector('.who').textContent = `${item.username}（${item.displayName}）`;

    if (item.role === MEMBER_ROLE.OWNER) {
      // owner 唯一且不可移除 / 降级：界面上不给操作，避免用户点了才被告知。
      const tag = document.createElement('span');
      tag.className = 'owner-tag';
      tag.textContent = 'owner';
      li.appendChild(tag);
    } else {
      const select = document.createElement('select');
      for (const [value, label] of [
        [MEMBER_ROLE.MAINTAINER, 'maintainer'],
        [MEMBER_ROLE.WRITER, 'writer'],
        [MEMBER_ROLE.READER, 'reader'],
      ]) {
        const option = document.createElement('option');
        option.value = String(value);
        option.textContent = label;
        if (value === item.role) option.selected = true;
        select.appendChild(option);
      }
      select.addEventListener('change', () => {
        void guard('修改成员角色', () =>
          changeMemberRole(item.userId, item.username, Number(select.value)),
        );
      });
      li.appendChild(select);

      const remove = document.createElement('button');
      remove.className = 'ghost';
      remove.textContent = '移除';
      remove.addEventListener('click', () => {
        void guard('移除成员', () => removeMember(item.userId, item.username));
      });
      li.appendChild(remove);
    }
    list.appendChild(li);
  }
}

async function changeMemberRole(userId, username, role) {
  unwrap(await api.setMemberRole(state.selectedRepoId, userId, role), '修改成员角色');
  banner(
    `已把 ${username} 改为 ${memberRoleLabel(role)}` +
      (role === MEMBER_ROLE.READER ? '；其手上的提交会话已作废。' : '。'),
  );
  await loadMembers(state.selectedRepoId);
}

async function removeMember(userId, username) {
  unwrap(await api.removeMember(state.selectedRepoId, userId), '移除成员');
  banner(`已把 ${username} 移出该仓库，其未完成的提交会话已作废。`);
  await loadMembers(state.selectedRepoId);
}

async function searchUsers() {
  const query = el('member-query').value.trim();
  if (!query) {
    banner('请输入用户名或姓名', 'error');
    return;
  }
  state.memberCandidates = unwrap(await api.searchUsers(query), '搜索用户');
  renderCandidates();
}

function renderCandidates() {
  const list = el('member-candidates');
  list.innerHTML = '';
  if (state.memberCandidates.length === 0) {
    list.innerHTML = '<li class="empty">没有匹配的账号（只有 ACTIVE 用户可被添加）</li>';
    return;
  }
  const existing = new Set(state.members.map((item) => item.userId));
  for (const candidate of state.memberCandidates) {
    const li = document.createElement('li');
    li.innerHTML = '<span class="who"></span>';
    li.querySelector('.who').textContent =
      `${candidate.username}（${candidate.displayName}）`;
    if (existing.has(candidate.userId)) {
      const tag = document.createElement('span');
      tag.className = 'owner-tag';
      tag.textContent = '已是成员';
      li.appendChild(tag);
    } else {
      const select = document.createElement('select');
      for (const [value, label] of [
        [MEMBER_ROLE.WRITER, 'writer'],
        [MEMBER_ROLE.READER, 'reader'],
        [MEMBER_ROLE.MAINTAINER, 'maintainer'],
      ]) {
        const option = document.createElement('option');
        option.value = String(value);
        option.textContent = label;
        select.appendChild(option);
      }
      li.appendChild(select);

      const add = document.createElement('button');
      add.className = 'primary';
      add.textContent = '添加';
      add.addEventListener('click', () => {
        void guard('添加成员', () =>
          addMember(candidate.userId, candidate.username, Number(select.value)),
        );
      });
      li.appendChild(add);
    }
    list.appendChild(li);
  }
}

async function addMember(userId, username, role) {
  unwrap(await api.addMember(state.selectedRepoId, userId, role), '添加成员');
  banner(`已把 ${username} 加为 ${memberRoleLabel(role)}。`);
  await loadMembers(state.selectedRepoId);
  renderCandidates();
}

// ---------------- 配额设置 ----------------

function openQuotaModal() {
  el('quota-modal').classList.remove('hidden');
  void guard('加载配额', loadQuota);
}

function closeQuotaModal() {
  el('quota-modal').classList.add('hidden');
}

async function loadQuota() {
  const defaults = unwrap(await api.getQuotaDefaults(), '加载全局配额');
  el('default-soft').value = String(defaults.softBytes || '');
  el('default-hard').value = String(defaults.hardBytes || '');
  el('defaults-text').textContent =
    defaults.updatedAt > 0
      ? `上次修改：${formatTime(defaults.updatedAt)} by ${defaults.updatedBy || '-'}` +
        '。仅管理员可改；未单独核定的仓库继承这里的值。'
      : '尚未配置（0 表示未配置，此时由系统兜底值生效）。仅管理员可改。';

  if (!state.selectedRepoId) {
    el('repo-quota-text').textContent = '先在左侧选择一个仓库。';
    return;
  }
  const quota = unwrap(await api.getRepoQuota(state.selectedRepoId), '加载仓库配额');
  el('repo-soft').value = String(quota.repoSoftBytes || '');
  el('repo-hard').value = String(quota.repoHardBytes || '');
  el('repo-quota-text').textContent =
    `当前生效：软 ${humanBytes(quota.effectiveSoft)} / 硬 ${humanBytes(quota.effectiveHard)}` +
    `（来源：${QUOTA_SOURCE_LABEL[quota.source] || '未知'}）。` +
    '把两个值都留空（0）表示继承全局默认。';
}

async function saveDefaults() {
  const soft = Number(el('default-soft').value) || 0;
  const hard = Number(el('default-hard').value) || 0;
  const saved = unwrap(await api.updateQuotaDefaults(soft, hard), '保存全局配额');
  banner(`全局默认配额已更新：软 ${humanBytes(saved.softBytes)} / 硬 ${humanBytes(saved.hardBytes)}`);
  await loadQuota();
}

async function saveRepoQuota(inherit) {
  if (!state.selectedRepoId) {
    banner('先选择一个仓库', 'error');
    return;
  }
  const soft = inherit ? 0 : Number(el('repo-soft').value) || 0;
  const hard = inherit ? 0 : Number(el('repo-hard').value) || 0;
  const quota = unwrap(
    await api.setRepoQuota(state.selectedRepoId, soft, hard, inherit),
    inherit ? '恢复继承' : '保存仓库配额',
  );
  banner(
    `仓库配额已${inherit ? '恢复继承' : '更新'}：生效 软 ${humanBytes(quota.effectiveSoft)} / ` +
      `硬 ${humanBytes(quota.effectiveHard)}（来源：${QUOTA_SOURCE_LABEL[quota.source] || '未知'}）`,
  );
  await loadQuota();
  await loadSpace(state.selectedRepoId);
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

// ---------------- 空间与清理 ----------------

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

async function loadSpace(repoId) {
  const repo = state.repos.find((item) => item.repoId === repoId);
  el('space-repo').textContent = repo ? repo.name : '';
  const quota = unwrap(await api.getQuota(repoId), '加载容量');
  state.purgeCandidates = unwrap(
    await api.listPurgeCandidates(repoId, 10),
    '加载可清理目录',
  );
  state.purged = unwrap(await api.listPurgedPaths(repoId), '加载回收站');
  state.policies = unwrap(await api.listRetentionPolicies(repoId), '加载保留策略');
  renderQuota(quota);
  renderPurgeCandidates();
  renderPurged();
  renderPolicies();
  el('space-result').textContent = '';
}

function renderQuota(quota) {
  const ratio = quota.hardLimit > 0 ? quota.usedBytes / quota.hardLimit : 0;
  const fill = el('quota-fill');
  fill.style.width = `${Math.min(100, Math.round(ratio * 100))}%`;
  fill.className = ratio >= 0.95 ? 'danger' : ratio >= 0.85 ? 'warn' : '';
  el('quota-text').textContent =
    `已用 ${humanBytes(quota.usedBytes)} / 硬上限 ${humanBytes(quota.hardLimit)}` +
    `（软上限 ${humanBytes(quota.softLimit)}，来源：${quotaSourceLabel(quota.source)}）`;
}

function renderPurgeCandidates() {
  const list = el('purge-candidates');
  list.innerHTML = '';
  if (state.purgeCandidates.length === 0) {
    list.innerHTML = '<li class="empty">没有可清理的目录</li>';
    return;
  }
  for (const item of state.purgeCandidates) {
    const li = document.createElement('li');
    li.innerHTML =
      '<input type="checkbox" /><span class="who"></span><span class="size"></span>';
    li.querySelector('input').dataset.path = item.path;
    li.querySelector('.who').textContent = item.path;
    // 逻辑大小与独占大小并列展示：用户才能理解"删了这个目录为什么释放不了那么多"。
    // 两个数字口径不同：可释放量包含「只属于该目录的历史版本内容」，所以可能大于当前版本大小。
    li.querySelector('.size').textContent =
      `${humanBytes(item.uniqueBytes)} 可释放 · 当前 ${humanBytes(item.logicalBytes)}`;
    list.appendChild(li);
  }
}

const PURGED_STATE = {
  pending: { label: '宽限期（7 天后回收）', css: 'pending' },
  active: { label: '已生效（内容已回收）', css: 'active' },
  reverted: { label: '已撤销', css: '' },
};

function renderPurged() {
  const list = el('purged');
  list.innerHTML = '';
  if (state.purged.length === 0) {
    list.innerHTML = '<li class="empty">还没有执行过清理</li>';
    return;
  }
  for (const item of state.purged) {
    const meta = PURGED_STATE[item.state] || { label: item.state, css: '' };
    const li = document.createElement('li');
    li.innerHTML =
      '<div class="line"><span class="who"></span><span class="state"></span></div>';
    li.querySelector('.who').textContent = item.path;
    const stateNode = li.querySelector('.state');
    stateNode.textContent = meta.label;
    stateNode.className = `state ${meta.css}`;
    if (item.state !== 'reverted') {
      const undo = document.createElement('button');
      undo.className = 'ghost';
      undo.textContent = '撤销';
      undo.addEventListener('click', () => {
        void guard('撤销清理', () => undoPurge(item.purgeId, item.path));
      });
      li.querySelector('.line').appendChild(undo);
    }
    list.appendChild(li);
  }
}

/**
 * 撤销清理。先确认再执行：这会产生一次新提交，而且内容若已被 GC 回收就无法恢复。
 */
async function undoPurge(purgeId, path) {
  const confirmed = window.confirm(
    `撤销对 ${path} 的清理？\n\n会把该路径的内容恢复成一次新提交，` +
      '并让 GC 不再跳过这些路径。若内容已被物理回收，撤销会失败（不会伪造空内容）。',
  );
  if (!confirmed) return;
  const rev = unwrap(await api.revertPurge(state.selectedRepoId, purgeId), '撤销清理');
  banner(`已撤销对 ${path} 的清理，内容恢复到 r${rev}。`);
  await loadSpace(state.selectedRepoId);
  await loadRepos();
  await loadLog();
}

function renderPolicies() {
  const list = el('policies');
  list.innerHTML = '';
  if (state.policies.length === 0) {
    list.innerHTML = '<li class="empty">没有配置保留策略</li>';
    return;
  }
  for (const policy of state.policies) {
    const rules = [];
    if (policy.keepMonths > 0) rules.push(`保留 ${policy.keepMonths} 个`);
    if (policy.maxBytes > 0) rules.push(`上限 ${humanBytes(policy.maxBytes)}`);
    if (policy.maxAgeDays > 0) rules.push(`${policy.maxAgeDays} 天未改`);
    const stateLabel = !policy.enabled
      ? '已停用'
      : policy.dryRun
        ? '仅预览'
        : '已启用';
    const li = document.createElement('li');
    li.innerHTML =
      '<div class="line"><span class="who"></span><span class="state"></span></div>';
    li.querySelector('.who').textContent = policy.pathPattern;
    const badge = li.querySelector('.state');
    badge.textContent = stateLabel;
    badge.className = `state ${policy.enabled && !policy.dryRun ? 'active' : ''}`;
    const remove = document.createElement('button');
    remove.className = 'ghost';
    remove.textContent = '删除';
    remove.addEventListener('click', () => {
      void guard('删除策略', () => deletePolicy(policy.policyId));
    });
    li.querySelector('.line').appendChild(remove);
    const meta = document.createElement('div');
    meta.className = 'state';
    meta.textContent = rules.join('、') || '未设置任何上限';
    li.appendChild(meta);
    list.appendChild(li);
  }
}

function numberValue(id) {
  const raw = Number(el(id).value);
  return Number.isFinite(raw) && raw > 0 ? Math.floor(raw) : 0;
}

async function savePolicy() {
  const pattern = el('policy-pattern').value.trim();
  if (!pattern) {
    banner('请填写目录名正则，例如 ^20\\d{4}$', 'error');
    return;
  }
  const policy = unwrap(
    await api.upsertRetentionPolicy(state.selectedRepoId, {
      pathPattern: pattern,
      keepMonths: numberValue('policy-keep'),
      maxBytes: numberValue('policy-bytes'),
      maxAgeDays: numberValue('policy-age'),
      enabled: true,
      dryRun: !el('policy-execute').checked,
    }),
    '保存策略',
  );
  banner(
    `已保存策略 ${policy.policyId}（${policy.pathPattern}）` +
      (policy.dryRun ? '：仅预览，不会真的删除' : '：已启用，执行时会真的删除'),
  );
  await loadSpace(state.selectedRepoId);
}

async function deletePolicy(policyId) {
  unwrap(await api.deleteRetentionPolicy(state.selectedRepoId, policyId), '删除策略');
  banner(`已删除策略 ${policyId}`);
  await loadSpace(state.selectedRepoId);
}

async function runRetention(dryRun) {
  if (!dryRun) {
    const confirmed = window.confirm(
      '按保留策略执行清理？\n\n会生成删除提交，删除的内容进入回收站（可在上面撤销）。',
    );
    if (!confirmed) return;
  }
  const outcome = unwrap(
    await api.applyRetention(state.selectedRepoId, dryRun, false),
    dryRun ? '评估策略' : '执行策略',
  );
  if (outcome.candidates.length === 0) {
    banner('所有目录都满足保留策略，没有需要清理的内容。');
    return;
  }
  const head =
    `候选目录：${outcome.candidates.join('、')}；预计释放 ${humanBytes(outcome.reclaimableBytes)}。`;
  banner(
    dryRun
      ? `${head}这是预览，没有改动任何数据。`
      : `${head}已生成删除提交 r${outcome.newRev}，内容在回收站里可撤销。`,
  );
  await loadSpace(state.selectedRepoId);
  if (!dryRun) {
    await loadRepos();
    await loadLog();
  }
}

function selectedPaths() {
  const boxes = el('purge-candidates').querySelectorAll('input[type="checkbox"]');
  return Array.from(boxes)
    .filter((box) => box.checked)
    .map((box) => box.dataset.path);
}

async function estimateSelected() {
  const paths = selectedPaths();
  if (paths.length === 0) {
    banner('请先勾选要清理的目录', 'error');
    return;
  }
  const estimate = unwrap(
    await api.estimatePurge(state.selectedRepoId, paths),
    '预估清理',
  );
  const shared = estimate.sharedPaths.length
    ? `；被共享、不会被回收的部分来自 ${estimate.sharedPaths.slice(0, 3).join('、')}` +
      `${estimate.sharedPaths.length > 3 ? ' 等' : ''}`
    : '';
  el('space-result').textContent =
    `预估：清理 ${paths.join('、')} 可释放 ${humanBytes(estimate.reclaimableBytes)}，` +
    `影响 ${estimate.affectedRevisions} 个历史版本${shared}。这是 dry-run，没有改动任何数据。`;
}

async function purgeSelected() {
  const paths = selectedPaths();
  if (paths.length === 0) {
    banner('请先勾选要清理的目录', 'error');
    return;
  }
  const immediate = el('purge-immediate').checked;
  // 清理会改写 HEAD 并物理删除内容，必须让用户明确确认一次。
  const confirmed = window.confirm(
    `确认清理 ${paths.join('、')}？\n\n` +
      '会生成一个删除提交，并' +
      (immediate ? '立即' : '在 7 天宽限期后') +
      '回收仅被这些路径独占的内容。\n此操作不可撤销。',
  );
  if (!confirmed) return;

  const result = unwrap(
    await api.purge(state.selectedRepoId, paths, immediate, '桌面端清理'),
    '清理',
  );
  let text = `已生成删除提交 r${result.newRev}。`;
  if (immediate) {
    const gc = unwrap(
      await api.getGcStatus(state.selectedRepoId, result.gcJobId),
      '查询回收结果',
    );
    text +=
      gc.error
        ? ` GC 失败：${gc.error}`
        : ` GC 已清扫 ${gc.sweptObjects} 个对象，实际释放 ${humanBytes(gc.reclaimedBytes)}。`;
  } else {
    text += ' 已进入回收站，7 天后由 GC 释放（需要立刻腾空间请勾选「立即释放」）。';
  }
  banner(text);
  await loadSpace(state.selectedRepoId);
  await loadRepos();
  await loadLog();
}

// ---------------- 注册与激活 ----------------

function toggle(id) {
  el(id).classList.toggle('hidden');
}

async function doRegister() {
  const target = el('target').value.trim();
  const username = el('reg-username').value.trim();
  const displayName = el('reg-name').value.trim();
  const reason = el('reg-reason').value.trim();
  if (!target || !username || !displayName) {
    banner('请填写服务端地址、用户名与姓名', 'error');
    return;
  }
  const result = unwrap(
    await api.register(target, username, displayName, reason),
    '提交注册申请',
  );
  el('register-form').classList.add('hidden');
  banner(
    `注册申请已提交（${result.requestId}）。账号处于「待审批」，管理员通过后会给你一个\n` +
      '一次性链接，用它设置密码后才能登录。',
  );
}

async function doSetPassword() {
  const target = el('target').value.trim();
  const token = el('setpw-token').value.trim();
  const password = el('setpw-password').value;
  if (!target || !token || !password) {
    banner('请填写服务端地址、一次性 token 与新密码', 'error');
    return;
  }
  unwrap(await api.setPassword(target, token, password), '设置密码');
  el('setpw-form').classList.add('hidden');
  el('setpw-token').value = '';
  el('setpw-password').value = '';
  banner('密码已设置，可以登录了。同一条链接只能使用一次。');
}

// ---------------- 建库申请与审批台 ----------------

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

function formatTime(ts) {
  if (!ts) return '';
  return new Date(ts * 1000).toLocaleString();
}

async function loadAdmin() {
  const [registrations, repoRequests] = await Promise.all([
    api.listRegistrations(1),
    api.listRepoCreations(1),
  ]);
  state.registrations = unwrap(registrations, '加载注册申请');
  state.repoRequests = unwrap(repoRequests, '加载建库申请');
  renderRegistrations();
  renderRepoRequests();
}

function renderRegistrations() {
  const list = el('reg-list');
  list.innerHTML = '';
  if (state.registrations.length === 0) {
    list.innerHTML = '<li class="empty">没有待审批的注册申请</li>';
    return;
  }
  for (const item of state.registrations) {
    const li = document.createElement('li');
    li.innerHTML =
      '<div class="req"><span class="who"></span>' +
      '<select class="role">' +
      '<option value="2">可写</option><option value="1">只读</option><option value="3">管理员</option>' +
      '</select>' +
      '<button class="ok primary">通过</button><button class="no ghost">拒绝</button></div>' +
      '<div class="meta"></div>' +
      '<div class="req"><input class="reason" placeholder="拒绝原因（拒绝时必填）" /></div>';
    li.querySelector('.who').textContent = `${item.username}（${item.displayName}）`;
    li.querySelector('.meta').textContent =
      `${item.reason || '未填写理由'} · ${formatTime(item.submittedAt)}`;
    li.querySelector('.ok').addEventListener('click', () => {
      const role = Number(li.querySelector('.role').value);
      void guard('审批注册', () => approveRegistration(item.requestId, role));
    });
    li.querySelector('.no').addEventListener('click', () => {
      const reason = li.querySelector('.reason').value.trim();
      void guard('拒绝注册', () => rejectRegistration(item.requestId, reason));
    });
    list.appendChild(li);
  }
}

async function approveRegistration(requestId, role) {
  const approved = unwrap(await api.approveRegistration(requestId, role), '审批注册');
  banner(
    `已通过审批。请把下面这条一次性链接转交申请人（只显示这一次）：\n` +
      `vcs_setup token: ${approved.setupToken}\n` +
      `有效期至 ${formatTime(Math.floor(approved.expiresAt))}；对方在「用一次性链接设置密码」里粘贴即可。`,
  );
  await loadAdmin();
}

async function rejectRegistration(requestId, reason) {
  if (!reason) {
    banner('拒绝必须填写原因', 'error');
    return;
  }
  unwrap(await api.rejectRegistration(requestId, reason), '拒绝注册');
  banner('已拒绝该注册申请。');
  await loadAdmin();
}

function renderRepoRequests() {
  const list = el('repo-req-list');
  list.innerHTML = '';
  if (state.repoRequests.length === 0) {
    list.innerHTML = '<li class="empty">没有待审批的建库申请</li>';
    return;
  }
  for (const item of state.repoRequests) {
    const li = document.createElement('li');
    li.innerHTML =
      '<div class="req"><span class="who"></span>' +
      '<button class="ok primary">通过</button><button class="no ghost">拒绝</button></div>' +
      '<div class="meta"></div>' +
      '<div class="req"><input class="reason" placeholder="拒绝原因（拒绝时必填）" /></div>';
    li.querySelector('.who').textContent = item.repoName;
    li.querySelector('.meta').textContent =
      `${item.reason || '未填写用途'} · ${formatTime(item.submittedAt)}`;
    li.querySelector('.ok').addEventListener('click', () => {
      void guard('审批建库', () => approveRepoRequest(item.requestId));
    });
    li.querySelector('.no').addEventListener('click', () => {
      const reason = li.querySelector('.reason').value.trim();
      void guard('拒绝建库', () => rejectRepoRequest(item.requestId, reason));
    });
    list.appendChild(li);
  }
}

async function approveRepoRequest(requestId) {
  // 留空即继承全局默认配额：这里不核定，交由管理员后续在仓库设置里调整。
  const repo = unwrap(await api.approveRepoCreation(requestId, 0, 0, '同意'), '审批建库');
  banner(`仓库 ${repo.name} 已创建，申请人已成为 owner。`);
  await loadAdmin();
  await loadRepos();
}

async function rejectRepoRequest(requestId, reason) {
  if (!reason) {
    banner('拒绝必须填写原因', 'error');
    return;
  }
  unwrap(await api.rejectRepoCreation(requestId, reason), '拒绝建库');
  banner('已拒绝该建库申请。');
  await loadAdmin();
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
  state.registrations = [];
  state.repoRequests = [];
  el('who').textContent = '';
  el('disconnect').classList.add('hidden');
  el('admin-panel').classList.add('hidden');
  el('repos').innerHTML = '';
  el('changes').innerHTML = '';
  el('log').innerHTML = '';
  el('wc-path').textContent = '';
  banner('已断开连接');
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
el('search-users').addEventListener('click', () => void guard('搜索用户', searchUsers));
el('refresh-space').addEventListener('click', () =>
  void guard('刷新空间', () => loadSpace(state.selectedRepoId)),
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
el('open-quota').addEventListener('click', () => void guard('打开配额设置', async () => openQuotaModal()));
el('close-quota').addEventListener('click', closeQuotaModal);
el('save-defaults').addEventListener('click', () => void guard('保存全局配额', saveDefaults));
el('save-repo-quota').addEventListener('click', () => void guard('保存仓库配额', () => saveRepoQuota(false)));
el('inherit-repo-quota').addEventListener('click', () =>
  void guard('恢复继承', () => saveRepoQuota(true)),
);
el('save-policy').addEventListener('click', () => void guard('保存策略', savePolicy));
el('preview-retention').addEventListener('click', () =>
  void guard('评估策略', () => runRetention(true)),
);
el('apply-retention').addEventListener('click', () =>
  void guard('执行策略', () => runRetention(false)),
);
el('estimate').addEventListener('click', () => void guard('预估清理', estimateSelected));
el('purge').addEventListener('click', () => void guard('清理', purgeSelected));
el('toggle-register').addEventListener('click', () => toggle('register-form'));
el('toggle-setpw').addEventListener('click', () => toggle('setpw-form'));
el('do-register').addEventListener('click', () => void guard('提交注册申请', doRegister));
el('do-setpw').addEventListener('click', () => void guard('设置密码', doSetPassword));
el('do-request').addEventListener('click', () => void guard('提交建库申请', requestRepo));
el('refresh-admin').addEventListener('click', () => void guard('刷新审批台', loadAdmin));
el('disconnect').addEventListener('click', () => void guard('断开连接', disconnect));
el('create-repo').addEventListener('click', () => void guard('新建仓库', createRepo));
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
