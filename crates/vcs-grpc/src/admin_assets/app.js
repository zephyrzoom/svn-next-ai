/* svn-next 内置 Web 管理端：纯原生 JS，无外部依赖。 */
const TOKEN_KEY = 'vcs_admin_token';
let token = localStorage.getItem(TOKEN_KEY) || '';
let currentView = 'overview';
let allRepos = [];
let selectedRepo = '';

const $ = (id) => document.getElementById(id);
const esc = (value) => String(value == null ? '' : value).replace(/[&<>"']/g, (c) => ({
  '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
}[c]));

function toast(message, isError = false) {
  const box = $('toast');
  box.textContent = message;
  box.classList.toggle('error', isError);
  box.classList.remove('hidden');
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => box.classList.add('hidden'), 3600);
}

async function act(fn) {
  try {
    return await fn();
  } catch (err) {
    toast(err.message || String(err), true);
    return undefined;
  }
}

async function request(path, options = {}) {
  const headers = { ...(options.headers || {}) };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (options.body) headers['Content-Type'] = 'application/json';
  const res = await fetch(`/api${path}`, { ...options, headers });
  let body;
  try {
    body = await res.json();
  } catch {
    throw new Error(`服务端返回异常（HTTP ${res.status}）`);
  }
  if (res.status === 401 || res.status === 403) {
    if (path !== '/login') signOut();
    throw new Error(body.message || '登录已失效，请重新登录');
  }
  if (!body.ok) throw new Error(body.message || '操作失败');
  return body.data;
}

const get = (path) => request(path);
const post = (path, data) => request(path, { method: 'POST', body: JSON.stringify(data || {}) });
const put = (path, data) => request(path, { method: 'PUT', body: JSON.stringify(data || {}) });
const del = (path) => request(path, { method: 'DELETE' });

function humanBytes(n) {
  const value = Number(n || 0);
  if (value < 1024) return `${value} B`;
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let size = value / 1024;
  let i = 0;
  while (size >= 1024 && i < units.length - 1) { size /= 1024; i += 1; }
  return `${size.toFixed(1)} ${units[i]}`;
}

function fmtTime(seconds) {
  if (!seconds) return '—';
  return new Date(Number(seconds) * 1000).toLocaleString('zh-CN', { hour12: false });
}

const STATE_LABEL = {
  Pending: '待处理', Approved: '已通过', Rejected: '已拒绝', Expired: '已过期',
  Active: '正常', Disabled: '停用', Archived: '归档',
  pending: '待审批', approved: '已通过', rejected: '已拒绝', expired: '已过期',
  active: '正常', disabled: '停用', archived: '归档',
};

function stateBadge(state) {
  const label = STATE_LABEL[state] || state || '—';
  const cls = state === 'Pending' ? 'warn'
    : (state === 'Rejected' || state === 'Expired' || state === 'Disabled') ? 'bad'
      : (state === 'Approved' || state === 'Active') ? 'ok' : '';
  return `<span class="badge ${cls}">${esc(label)}</span>`;
}

const ROLE_LABEL = { Admin: '管理员', Writer: '写', Readonly: '只读', admin: '管理员', writer: '写', readonly: '只读' };
const MEMBER_LABEL = { Owner: 'owner', Maintainer: 'maintainer', Writer: 'writer', Reader: 'reader' };

function empty(text) {
  return `<div class="empty">${esc(text)}</div>`;
}

/* ---------------- 登录与导航 ---------------- */

function signOut() {
  token = '';
  localStorage.removeItem(TOKEN_KEY);
  $('app').classList.add('hidden');
  $('login').classList.remove('hidden');
}

function showApp(user) {
  $('login').classList.add('hidden');
  $('app').classList.remove('hidden');
  $('who').textContent = `${user.username}（管理员）`;
  setView('overview');
}

function setView(view) {
  currentView = view;
  document.querySelectorAll('.view').forEach((node) => node.classList.add('hidden'));
  $(`view-${view}`).classList.remove('hidden');
  document.querySelectorAll('.nav-item').forEach((node) => {
    node.classList.toggle('active', node.dataset.view === view);
  });
  const loaders = {
    overview: loadOverview, approvals: loadApprovals, users: loadUsers,
    repos: loadRepos, quota: loadQuota, ldap: loadLdap, space: loadSpace,
  };
  if (loaders[view]) void act(loaders[view]);
}

$('login-form').addEventListener('submit', (event) => {
  event.preventDefault();
  void (async () => {
    try {
      const data = await post('/login', {
        username: $('login-username').value.trim(),
        password: $('login-password').value,
        source: $('login-source').value,
      });
      token = data.token;
      localStorage.setItem(TOKEN_KEY, token);
      $('login-error').classList.add('hidden');
      $('login-password').value = '';
      showApp(data.user);
    } catch (err) {
      $('login-error').textContent = err.message || '登录失败，请检查账号与密码';
      $('login-error').classList.remove('hidden');
    }
  })();
});

$('logout').addEventListener('click', () => {
  signOut();
  toast('已退出登录');
});

$('nav').addEventListener('click', (event) => {
  const item = event.target.closest('.nav-item');
  if (item) setView(item.dataset.view);
});

/* ---------------- 概览 ---------------- */

async function loadOverview() {
  const data = await get('/overview');
  const cards = [
    ['账号', data.users, `其中 ${data.active_users} 个可用`],
    ['仓库', data.repos, `其中 ${data.active_repos} 个启用`],
    ['待审批注册', data.pending_registrations, '需要管理员处理', data.pending_registrations > 0],
    ['待审批建库', data.pending_repo_requests, '需要管理员处理', data.pending_repo_requests > 0],
    ['待审批扩容', data.pending_quota_requests, '每次 +10 GiB', data.pending_quota_requests > 0],
    ['CAS 对象', data.objects, humanBytes(data.object_bytes)],
    ['待转正清理', data.purged_pending, '宽限期内的回收站记录'],
  ];
  $('stats').innerHTML = cards.map(([name, num, note, alert]) => `
    <div class="stat ${alert ? 'alert' : ''}">
      <div class="num">${esc(num)}</div>
      <div class="name">${esc(name)} · ${esc(note)}</div>
    </div>`).join('');
  const todo = [];
  if (data.pending_registrations) todo.push(`${data.pending_registrations} 条注册申请待审批`);
  if (data.pending_repo_requests) todo.push(`${data.pending_repo_requests} 条建库申请待审批`);
  if (data.pending_quota_requests) todo.push(`${data.pending_quota_requests} 条扩容申请待审批`);
  $('todo').innerHTML = todo.length
    ? todo.map((text) => `<span class="pill">${esc(text)}</span>`).join('')
    : empty('当前没有待办事项');
}

$('refresh-overview').addEventListener('click', () => void act(loadOverview));

/* ---------------- 审批台 ---------------- */

async function loadApprovals() {
  const [regs, reqs, expansions] = await Promise.all([
    get('/registrations?state=pending'),
    get('/repo-requests?state=pending'),
    get('/repo-expansions?state=pending'),
  ]);
  $('reg-list').innerHTML = regs.items.length ? regs.items.map((r) => `
    <div class="list-item" data-id="${esc(r.request_id)}">
      <div>
        <div class="title">${esc(r.username)} <span class="muted">${esc(r.display_name)}</span> ${stateBadge(r.state)}</div>
        <div class="meta">申请理由：${esc(r.reason || '—')} · ${esc(fmtTime(r.submitted_at))}</div>
      </div>
      <div class="actions">
        <select data-field="role">
          <option value="readonly">只读</option><option value="writer">写</option><option value="admin">管理员</option>
        </select>
        <button class="mini primary" data-action="approve-reg">通过</button>
        <button class="mini danger" data-action="reject-reg">拒绝</button>
      </div>
    </div>`).join('') : empty('没有待审批的注册申请');

  $('req-list').innerHTML = reqs.items.length ? reqs.items.map((r) => `
    <div class="list-item" data-id="${esc(r.request_id)}">
      <div>
        <div class="title">${esc(r.repo_name)} ${stateBadge(r.state)}</div>
        <div class="meta">申请人 ${esc(r.requester_user_id)} · 默认容量 ${esc(humanBytes(r.requested_quota_bytes))} · ${esc(fmtTime(r.submitted_at))}<br />理由：${esc(r.reason || '—')}</div>
      </div>
      <div class="actions">
        <input data-field="note" placeholder="审批意见" />
        <button class="mini primary" data-action="approve-req">通过</button>
        <button class="mini danger" data-action="reject-req">拒绝</button>
      </div>
    </div>`).join('') : empty('没有待审批的建库申请');

  const disk = expansions.storage || {};
  $('storage-note').textContent = disk.total_bytes
    ? `磁盘可用 ${humanBytes(disk.available_bytes)} / 共 ${humanBytes(disk.total_bytes)}`
    : '';
  $('expansion-list').innerHTML = expansions.items.length ? expansions.items.map((r) => `
    <div class="list-item" data-id="${esc(r.request_id)}">
      <div>
        <div class="title">${esc(r.repo_name || r.repo_id)} ${stateBadge(r.state)}</div>
        <div class="meta">申请人 ${esc(r.requester_user_id)} · 上限 ${esc(humanBytes(r.quota_before_bytes))} → ${esc(humanBytes(r.target_quota_bytes))}（+${esc(humanBytes(r.increment_bytes))}） · ${esc(fmtTime(r.submitted_at))}<br />说明：${esc(r.reason || '—')}</div>
      </div>
      <div class="actions">
        <input data-field="note" placeholder="审批意见" />
        <button class="mini primary" data-action="approve-expansion">通过</button>
        <button class="mini danger" data-action="reject-expansion">拒绝</button>
      </div>
    </div>`).join('') : empty('没有待审批的扩容申请');
}

function field(row, name) {
  const node = row.querySelector(`[data-field="${name}"]`);
  return node ? node.value.trim() : '';
}

$('reg-list').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const row = button.closest('.list-item');
  const id = row.dataset.id;
  void act(async () => {
    if (button.dataset.action === 'approve-reg') {
      await post(`/registrations/${encodeURIComponent(id)}/approve`, { role: field(row, 'role') });
      toast('已通过，申请人可直接用注册时设置的密码登录');
    } else {
      const reason = window.prompt('拒绝原因（必填）', '');
      if (!reason) return;
      await post(`/registrations/${encodeURIComponent(id)}/reject`, { reason });
      toast('已拒绝该申请');
    }
    await loadApprovals();
  });
});

$('req-list').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const row = button.closest('.list-item');
  const id = row.dataset.id;
  void act(async () => {
    if (button.dataset.action === 'approve-req') {
      await post(`/repo-requests/${encodeURIComponent(id)}/approve`, {
        note: field(row, 'note'),
      });
      toast('建库申请已通过，仓库已按 10 GiB 初始容量创建');
    } else {
      const reason = window.prompt('拒绝原因（必填）', '');
      if (!reason) return;
      await post(`/repo-requests/${encodeURIComponent(id)}/reject`, { reason });
      toast('已拒绝该申请');
    }
    await loadApprovals();
  });
});

$('expansion-list').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const row = button.closest('.list-item');
  const id = row.dataset.id;
  void act(async () => {
    if (button.dataset.action === 'approve-expansion') {
      await post(`/repo-expansions/${encodeURIComponent(id)}/approve`, {
        note: field(row, 'note'),
      });
      toast('扩容申请已通过，仓库上限已增加 10 GiB');
    } else {
      const reason = window.prompt('拒绝原因（必填）', '');
      if (!reason) return;
      await post(`/repo-expansions/${encodeURIComponent(id)}/reject`, { reason });
      toast('已拒绝该扩容申请');
    }
    await loadApprovals();
  });
});

$('refresh-approvals').addEventListener('click', () => void act(loadApprovals));

/* ---------------- 用户 ---------------- */

async function loadUsers() {
  const data = await get('/users');
  const tbody = $('users-table').querySelector('tbody');
  tbody.innerHTML = data.items.length ? data.items.map((u) => `
    <tr data-id="${esc(u.user_id)}">
      <td>${esc(u.username)}</td><td>${esc(u.display_name)}</td>
      <td>${esc(u.source === 'ldap' ? 'LDAP' : '本地')}</td>
      <td>${esc(STATE_LABEL[u.status] || u.status)}</td>
      <td>${esc(ROLE_LABEL[u.role] || u.role)}</td>
      <td class="muted">${esc(fmtTime(u.last_login_at))}</td>
      <td><div class="actions">
        <select data-field="role">
          <option value="readonly" ${u.role === 'readonly' ? 'selected' : ''}>只读</option>
          <option value="writer" ${u.role === 'writer' ? 'selected' : ''}>写</option>
          <option value="admin" ${u.role === 'admin' ? 'selected' : ''}>管理员</option>
        </select>
        <button class="mini" data-action="role">改角色</button>
        <button class="mini ${u.status === 'active' ? 'danger' : ''}" data-action="status">
          ${u.status === 'active' ? '停用' : '启用'}
        </button>
      </div></td>
    </tr>`).join('') : '<tr><td colspan="7">' + empty('还没有账号') + '</td></tr>';
}

$('users-table').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const row = button.closest('tr');
  const id = row.dataset.id;
  void act(async () => {
    if (button.dataset.action === 'role') {
      await post(`/users/${encodeURIComponent(id)}/role`, { role: field(row, 'role') });
      toast('角色已更新，该用户的登录令牌已失效');
    } else {
      const active = button.textContent.trim() === '启用';
      await post(`/users/${encodeURIComponent(id)}/status`, { active });
      toast(active ? '账号已启用' : '账号已停用');
    }
    await loadUsers();
  });
});

$('create-user-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  void act(async () => {
    await post('/users', {
      username: form.username.value.trim(),
      display_name: form.display_name.value.trim(),
      password: form.password.value,
      role: form.role.value,
    });
    form.reset();
    toast('账号已创建');
    await loadUsers();
  });
});

$('refresh-users').addEventListener('click', () => void act(loadUsers));

/* ---------------- 仓库 ---------------- */

async function loadRepos() {
  const data = await get('/repos');
  allRepos = data.items;
  const tbody = $('repos-table').querySelector('tbody');
  tbody.innerHTML = data.items.length ? data.items.map((r) => `
    <tr data-id="${esc(r.repo_id)}" data-name="${esc(r.name)}">
      <td>${esc(r.name)}</td><td>r${esc(r.head_rev)}</td>
      <td>${stateBadge(r.status)}</td><td class="muted">${esc(r.repo_id)}</td>
      <td><button class="mini" data-action="detail">管理</button></td>
    </tr>`).join('') : '<tr><td colspan="5">' + empty('还没有仓库') + '</td></tr>';
  const select = $('space-repo');
  const keep = selectedRepo || (allRepos[0] && allRepos[0].repo_id) || '';
  select.innerHTML = allRepos.map((r) => `<option value="${esc(r.repo_id)}">${esc(r.name)}</option>`).join('');
  if (allRepos.some((r) => r.repo_id === keep)) select.value = keep;
  selectedRepo = select.value;
}

$('repos-table').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action="detail"]');
  if (!button) return;
  const row = button.closest('tr');
  openRepoDetail(row.dataset.id, row.dataset.name);
});

function openRepoDetail(repoId, name) {
  selectedRepo = repoId;
  $('repo-detail').classList.remove('hidden');
  $('repo-detail-title').textContent = `仓库详情 · ${name}`;
  $('repo-detail').dataset.repo = repoId;
  void act(loadRepoDetail);
}

$('close-repo-detail').addEventListener('click', () => $('repo-detail').classList.add('hidden'));

async function loadRepoDetail() {
  const id = $('repo-detail').dataset.repo;
  if (!id) return;
  const [members, quota] = await Promise.all([
    get(`/repos/${encodeURIComponent(id)}/members`),
    get(`/repos/${encodeURIComponent(id)}/quota`),
  ]);
  $('repo-members').innerHTML = members.items.length ? members.items.map((m) => `
    <div class="list-item" data-user="${esc(m.username)}">
      <div><span class="title">${esc(m.username)}</span> <span class="muted">${esc(m.display_name)}</span>
        <span class="badge">${esc(MEMBER_LABEL[m.role] || m.role)}</span></div>
      <div class="actions">
        <select data-field="role">
          ${['reader', 'writer', 'maintainer'].map((r) => `<option value="${r}">${r}</option>`).join('')}
        </select>
        <button class="mini" data-action="member-role">改角色</button>
        <button class="mini danger" data-action="member-remove">移除</button>
      </div>
    </div>`).join('') : empty('还没有成员');
  $('repo-usage').textContent = `已用 ${humanBytes(quota.usage.used_bytes)} / 上限 ${humanBytes(quota.effective.bytes)}（${esc(quota.effective.source)}）`;
  const form = $('repo-quota-form');
  form.quota_bytes.value = quota.quota_bytes;
}

$('repo-members').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const row = button.closest('.list-item');
  const id = $('repo-detail').dataset.repo;
  const username = row.dataset.user;
  void act(async () => {
    if (button.dataset.action === 'member-role') {
      await put(`/repos/${encodeURIComponent(id)}/members/${encodeURIComponent(username)}`, { role: field(row, 'role') });
      toast('成员角色已更新');
    } else {
      await del(`/repos/${encodeURIComponent(id)}/members/${encodeURIComponent(username)}`);
      toast('成员已移除');
    }
    await loadRepoDetail();
  });
});

$('add-member-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  const id = $('repo-detail').dataset.repo;
  void act(async () => {
    await post(`/repos/${encodeURIComponent(id)}/members`, {
      username: form.username.value.trim(), role: form.role.value,
    });
    form.reset();
    toast('成员已添加');
    await loadRepoDetail();
  });
});

$('repo-quota-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  const id = $('repo-detail').dataset.repo;
  void act(async () => {
    await put(`/repos/${encodeURIComponent(id)}/quota`, {
      quota_bytes: Number(form.quota_bytes.value) || 0,
      inherit: false,
    });
    toast('仓库配额已保存');
    await loadRepoDetail();
  });
});

$('inherit-quota').addEventListener('click', () => {
  const id = $('repo-detail').dataset.repo;
  void act(async () => {
    await put(`/repos/${encodeURIComponent(id)}/quota`, { quota_bytes: 0, inherit: true });
    toast('已恢复继承全局默认');
    await loadRepoDetail();
  });
});

$('save-repo-status').addEventListener('click', () => {
  const id = $('repo-detail').dataset.repo;
  void act(async () => {
    await post(`/repos/${encodeURIComponent(id)}/status`, { status: $('repo-status-select').value });
    toast('仓库状态已更新');
    await Promise.all([loadRepos(), loadRepoDetail()]);
  });
});

$('create-repo-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  void act(async () => {
    await post('/repos', {
      name: form.name.value.trim(),
      quota_bytes: Number(form.quota_bytes.value) || 0,
    });
    form.reset();
    toast('仓库已创建');
    await loadRepos();
  });
});

$('refresh-repos').addEventListener('click', () => void act(loadRepos));

/* ---------------- 全局配额 ---------------- */

async function loadQuota() {
  const data = await get('/quota-defaults');
  const form = $('defaults-form');
  form.quota_bytes.value = data.bytes;
  $('defaults-note').textContent = `当前默认：${humanBytes(data.bytes)}（0 表示未配置，按内置兜底 100 GiB）`;
}

$('defaults-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  void act(async () => {
    await put('/quota-defaults', {
      quota_bytes: Number(form.quota_bytes.value) || 0,
    });
    toast('全局默认配额已保存');
    await loadQuota();
  });
});

$('refresh-quota').addEventListener('click', () => void act(loadQuota));

/* ---------------- LDAP ---------------- */

function ldapFormData() {
  const form = $('ldap-form');
  return {
    host: form.host.value.trim(), port: Number(form.port.value) || 636,
    tls_mode: form.tls_mode.value, enabled: form.enabled.value === '1',
    base_dn: form.base_dn.value.trim(), bind_dn: form.bind_dn.value.trim(),
    bind_password: form.bind_password.value,
    user_filter: form.user_filter.value.trim(), group_filter: form.group_filter.value.trim(),
    attr_username: form.attr_username.value.trim(),
    attr_display_name: form.attr_display_name.value.trim(),
    group_roles: form.group_roles.value,
  };
}

async function loadLdap() {
  const cfg = await get('/ldap');
  const form = $('ldap-form');
  ['host', 'port', 'tls_mode', 'base_dn', 'bind_dn', 'user_filter', 'group_filter',
    'attr_username', 'attr_display_name', 'group_roles'].forEach((key) => {
    form[key].value = cfg[key] == null ? '' : cfg[key];
  });
  form.enabled.value = cfg.enabled ? '1' : '0';
  form.bind_password.value = '';
  $('ldap-state').textContent = cfg.bind_password_set ? '服务账号密码已保存（留空表示不修改）' : '尚未设置服务账号密码';
}

$('ldap-form').addEventListener('submit', (event) => {
  event.preventDefault();
  void act(async () => {
    await put('/ldap', ldapFormData());
    toast('目录配置已保存');
    await loadLdap();
  });
});

$('test-ldap').addEventListener('click', () => {
  void act(async () => {
    const data = await post('/ldap/test', ldapFormData());
    toast(`连接成功：${data.bound_dn}（抽样 ${data.user_count_sample} 个用户）`);
  });
});

$('refresh-ldap').addEventListener('click', () => void act(loadLdap));

/* ---------------- 空间治理 ---------------- */

function selectedPaths() {
  return Array.from($('candidate-list').querySelectorAll('input[type="checkbox"]:checked'))
    .map((node) => node.value);
}

async function loadSpace() {
  if (!allRepos.length) await loadRepos();
  const select = $('space-repo');
  if (!select.value) {
    $('candidate-list').innerHTML = empty('请先创建仓库');
    return;
  }
  selectedRepo = select.value;
  const id = encodeURIComponent(selectedRepo);
  const [candidates, purged, policies] = await Promise.all([
    get(`/repos/${id}/candidates`), get(`/repos/${id}/purged`), get(`/repos/${id}/policies`),
  ]);
  $('candidate-list').innerHTML = candidates.items.length ? candidates.items.map((c) => `
    <div class="list-item">
      <label class="check"><input type="checkbox" value="${esc(c.path)}" /> ${esc(c.path)}</label>
      <span class="meta">逻辑 ${esc(humanBytes(c.logical_bytes))} · 独占 ${esc(humanBytes(c.unique_bytes))} · r${esc(c.first_seen_rev)}~r${esc(c.last_modified_rev)}</span>
    </div>`).join('') : empty('没有可清理的顶层目录');
  $('purged-list').innerHTML = purged.items.length ? purged.items.map((p) => `
    <div class="list-item" data-id="${esc(p.purge_id)}">
      <div><span class="title">${esc(p.path)}</span> <span class="badge ${p.state === 'pending' ? 'warn' : ''}">${esc(p.state)}</span>
        <div class="meta">${esc(p.reason || '—')} · ${esc(fmtTime(p.created_at))}</div></div>
      <div class="actions"><button class="mini" data-action="revert">撤销</button></div>
    </div>`).join('') : empty('回收站为空');
  $('policy-list').innerHTML = policies.items.length ? policies.items.map((p) => `
    <div class="list-item" data-id="${esc(p.policy_id)}">
      <div><span class="title">${esc(p.path_pattern)}</span>
        <span class="badge ${p.dry_run ? 'warn' : 'ok'}">${p.dry_run ? '试运行' : '可执行'}</span>
        <div class="meta">保留 ${esc(p.keep_months)} 月 / ${esc(humanBytes(p.max_bytes))} / ${esc(p.max_age_days)} 天 · ${p.enabled ? '启用' : '停用'}</div></div>
      <div class="actions">
        <button class="mini" data-action="edit-policy">编辑</button>
        <button class="mini danger" data-action="delete-policy">删除</button>
      </div>
    </div>`).join('') : empty('没有保留策略');
  $('space-result').textContent = '';
}

$('space-repo').addEventListener('change', () => { selectedRepo = $('space-repo').value; });

$('estimate-purge').addEventListener('click', () => {
  void act(async () => {
    const paths = selectedPaths();
    if (!paths.length) throw new Error('请先勾选要清理的目录');
    const data = await post(`/repos/${encodeURIComponent(selectedRepo)}/estimate`, { paths });
    $('space-result').textContent = `预估可释放 ${humanBytes(data.reclaimable_bytes)}（共享 ${humanBytes(data.shared_bytes)}，涉及 ${data.affected_revisions} 个版本）`;
  });
});

$('purge-selected').addEventListener('click', () => {
  void act(async () => {
    const paths = selectedPaths();
    if (!paths.length) throw new Error('请先勾选要清理的目录');
    if (!window.confirm(`确认清理 ${paths.length} 个目录？这会生成删除提交。`)) return;
    const data = await post(`/repos/${encodeURIComponent(selectedRepo)}/purge`, {
      paths, immediate: $('purge-immediate').checked, reason: '管理端清理',
    });
    $('space-result').textContent = `已生成 r${data.rev}，预计释放 ${humanBytes(data.estimate.reclaimable_bytes)}，GC 任务 ${data.gc_job_id}`;
    await loadSpace();
  });
});

$('run-gc').addEventListener('click', () => {
  void act(async () => {
    const data = await post(`/repos/${encodeURIComponent(selectedRepo)}/gc`, {});
    $('space-result').textContent = `GC ${data.state}：回收 ${humanBytes(data.reclaimed_bytes)}，清理对象 ${data.swept_objects}`;
    await loadSpace();
  });
});

$('purged-list').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action="revert"]');
  if (!button) return;
  const id = button.closest('.list-item').dataset.id;
  void act(async () => {
    const data = await post(`/repos/${encodeURIComponent(selectedRepo)}/purged/${encodeURIComponent(id)}/revert`, {});
    toast(`已撤销，生成 r${data.rev}`);
    await loadSpace();
  });
});

$('policy-list').addEventListener('click', (event) => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const id = button.closest('.list-item').dataset.id;
  void act(async () => {
    if (button.dataset.action === 'delete-policy') {
      await del(`/repos/${encodeURIComponent(selectedRepo)}/policies/${encodeURIComponent(id)}`);
      toast('策略已删除');
      await loadSpace();
    } else {
      const policies = await get(`/repos/${encodeURIComponent(selectedRepo)}/policies`);
      const policy = policies.items.find((p) => p.policy_id === id);
      if (!policy) return;
      const form = $('policy-form');
      form.path_pattern.value = policy.path_pattern;
      form.keep_months.value = policy.keep_months;
      form.max_bytes.value = policy.max_bytes;
      form.max_age_days.value = policy.max_age_days;
      form.enabled.value = policy.enabled ? '1' : '0';
      form.dry_run.value = policy.dry_run ? '1' : '0';
      form.dataset.policy = policy.policy_id;
      toast('已载入表单，修改后点「保存策略」');
    }
  });
});

$('policy-form').addEventListener('submit', (event) => {
  event.preventDefault();
  const form = event.target;
  void act(async () => {
    await post(`/repos/${encodeURIComponent(selectedRepo)}/policies`, {
      policy_id: form.dataset.policy || '',
      path_pattern: form.path_pattern.value.trim(),
      keep_months: Number(form.keep_months.value) || 0,
      max_bytes: Number(form.max_bytes.value) || 0,
      max_age_days: Number(form.max_age_days.value) || 0,
      enabled: form.enabled.value === '1',
      dry_run: form.dry_run.value === '1',
    });
    delete form.dataset.policy;
    form.reset();
    toast('策略已保存');
    await loadSpace();
  });
});

async function retention(dryRun) {
  const data = await post(`/repos/${encodeURIComponent(selectedRepo)}/retention`, {
    dry_run: dryRun, immediate: true,
  });
  $('space-result').textContent = `${dryRun ? '预览' : '执行'}：候选 ${data.candidates.length} 个，可释放 ${humanBytes(data.reclaimable_bytes)}，新版本 ${data.new_rev || '无'}`;
  if (!dryRun) await loadSpace();
}

$('preview-retention').addEventListener('click', () => void act(() => retention(true)));
$('apply-retention').addEventListener('click', () => void act(() => retention(false)));
$('refresh-space').addEventListener('click', () => void act(loadSpace));

/* ---------------- 启动 ---------------- */

async function bootstrap() {
  if (!token) return;
  try {
    const user = await get('/session');
    showApp(user);
  } catch {
    signOut();
  }
}

void bootstrap();
