'use strict';

const $ = id => document.getElementById(id);
const API = '/api/private/v1';
const state = { key: '', session: 0, view: 'telemetry', projects: [], profiles: [], accounts: [], loginAvailable: false, managementEnabled: false, editingAccount: null, login: null, loginTimer: null, tokens: [], telemetryKeys: [], requests: [], cursor: null, query: '', telemetryLoad: 0, tokenLoad: 0, scopeLoad: 0, editingProject: null, editingToken: null, tokenFormProject: '', secretID: '', confirmation: null, requestID: '', events: [], eventCursor: null };
const number = value => Number(value || 0).toLocaleString();
const time = value => value ? new Date(value).toLocaleString([], { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : '—';
const text = (id, value) => { $(id).textContent = value; };
const element = (tag, value, className) => { const node = document.createElement(tag); if (value !== undefined) node.textContent = value; if (className) node.className = className; return node; };
const badge = (value, tone) => element('span', value, `badge ${tone || ''}`);
const cell = (...nodes) => { const td = element('td'); for (const node of nodes) td.append(typeof node === 'string' ? document.createTextNode(node) : node); return td; };
const lines = (main, sub, mono = false) => { const div = element('div', main, mono ? 'mono' : ''); if (sub) div.append(element('span', sub, 'subline')); return div; };
const emptyRow = (target, columns, message) => { const row = element('tr', undefined, 'empty-row'); const td = cell(message); td.colSpan = columns; row.append(td); $(target).replaceChildren(row); };
const button = (label, callback, className = 'action') => { const node = element('button', label, className); node.type = 'button'; node.addEventListener('click', () => task(node, callback)); return node; };

function notice(message, error = false) {
  text('notice', message); $('notice').classList.toggle('error', error); $('notice').hidden = !message;
}

async function task(control, callback, errorID) {
  if (control) control.disabled = true;
  if (errorID) text(errorID, '');
  try { await callback(); }
  catch (error) {
    if (!state.key) text('connect-error', error.message);
    else if (errorID) text(errorID, error.message);
    else notice(error.message, true);
  }
  finally { if (control) control.disabled = false; }
}

async function api(path, method = 'GET', body) {
  const session = state.session;
  const headers = { Authorization: `Bearer ${state.key}`, Accept: 'application/json' };
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  let response;
  try { response = await fetch(API + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), credentials: 'omit', cache: 'no-store' }); }
  catch (_) { throw new Error('Could not reach the relay. Check the connection and try again.'); }
  if (session !== state.session) throw new Error('Connection closed.');
  if (response.status === 401) { disconnect(); throw new Error('The admin key was rejected. Enter a valid key to reconnect.'); }
  const value = response.status === 204 ? null : await response.json();
  if (session !== state.session) throw new Error('Connection closed.');
  if (!response.ok) throw new Error(value?.error?.message || `Request failed (${response.status}).`);
  return value;
}

async function collection(path) {
  const items = [];
  let after;
  const seen = new Set();
  do {
    const page = await api(`${path}?limit=500${after ? `&after_id=${encodeURIComponent(after)}` : ''}`);
    items.push(...page.items);
    after = page.next_after_id;
    if (after && seen.has(after)) throw new Error('The relay returned a repeated pagination cursor.');
    seen.add(after);
  } while (after);
  return items;
}

function disconnect() {
  clearLogin(true);
  state.key = ''; state.session++; state.telemetryLoad++; state.tokenLoad++; state.scopeLoad++;
  state.projects = []; state.profiles = []; state.accounts = []; state.editingAccount = null; state.tokens = []; state.telemetryKeys = []; state.requests = []; state.events = [];
  state.cursor = null; state.confirmation = null; state.editingProject = null; state.editingToken = null;
  state.query = '';
  state.requestID = ''; state.eventCursor = null; state.tokenFormProject = '';
  for (const dialog of document.querySelectorAll('dialog')) dialog.close();
  for (const form of [$('project-form'), $('token-form'), $('account-form')]) form.reset();
  for (const id of ['request-rows', 'project-rows', 'token-rows', 'profile-cards', 'account-rows', 'model-bars', 'request-health']) $(id).replaceChildren();
  for (const id of ['request-detail', 'request-events', 'secret-details', 'secret-status']) text(id, '');
  $('issued-secret').value = ''; state.secretID = ''; $('admin-key').value = '';
  $('filters').reset();
  selectOptions('telemetry-scope', [], 'All projects and keys');
  $('telemetry-scope').title = '';
  for (const id of ['token-project', 'project-profile']) $(id).replaceChildren();
  for (const id of ['stat-requests', 'stat-input', 'stat-output', 'stat-cache']) text(id, '—');
  for (const id of ['stat-request-note', 'stat-cache-note', 'request-scope', 'pagination-note', 'projects-note', 'tokens-note', 'token-project-note', 'updated-at', 'profile-count']) text(id, '');
  text('project-count', '0'); text('health-text', 'Disconnected');
  $('app').hidden = true; $('connect-screen').hidden = false; notice(''); $('admin-key').focus();
}

function selectOptions(id, items, first) {
  const select = $(id); const previous = select.value;
  const options = [];
  if (first) { const option = element('option', first); option.value = ''; options.push(option); }
  for (const item of items) { const option = element('option', item.label); option.value = item.value; options.push(option); }
  select.replaceChildren(...options);
  if (options.some(option => option.value === previous)) select.value = previous;
}

const scopeValue = (project, key) => JSON.stringify(key ? [project, key] : [project]);

function renderTelemetryScopes() {
  const select = $('telemetry-scope'); const previous = select.value;
  const previousLabel = select.selectedOptions[0]?.textContent;
  const all = element('option', 'All projects and keys'); all.value = '';
  const groups = state.projects.map(project => {
    const group = element('optgroup'); group.label = `${project.display_name} · ${project.project_id}${project.enabled ? '' : ' · disabled'}`;
    const wholeProject = element('option', `${project.display_name} · all keys`); wholeProject.value = scopeValue(project.project_id); group.append(wholeProject);
    const keys = state.telemetryKeys.filter(key => key.project_id === project.project_id);
    keys.sort((a, b) => `${a.token_type}:${a.name || a.client_id}:${a.key_id}`.localeCompare(`${b.token_type}:${b.name || b.client_id}:${b.key_id}`));
    for (const key of keys) {
      const kind = { maas: 'MaaS key', anthropic: 'Anthropic key', vertex: 'Vertex workload key', legacy: 'Legacy combined key' }[key.token_type];
      const expired = key.expires_at_ms != null && key.expires_at_ms <= Date.now();
      const option = element('option', `${kind} · ${key.name || key.client_id} · ${key.key_id}${expired ? ' · expired' : ''}`);
      option.value = scopeValue(project.project_id, key.key_id); group.append(option);
    }
    return group;
  });
  select.replaceChildren(all, ...groups);
  if (previous && ![...select.options].some(option => option.value === previous)) {
    // Revocation/deletion must not silently broaden an active telemetry filter.
    const group = element('optgroup'); group.label = 'Previously selected · historical telemetry';
    const option = element('option', previousLabel); option.value = previous; group.append(option); select.append(group);
  }
  select.value = previous;
  select.title = select.selectedOptions[0]?.textContent || '';
}

async function refreshTelemetryScopes() {
  const load = ++state.scopeLoad; const projects = [...state.projects];
  const keys = []; let next = 0;
  // Bound inventory requests when the relay manages many projects.
  await Promise.all(Array.from({ length: Math.min(projects.length, 4) }, async () => {
    while (next < projects.length) {
      const project = projects[next++];
      keys.push(...await collection(`/projects/${encodeURIComponent(project.project_id)}/telemetry/keys`));
    }
  }));
  if (load !== state.scopeLoad) return;
  state.telemetryKeys = keys; renderTelemetryScopes();
}

function renderHealth(health) {
  const warning = health.storage_errors > 0 || health.dropped_records > 0;
  $('health-dot').classList.toggle('warn', warning);
  text('health-text', warning ? 'Storage needs attention' : 'Relay connected');
  $('health-text').title = `${number(health.pending_writes)} pending writes · ${number(health.dropped_records)} dropped records · ${number(health.storage_errors)} storage errors`;
}

async function workspace() {
  const [projects, profiles, health, accounts] = await Promise.all([collection('/projects'), api('/upstream-profiles'), api('/health'), api('/upstream-accounts')]);
  state.projects = projects; state.profiles = profiles.items; state.accounts = accounts.items; state.loginAvailable = accounts.login_available; state.managementEnabled = accounts.management_enabled;
  const options = projects.map(project => ({ value: project.project_id, label: `${project.display_name}${project.enabled ? '' : ' · disabled'}` }));
  selectOptions('token-project', options);
  selectOptions('project-profile', state.profiles.map(profile => ({ value: profile.profile_id, label: `${profile.profile_id} · ${profile.auth_mode}` })));
  text('project-count', number(projects.length)); text('profile-count', `${number(state.profiles.length)} upstream profile${state.profiles.length === 1 ? '' : 's'}`);
  renderHealth(health); renderProjects(); renderAccounts(); await refreshTelemetryScopes();
}

function setView(view) {
  state.view = view; notice('');
  for (const name of ['telemetry', 'projects', 'tokens', 'accounts']) $(`${name}-view`).hidden = name !== view;
  for (const nav of document.querySelectorAll('[data-view]')) {
    const active = nav.dataset.view === view; nav.classList.toggle('active', active);
    if (active) nav.setAttribute('aria-current', 'page'); else nav.removeAttribute('aria-current');
  }
  if (view === 'tokens') return loadTokens();
  if (view === 'accounts') return workspace();
}

function stamp() { text('updated-at', `Updated ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`); }

function telemetryQuery() {
  const query = new URLSearchParams({ limit: '100' });
  if ($('telemetry-scope').value) {
    const [project, key] = JSON.parse($('telemetry-scope').value);
    query.set('project_id', project);
    if (key) query.set('key_id', key);
  }
  for (const [key, value] of new FormData($('filters'))) {
    if (!value) continue;
    query.set(key, ['from', 'to'].includes(key) ? new Date(value).toISOString() : value.trim());
  }
  if (query.has('from') && query.has('to') && query.get('from') > query.get('to')) throw new Error('The start time must be before the end time.');
  return query.toString();
}

async function loadTelemetry(more = false) {
  const load = ++state.telemetryLoad;
  const query = more ? state.query : telemetryQuery();
  const page = await api(`/telemetry/requests?${query}${more && state.cursor !== null ? `&cursor=${state.cursor}` : ''}`);
  if (load !== state.telemetryLoad) return;
  state.query = query;
  state.requests = more ? state.requests.concat(page.items) : page.items;
  state.cursor = page.next_cursor;
  renderTelemetry(); stamp();
}

function renderTelemetry() {
  const requests = state.requests;
  const generated = requests.filter(request => request.operation !== 'count_tokens');
  const total = field => generated.reduce((sum, request) => sum + Number(request.usage?.[field] || 0), 0);
  text('stat-requests', number(requests.length)); text('stat-input', number(total('input_tokens'))); text('stat-output', number(total('output_tokens')));
  text('stat-cache', number(total('cache_read_input_tokens') + total('cache_creation_input_tokens')));
  text('stat-cache-note', `${number(total('cache_read_input_tokens'))} read · ${number(total('cache_creation_input_tokens'))} created`);
  text('stat-request-note', `${number(requests.filter(request => !request.synthetic).length)} provider · ${number(requests.filter(request => request.synthetic).length)} synthetic`);
  const models = new Map();
  for (const request of generated) {
    const value = ['input_tokens', 'output_tokens', 'cache_read_input_tokens', 'cache_creation_input_tokens'].reduce((sum, key) => sum + Number(request.usage?.[key] || 0), 0);
    models.set(request.resolved_model, (models.get(request.resolved_model) || 0) + value);
  }
  const ordered = [...models].sort((a, b) => b[1] - a[1]).slice(0, 5);
  $('model-bars').replaceChildren();
  if (!ordered.length) $('model-bars').append(element('p', 'No generation usage in this selection.', 'empty-inline'));
  for (const [model, count] of ordered) {
    const row = element('div'); const heading = element('div', undefined, 'model-bar-heading');
    heading.append(element('span', model), element('span', number(count)));
    const bar = element('progress'); bar.max = Math.max(ordered[0][1], 1); bar.value = count; bar.setAttribute('aria-label', `${model}: ${number(count)} tokens`);
    row.append(heading, bar); $('model-bars').append(row);
  }
  const latencies = requests.map(request => request.latency_ms).filter(value => value !== null && value !== undefined);
  const health = [
    ['Completed', requests.filter(request => request.completion === 'complete').length],
    ['Failed / cancelled', requests.filter(request => ['failed', 'cancelled'].includes(request.completion)).length],
    ['Partial or unavailable usage', generated.filter(request => request.usage_status !== 'complete').length],
    ['Mean latency', latencies.length ? `${number(Math.round(latencies.reduce((a, b) => a + b, 0) / latencies.length))} ms` : '—'],
  ];
  $('request-health').replaceChildren(...health.map(([label, value]) => { const row = element('div'); row.append(element('dt', label), element('dd', typeof value === 'number' ? number(value) : value)); return row; }));
  $('request-rows').replaceChildren();
  if (!requests.length) emptyRow('request-rows', 8, 'No requests match these filters. Try a wider time range.');
  for (const request of [...requests].reverse()) {
    const row = element('tr');
    const ratio = request.context?.input_utilization_ratio;
    const status = request.completion === 'complete' ? 'good' : request.completion === 'in_progress' ? 'warn' : 'bad';
    row.append(
      cell(lines(time(request.started_at), request.client_api || request.operation)),
      cell(lines(request.project_id || '—', request.client_id)),
      cell(lines(request.resolved_model, request.upstream_profile || 'default', true)),
      cell(lines(`${number(request.usage?.input_tokens)} / ${number(request.usage?.output_tokens)}`, `${number(request.usage?.cache_read_input_tokens)} cached read`, true)),
      cell(lines(ratio === null || ratio === undefined ? '—' : `${(ratio * 100).toFixed(1)}%`, request.context?.window_tokens ? `${number(request.context.window_tokens)} capacity` : 'Capacity unknown')),
      cell(request.latency_ms === null ? '—' : `${number(request.latency_ms)} ms`),
      cell(badge(`${request.http_status} · ${request.completion}`, status)),
      cell(button('Details', () => requestDetail(request))),
    ); $('request-rows').append(row);
  }
  text('request-scope', `${number(requests.length)} loaded · newest loaded first`);
  text('pagination-note', state.cursor !== null ? 'More results available. Totals cover loaded pages; pages are fetched oldest first.' : 'All matching requests loaded. Counting requests are excluded from token totals.');
  $('load-more').hidden = state.cursor === null; $('export-telemetry').disabled = !requests.length;
}

function renderProjects() {
  text('projects-note', `${number(state.projects.length)} projects · ${number(state.projects.filter(project => project.enabled).length)} enabled`);
  $('project-rows').replaceChildren();
  if (!state.projects.length) emptyRow('project-rows', 5, 'Create a project to give your clients their own identity and telemetry.');
  for (const project of state.projects) {
    const row = element('tr'); const actions = element('div', undefined, 'actions');
    actions.append(button('Edit', () => projectEditor(project)), button('Tokens', () => { $('token-project').value = project.project_id; return setView('tokens'); }), button('Delete', () => confirmAction('Delete project?', `Delete ${project.project_id} and revoke all its managed credentials? Historical telemetry will be retained.`, 'Delete project', async () => { await api(`/projects/${encodeURIComponent(project.project_id)}`, 'DELETE'); await workspace(); notice('Project deleted. Historical telemetry is retained.'); }), 'danger-text'));
    row.append(cell(lines(project.display_name, project.project_id)), cell(lines(project.upstream_profile || 'default', 'Server-owned provider credentials')), cell(Object.entries(project.labels || {}).map(([key, value]) => `${key}: ${value}`).join(' · ') || '—'), cell(badge(project.enabled ? 'Enabled' : 'Disabled', project.enabled ? 'good' : 'warn')), cell(actions));
    $('project-rows').append(row);
  }
  $('profile-cards').replaceChildren(...state.profiles.map(profile => { const card = element('div', undefined, 'profile-card'); card.append(element('h3', profile.profile_id), element('p', `${profile.backend} · ${profile.auth_mode}${profile.synthetic ? ' · synthetic' : ''}`)); return card; }));
}

const accountPath = id => `/upstream-accounts/${encodeURIComponent(id)}`;

function renderAccounts() {
  $('add-account').disabled = !state.managementEnabled;
  text('accounts-note', state.loginAvailable ? `${number(state.accounts.length)} accounts · Browser login ready` : 'Claude login helper unavailable');
  $('account-rows').replaceChildren();
  if (!state.accounts.length) emptyRow('account-rows', 6, 'Add an upstream account, then connect it using your browser.');
  for (const account of state.accounts) {
    const auth = account.upstream_auth || {}; const row = element('tr'); const actions = element('div', undefined, 'actions account-actions');
    const assigned = state.projects.filter(project => project.upstream_profile === account.profile_id);
    const active = account.login && ['starting', 'awaiting_code', 'exchanging'].includes(account.login.state);
    if (account.managed) {
      const connect = button(active ? 'Continue login' : auth.state === 'ready' ? 'Reconnect' : 'Connect', () => connectAccount(account));
      connect.disabled = !state.loginAvailable; actions.append(connect, button('Rename', () => accountEditor(account)));
      if (auth.state === 'ready') actions.append(button('Refresh access', async () => { await api(accountPath(account.account_id) + '/refresh', 'POST'); await workspace(); notice('Account access refreshed.'); }));
      if (auth.expires_at || active) actions.append(button('Disconnect', () => confirmAction('Disconnect upstream account?', `Projects using ${account.account_id} will lose upstream access until you reconnect. Its credentials will be removed from the relay.`, 'Disconnect account', async () => { await api(accountPath(account.account_id) + '/disconnect', 'POST'); await workspace(); notice('Account disconnected.'); }), 'danger-text'));
      const remove = button('Delete', () => confirmAction('Delete upstream account?', `Delete ${account.account_id} and its stored credentials? Reassign any projects first.`, 'Delete account', async () => { await api(accountPath(account.account_id), 'DELETE'); await workspace(); notice('Upstream account deleted.'); }), 'danger-text');
      remove.disabled = assigned.length > 0; remove.title = assigned.length ? 'Reassign this account’s projects before deleting' : ''; actions.append(remove);
    } else actions.append(element('span', 'Server configured', 'hint'));
    const label = active ? 'Login in progress' : auth.state === 'ready' ? 'Connected' : 'Needs connection';
    row.append(cell(lines(account.display_name, account.account_id)), cell(badge(account.managed ? 'Relay managed' : 'Server configured')), cell(lines('', auth.last_error ? 'Reconnect required or refresh unavailable' : ''), badge(label, active ? 'warn' : auth.state === 'ready' ? 'good' : 'warn')), cell(time(auth.expires_at)), cell(lines(number(assigned.length), assigned.map(project => project.project_id).join(', '))), cell(actions));
    $('account-rows').append(row);
  }
}

function accountEditor(account = null) {
  state.editingAccount = account; $('account-form').reset(); text('account-form-error', '');
  text('account-dialog-title', account ? 'Rename upstream account' : 'Add upstream account');
  $('account-id').disabled = !!account; $('account-id').value = account?.account_id || ''; $('account-name').value = account?.display_name || '';
  $('account-dialog').showModal();
}

async function saveAccount() {
  const id = $('account-id').value.trim(); const display_name = $('account-name').value.trim();
  await api(state.editingAccount ? accountPath(id) : '/upstream-accounts', state.editingAccount ? 'PUT' : 'POST', state.editingAccount ? { display_name } : { account_id: id, display_name });
  const added = !state.editingAccount; $('account-dialog').close(); await workspace();
  if (added && state.loginAvailable) await connectAccount(state.accounts.find(account => account.account_id === id));
  else notice('Upstream account saved.');
}

function clearLogin(cancel = false) {
  const login = state.login; state.login = null; clearTimeout(state.loginTimer); state.loginTimer = null;
  $('login-code').value = ''; $('login-url').value = ''; $('open-login-url').removeAttribute('href'); $('login-instructions').hidden = true;
  if (cancel && login?.path && !login.terminal && state.key) {
    // Capture the current key before disconnect clears it. Never persist login material.
    fetch(API + login.path, { method: 'DELETE', headers: { Authorization: `Bearer ${state.key}` }, credentials: 'omit', cache: 'no-store', keepalive: true }).catch(() => {});
  }
}

async function connectAccount(account) {
  clearLogin(true); const session = state.session;
  text('login-title', `Connect ${account.display_name}`); text('login-state', 'Starting login…'); text('login-error', ''); text('close-login', 'Cancel login');
  $('login-dialog').showModal();
  // Track initialization too, so closing the dialog cancels a late POST response.
  const pending = {}; state.login = pending;
  try {
    const active = account.login && ['starting', 'awaiting_code', 'exchanging'].includes(account.login.state);
    const flow = active ? await api(accountPath(account.account_id) + `/login/${encodeURIComponent(account.login.flow_id)}`) : await api(accountPath(account.account_id) + '/login', 'POST');
    const path = accountPath(account.account_id) + `/login/${encodeURIComponent(flow.flow_id)}`;
    if (state.session !== session || state.login !== pending || !$('login-dialog').open) {
      if (state.session === session && state.key) await api(path, 'DELETE');
      return;
    }
    const login = { path, terminal: false }; state.login = login; showLogin(flow, login); pollLogin(login);
  } catch (error) { if (state.session === session && $('login-dialog').open) text('login-error', error.message); }
}

function showLogin(flow, login) {
  if (state.login !== login) return;
  const terminal = !['starting', 'awaiting_code', 'exchanging'].includes(flow.state); login.terminal = terminal;
  const messages = { starting: 'Preparing your browser login…', awaiting_code: 'Waiting for your browser authorization.', exchanging: 'Completing the connection…', connected: 'Account connected. Select this account in a project’s upstream profile.', cancelled: 'Login cancelled.', expired: 'Login expired. Close this dialog and connect again.', failed: 'Login failed. Close this dialog and connect again.' };
  text('login-state', messages[flow.state] || flow.state); text('login-error', flow.error || '');
  $('login-instructions').hidden = flow.state !== 'awaiting_code';
  $('login-url').value = flow.authorization_url || '';
  if (flow.authorization_url) $('open-login-url').href = flow.authorization_url; else $('open-login-url').removeAttribute('href');
  if (terminal) { $('login-code').value = ''; text('close-login', 'Close'); }
}

async function pollLogin(login) {
  if (state.login !== login || login.terminal) return;
  try {
    const flow = await api(login.path); if (state.login !== login) return;
    showLogin(flow, login);
    if (login.terminal) { await workspace(); return; }
    state.loginTimer = setTimeout(() => pollLogin(login), 1000);
  } catch (error) { if (state.login === login) { text('login-error', error.message); state.loginTimer = setTimeout(() => pollLogin(login), 3000); } }
}

async function submitLoginCode() {
  const login = state.login; if (!login || login.terminal) throw new Error('Start a new login.');
  const code = $('login-code').value.trim(); $('login-code').value = '';
  await api(login.path + '/code', 'POST', { code });
  if (state.login !== login) return;
  $('login-instructions').hidden = true; $('login-url').value = ''; $('open-login-url').removeAttribute('href'); text('login-state', 'Completing the connection…');
}

function projectEditor(project = null) {
  state.editingProject = project; $('project-form').reset(); text('project-form-error', '');
  text('project-dialog-title', project ? 'Edit project' : 'Add project');
  $('project-id').value = project?.project_id || ''; $('project-id').disabled = !!project;
  $('project-name').value = project?.display_name || ''; $('project-profile').value = project?.upstream_profile || 'default';
  $('project-enabled').checked = project?.enabled ?? true;
  $('project-labels').value = project && Object.keys(project.labels || {}).length ? JSON.stringify(project.labels, null, 2) : '';
  $('project-dialog').showModal();
}

async function saveProject() {
  const labels = $('project-labels').value.trim() ? JSON.parse($('project-labels').value) : {};
  if (!labels || Array.isArray(labels) || typeof labels !== 'object' || Object.values(labels).some(value => typeof value !== 'string')) throw new Error('Labels must be a JSON object with string values.');
  const project = { project_id: $('project-id').value.trim(), display_name: $('project-name').value.trim(), upstream_profile: $('project-profile').value, enabled: $('project-enabled').checked, labels };
  await api(state.editingProject ? `/projects/${encodeURIComponent(project.project_id)}` : '/projects', state.editingProject ? 'PUT' : 'POST', project);
  $('project-dialog').close(); await workspace(); notice('Project saved.');
}

async function loadTokens() {
  const projectID = $('token-project').value; const load = ++state.tokenLoad;
  const project = state.projects.find(project => project.project_id === projectID);
  $('issue-token').disabled = !project;
  text('token-project-note', project ? `${project.project_id} · upstream: ${project.upstream_profile}${project.enabled ? '' : ' · project disabled'}` : 'Create a project to issue credentials.');
  emptyRow('token-rows', 6, project ? 'Loading credentials…' : 'No projects available.');
  if (!project) { state.tokens = []; text('tokens-note', 'No project selected'); return; }
  const tokens = await collection(`/projects/${encodeURIComponent(projectID)}/credentials`);
  if (load !== state.tokenLoad || $('token-project').value !== projectID) return;
  state.tokens = tokens; renderTokens(project); stamp();
}

function renderTokens(project) {
  text('tokens-note', `${number(state.tokens.length)} credentials`); $('token-rows').replaceChildren();
  if (!state.tokens.length) emptyRow('token-rows', 6, 'No managed credentials yet. Issue a token for this project.');
  for (const token of state.tokens) {
    const expired = token.expiresAt && new Date(token.expiresAt) <= new Date();
    const kind = token.exchange_only ? 'Vertex workload key' : token.token_type === 'maas' ? (token.ephemeral ? 'MaaS · ephemeral' : 'MaaS API key') : token.mock_adc ? 'Legacy combined key' : 'Anthropic API key';
    const grants = [token.catalog_models ? 'Messages catalog' : null, token.vertex_catalog_models ? 'Vertex catalog' : null, ...(token.models || [])].filter(Boolean);
    const row = element('tr'); const actions = element('div', undefined, 'actions');
    actions.append(button('Edit grants', () => tokenEditor(token)), button('Rotate', () => confirmAction('Rotate token?', 'The current key and its outstanding Google tokens will stop working. Save the replacement when it is shown. Its expiration stays the same.', 'Rotate token', async () => { const result = await api(tokenPath(project.project_id, token.key_id) + '/rotate', 'POST'); showSecret(result); await loadTokens(); })), button('Revoke', () => confirmAction('Revoke token?', `Permanently revoke ${token.name || token.client_id}? Requests using this key or its Google tokens will be rejected.`, 'Revoke token', async () => { await api(tokenPath(project.project_id, token.key_id), 'DELETE'); await loadTokens(); await refreshTelemetryScopes(); notice('Token revoked.'); }), 'danger-text'));
    row.append(cell(lines(token.name || token.client_id, token.name ? token.client_id : token.key_id)), cell(badge(kind)), cell(lines(grants.join(', ') || 'No models granted', `${providerName(project)}${token.exchange_only ? ' · Token exchange only' : token.mock_adc ? ' · Legacy Google token exchange' : ''}`)), cell(lines(token.expiresAt ? time(token.expiresAt) : 'No expiration', token.ephemeral ? 'Ephemeral' : '')), cell(badge(expired ? 'Expired' : project.enabled ? 'Active' : 'Project disabled', expired ? 'bad' : project.enabled ? 'good' : 'warn')), cell(actions));
    $('token-rows').append(row);
  }
}

const tokenPath = (project, key) => `/projects/${encodeURIComponent(project)}/credentials/${encodeURIComponent(key)}`;

function projectProfile(project) {
  return state.profiles.find(item => item.profile_id === project?.upstream_profile);
}

function providerName(project) {
  const profile = projectProfile(project);
  if (profile?.backend === 'mock') return 'Development simulator';
  if (profile?.auth_mode === 'claude-oauth') return 'Anthropic OAuth';
  return profile?.backend === 'anthropic' ? 'Anthropic · server credentials' : 'Unavailable';
}

function tokenAuthFlow(vertex) {
  text('token-auth-flow', vertex ? 'Exchange this workload key for a short-lived Google bearer token, then use that token for Vertex requests. The workload key cannot authenticate inference directly.' : 'Use this API key directly with the relay. Google token exchange is disabled.');
}

function tokenTypeChanged() {
  const kind = $('token-type').value; const maas = kind === 'maas';
  $('maas-fields').hidden = !maas; $('token-name').required = maas && !$('token-ephemeral').checked;
  $('token-expiry').placeholder = $('token-ephemeral').checked ? '1h' : '90d';
  if (!state.editingToken) tokenAuthFlow(kind === 'vertex');
}

function tokenEditor(token = null) {
  state.editingToken = token; state.tokenFormProject = $('token-project').value;
  $('token-form').reset(); text('token-form-error', '');
  text('token-dialog-title', token ? 'Edit token grants' : 'Issue token');
  text('token-target', `Project: ${state.tokenFormProject}`); $('token-issue-fields').hidden = !!token;
  $('token-client').value = token?.client_id || '';
  $('token-models').value = (token?.models || []).join('\n');
  const project = state.projects.find(item => item.project_id === state.tokenFormProject);
  $('token-backend').value = providerName(project);
  if (token) {
    $('token-catalog').checked = token.catalog_models || token.vertex_catalog_models;
    $('token-name').required = false;
    if (token.mock_adc && !token.exchange_only) text('token-auth-flow', 'This legacy key allows both direct API access and Google token exchange. Its existing authentication permissions are preserved.');
    else tokenAuthFlow(token.exchange_only);
  } else { $('token-catalog').checked = true; tokenTypeChanged(); }
  text('token-form-note', token ? 'Editing replaces grants. The token value, type, and expiration are preserved.' : 'Credentials are local to the relay. Upstream OAuth stays on the server.');
  text('token-submit', token ? 'Save grants' : 'Issue token'); $('token-dialog').showModal();
}

async function saveToken() {
  const models = [...new Set($('token-models').value.split(/[\s,]+/).filter(Boolean))];
  const project = state.projects.find(item => item.project_id === state.tokenFormProject);
  const profile = projectProfile(project);
  if (!profile) throw new Error('The project’s upstream profile is unavailable.');
  const old = state.editingToken;
  const vertex = old ? !!old.exchange_only : $('token-type').value === 'vertex';
  const catalog = $('token-catalog').checked;
  if (!models.length && !catalog) throw new Error('Grant at least one model or allow available models.');
  const hadCatalog = old && (old.catalog_models || old.vertex_catalog_models);
  const data = {
    client_id: $('token-client').value.trim(), models,
    backends: old ? old.backends : [profile.backend],
    catalog_models: catalog && (hadCatalog ? old.catalog_models : !vertex),
    vertex_catalog_models: catalog && (hadCatalog ? old.vertex_catalog_models : vertex),
    mock_adc: old ? old.mock_adc : vertex,
    exchange_only: vertex,
  };
  if (!state.editingToken && $('token-type').value === 'maas') {
    data.token_type = 'maas'; data.ephemeral = $('token-ephemeral').checked;
    if ($('token-name').value.trim()) data.name = $('token-name').value.trim();
    if ($('token-expiry').value.trim()) data.expiresIn = $('token-expiry').value.trim();
  }
  const path = state.editingToken ? tokenPath(state.tokenFormProject, state.editingToken.key_id) : `/projects/${encodeURIComponent(state.tokenFormProject)}/credentials`;
  const result = await api(path, state.editingToken ? 'PUT' : 'POST', data);
  $('token-dialog').close();
  if (!state.editingToken) showSecret(result); else notice('Token grants saved.');
  await loadTokens();
  await refreshTelemetryScopes();
}

function showSecret(result) {
  $('issued-secret').value = result.api_key; delete result.api_key;
  state.secretID = result.key_id;
  text('secret-details', `${result.client_id} · ${result.key_id}${result.expiresAt ? ` · expires ${time(result.expiresAt)}` : ''}`);
  text('secret-status', ''); $('secret-dialog').showModal();
}

function download(value, filename, type) {
  const url = URL.createObjectURL(new Blob([value], { type }));
  const link = element('a'); link.href = url; link.download = filename; document.body.append(link); link.click(); link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function confirmAction(title, message, label, callback) {
  text('confirm-title', title); text('confirm-message', message); text('confirm-action', label);
  state.confirmation = callback; $('confirm-dialog').showModal();
}

async function requestDetail(request) {
  state.requestID = request.request_id; state.events = []; state.eventCursor = null;
  text('request-detail', JSON.stringify(request, null, 2)); text('request-events', 'Loading usage events…');
  $('more-events').hidden = true; $('request-dialog').showModal(); await requestEvents();
}

async function requestEvents(more = false) {
  const id = state.requestID;
  const page = await api(`/telemetry/requests/${encodeURIComponent(id)}/events?limit=100${more && state.eventCursor !== null ? `&cursor=${state.eventCursor}` : ''}`);
  if (state.requestID !== id) return;
  state.events = more ? state.events.concat(page.items) : page.items; state.eventCursor = page.next_cursor;
  text('request-events', state.events.length ? JSON.stringify(state.events, null, 2) : 'No usage events recorded.');
  $('more-events').hidden = state.eventCursor === null;
}

$('connect-form').addEventListener('submit', event => {
  event.preventDefault(); const key = $('admin-key').value.trim(); $('admin-key').value = '';
  task(event.submitter, async () => {
    state.key = key; state.session++; text('connect-error', '');
    try { await workspace(); await loadTelemetry(); }
    catch (error) { disconnect(); throw error; }
    $('connect-screen').hidden = true; $('app').hidden = false; await setView('telemetry');
  });
});
$('disconnect').addEventListener('click', disconnect);
$('refresh').addEventListener('click', event => task(event.currentTarget, async () => { notice(''); await workspace(); if (state.view === 'telemetry') await loadTelemetry(); else if (state.view === 'tokens') await loadTokens(); stamp(); }));
for (const nav of document.querySelectorAll('[data-view]')) nav.addEventListener('click', () => task(nav, () => setView(nav.dataset.view)));
$('filters').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, () => loadTelemetry()); });
$('telemetry-scope').addEventListener('change', event => { event.currentTarget.title = event.currentTarget.selectedOptions[0]?.textContent || ''; });
$('load-more').addEventListener('click', event => task(event.currentTarget, () => loadTelemetry(true)));
$('export-telemetry').addEventListener('click', () => download(JSON.stringify({ filters: Object.fromEntries(new URLSearchParams(state.query)), items: state.requests, next_cursor: state.cursor }, null, 2), 'model-relay-telemetry.json', 'application/json'));
$('add-account').addEventListener('click', () => accountEditor());
$('account-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, saveAccount, 'account-form-error'); });
$('login-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, submitLoginCode, 'login-error'); });
$('copy-login-url').addEventListener('click', event => task(event.currentTarget, async () => {
  if (navigator.clipboard?.writeText) { await navigator.clipboard.writeText($('login-url').value); text('login-state', 'Link copied. Open it in your authenticated browser.'); }
  else { $('login-url').focus(); $('login-url').select(); }
}, 'login-error'));
$('login-dialog').addEventListener('close', () => clearLogin(true));
$('add-project').addEventListener('click', () => projectEditor());
$('project-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, saveProject, 'project-form-error'); });
$('token-project').addEventListener('change', event => task(event.currentTarget, loadTokens));
$('issue-token').addEventListener('click', () => tokenEditor());
$('token-type').addEventListener('change', tokenTypeChanged);
$('token-ephemeral').addEventListener('change', () => { $('token-name').required = !$('token-ephemeral').checked; $('token-expiry').placeholder = $('token-ephemeral').checked ? '1h' : '90d'; });
$('token-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, saveToken, 'token-form-error'); });
$('confirm-action').addEventListener('click', event => { const callback = state.confirmation; $('confirm-dialog').close(); if (callback) task(event.currentTarget, callback); });
$('copy-secret').addEventListener('click', event => task(event.currentTarget, async () => {
  if (navigator.clipboard?.writeText) { await navigator.clipboard.writeText($('issued-secret').value); text('secret-status', 'Token copied.'); }
  else { $('issued-secret').focus(); $('issued-secret').select(); text('secret-status', 'Token selected. Copy it with your keyboard.'); }
}));
$('download-secret').addEventListener('click', () => { download($('issued-secret').value + '\n', `${state.secretID}.txt`, 'text/plain'); text('secret-status', 'Token downloaded. Keep the file private.'); });
$('more-events').addEventListener('click', event => task(event.currentTarget, () => requestEvents(true)));
for (const control of document.querySelectorAll('[data-close]')) control.addEventListener('click', () => $(control.dataset.close).close());
$('secret-dialog').addEventListener('close', () => { $('issued-secret').value = ''; state.secretID = ''; text('secret-details', ''); text('secret-status', ''); });
$('confirm-dialog').addEventListener('close', () => { state.confirmation = null; });
$('request-dialog').addEventListener('close', () => { state.requestID = ''; state.events = []; state.eventCursor = null; text('request-detail', ''); text('request-events', ''); });
window.addEventListener('pagehide', disconnect);
text('origin', window.location.origin);
const yesterday = new Date(Date.now() - 24 * 60 * 60 * 1000);
const local = new Date(yesterday.getTime() - yesterday.getTimezoneOffset() * 60 * 1000);
$('filter-from').defaultValue = local.toISOString().slice(0, 16);
$('admin-key').focus();
