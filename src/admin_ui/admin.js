'use strict';

const $ = id => document.getElementById(id);
const API = '/api/private/v1';
const state = { key: '', session: 0, view: 'telemetry', projects: [], profiles: [], accounts: [], loginAvailable: false, managementEnabled: false, editingAccount: null, login: null, loginTimer: null, tokens: [], telemetryKeys: [], requests: [], aggregate: null, cursor: null, query: '', telemetryLoad: 0, tokenLoad: 0, scopeLoad: 0, editingProject: null, editingToken: null, tokenFormProject: '', secretID: '', confirmation: null, requestID: '', events: [], eventCursor: null };
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
  previewGeneration++; projectPreviewGeneration++; routingInventories.clear(); modelEditor = null; accountTokenTarget = null;
  state.key = ''; state.session++; state.telemetryLoad++; state.tokenLoad++; state.scopeLoad++;
  state.projects = []; state.profiles = []; state.accounts = []; state.editingAccount = null; state.tokens = []; state.telemetryKeys = []; state.requests = []; state.events = [];
  state.cursor = null; state.confirmation = null; state.editingProject = null; state.editingToken = null;
  state.query = ''; state.aggregate = null;
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
  for (const id of ['aggregate-scope', 'stat-request-note', 'stat-cache-note', 'request-scope', 'pagination-note', 'projects-note', 'tokens-note', 'token-project-note', 'updated-at', 'profile-count']) text(id, '');
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

// Drop and error counters are totals since the relay started, so only a recent one is a warning.
const HEALTH_WINDOW_MS = 15 * 60 * 1000;
function renderHealth(health) {
  const recent = at => at !== null && at !== undefined && Date.now() - at < HEALTH_WINDOW_MS;
  const errors = recent(health.last_storage_error_at_unix_ms);
  const requestsLost = health.dropped_requests > 0 && recent(health.last_dropped_at_unix_ms);
  const warning = errors || requestsLost;
  $('health-dot').classList.toggle('warn', warning);
  text('health-text', errors ? 'Storage errors' : requestsLost ? 'Telemetry dropping requests' : 'Relay connected');
  $('health-text').title = `${number(health.pending_writes)} pending writes · since start: ${number(health.dropped_requests ?? health.dropped_records)} dropped requests, ${number(health.dropped_events ?? 0)} dropped usage events, ${number(health.storage_errors)} storage errors`;
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
  const query = new URLSearchParams({ limit: '100', order: 'desc' });
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
  const pageRequest = api(`/telemetry/requests?${query}${more && state.cursor !== null ? `&cursor=${state.cursor}` : ''}`);
  let page, aggregate;
  if (more) page = await pageRequest;
  else {
    const scope = new URLSearchParams(query);
    for (const key of ['limit', 'cursor', 'order', 'format']) scope.delete(key);
    [page, aggregate] = await Promise.all([pageRequest, api(`/telemetry/aggregate?${scope}`)]);
  }
  if (load !== state.telemetryLoad) return;
  if (!more) state.aggregate = aggregate;
  state.query = query;
  state.requests = more ? state.requests.concat(page.items) : page.items;
  state.cursor = page.next_cursor;
  renderTelemetry(); stamp();
}

function renderTelemetry() {
  const requests = state.requests;
  const overview = state.aggregate;
  const total = field => Number(overview.generation_tokens[field] || 0);
  text('stat-requests', number(overview.request_count)); text('stat-input', number(total('input_tokens'))); text('stat-output', number(total('output_tokens')));
  text('stat-cache', number(total('cache_read_input_tokens') + total('cache_creation_input_tokens')));
  text('stat-cache-note', `${number(total('cache_read_input_tokens'))} read · ${number(total('cache_creation_input_tokens'))} created`);
  text('stat-request-note', `${number(overview.provider_requests)} provider · ${number(overview.synthetic_requests)} synthetic`);
  text('aggregate-scope', `All matching requests · totals calculated ${time(overview.generated_at)}`);
  const ordered = overview.models.map(item => [item.model, item.tokens]);
  $('model-bars').replaceChildren();
  if (!ordered.length) $('model-bars').append(element('p', 'No generation usage in this selection.', 'empty-inline'));
  for (const [model, count] of ordered) {
    const row = element('div'); const heading = element('div', undefined, 'model-bar-heading');
    heading.append(element('span', model), element('span', number(count)));
    const bar = element('progress'); bar.max = Math.max(ordered[0][1], 1); bar.value = count; bar.setAttribute('aria-label', `${model}: ${number(count)} tokens`);
    row.append(heading, bar); $('model-bars').append(row);
  }
  const health = [
    ['Completed', overview.request_health.complete],
    ['Failed / cancelled', overview.request_health.failed_cancelled],
    ['Partial or unavailable usage', overview.generation_usage_counts.partial + overview.generation_usage_counts.unavailable],
    ['Mean latency', overview.request_health.mean_latency_ms === null ? '—' : `${number(overview.request_health.mean_latency_ms)} ms`],
  ];
  $('request-health').replaceChildren(...health.map(([label, value]) => { const row = element('div'); row.append(element('dt', label), element('dd', typeof value === 'number' ? number(value) : value)); return row; }));
  $('request-rows').replaceChildren();
  if (!requests.length) emptyRow('request-rows', 8, 'No requests match these filters. Try a wider time range.');
  for (const request of requests) {
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
  text('pagination-note', state.cursor !== null ? 'Older requests available. Overview totals cover all matching requests.' : 'All matching requests loaded. Counting requests are excluded from token totals.');
  $('load-more').hidden = state.cursor === null; $('export-telemetry').disabled = !requests.length;
}

function renderProjects() {
  text('projects-note', `${number(state.projects.length)} projects · ${number(state.projects.filter(project => project.enabled).length)} enabled`);
  $('project-rows').replaceChildren();
  if (!state.projects.length) emptyRow('project-rows', 5, 'Create a project to give your clients their own identity and telemetry.');
  for (const project of state.projects) {
    const row = element('tr'); const actions = element('div', undefined, 'actions');
    actions.append(button('Edit', () => projectEditor(project)), button('Tokens', () => { $('token-project').value = project.project_id; return setView('tokens'); }), button('Delete', () => confirmAction('Delete project?', `Delete ${project.project_id} and revoke all its managed credentials? Historical telemetry will be retained.`, 'Delete project', async () => { await api(`/projects/${encodeURIComponent(project.project_id)}`, 'DELETE'); await workspace(); notice('Project deleted. Historical telemetry is retained.'); }), 'danger-text'));
    row.append(cell(lines(project.display_name, project.project_id)), cell(lines(routingSummary(project), routingDetail(project))), cell(Object.entries(project.labels || {}).map(([key, value]) => `${key}: ${value}`).join(' · ') || '—'), cell(badge(project.enabled ? 'Enabled' : 'Disabled', project.enabled ? 'good' : 'warn')), cell(actions));
    $('project-rows').append(row);
  }
  $('profile-cards').replaceChildren(...state.profiles.map(profile => { const card = element('div', undefined, 'profile-card'); card.append(element('h3', profile.profile_id), element('p', `${profile.provider || profile.backend} · ${profile.auth_mode}${profile.synthetic ? ' · synthetic' : ''}`), button('Models', () => modelsEditor(profile.profile_id))); return card; }));
}

const accountPath = id => `/upstream-accounts/${encodeURIComponent(id)}`;

function renderAccounts() {
  $('add-account').disabled = !state.managementEnabled;
  text('accounts-note', state.loginAvailable ? `${number(state.accounts.length)} accounts · Browser login ready` : 'Claude login helper unavailable');
  $('account-rows').replaceChildren();
  if (!state.accounts.length) emptyRow('account-rows', 6, 'Add an upstream account, then connect it using your browser.');
  for (const account of state.accounts) {
    const auth = account.upstream_auth || {}; const row = element('tr'); const actions = element('div', undefined, 'actions account-actions');
    const assigned = state.projects.filter(project => routingProfiles(projectRouting(project)).includes(account.profile_id));
    const active = account.login && ['starting', 'awaiting_code', 'exchanging'].includes(account.login.state);
    if (account.managed) {
      if (account.provider === 'enmaas') { actions.append(button(auth.state === 'ready' ? 'Rotate token' : 'Set token', () => tokenAccountEditor(account)), button('Rename', () => accountEditor(account))); } else {
      const connect = button(active ? 'Continue login' : auth.state === 'ready' ? 'Reconnect' : 'Connect', () => connectAccount(account));
      connect.disabled = !state.loginAvailable; actions.append(connect, button('Rename', () => accountEditor(account)));
      if (auth.state === 'ready') actions.append(button('Refresh access', async () => { await api(accountPath(account.account_id) + '/refresh', 'POST'); await workspace(); notice('Account access refreshed.'); }));
      }
      if (auth.state === 'ready' || auth.expires_at || active) actions.append(button('Disconnect', () => confirmAction('Disconnect upstream account?', `Projects using ${account.account_id} will lose upstream access until you reconnect. Its credentials will be removed from the relay.`, 'Disconnect account', async () => { await api(accountPath(account.account_id) + '/disconnect', 'POST'); await workspace(); notice('Account disconnected.'); }), 'danger-text'));
      const remove = button('Delete', () => confirmAction('Delete upstream account?', `Delete ${account.account_id} and its stored credentials? Reassign any projects first.`, 'Delete account', async () => { await api(accountPath(account.account_id), 'DELETE'); await workspace(); notice('Upstream account deleted.'); }), 'danger-text');
      remove.disabled = assigned.length > 0; remove.title = assigned.length ? 'Reassign this account’s projects before deleting' : ''; actions.append(remove);
    } else actions.append(element('span', 'Server configured', 'hint'));
    const label = active ? 'Login in progress' : auth.state === 'ready' ? 'Connected' : 'Needs connection';
    actions.append(button('Models', () => modelsEditor(account.profile_id)));
    row.append(cell(lines(account.display_name, account.account_id)), cell(badge(account.managed ? 'Relay managed' : 'Server configured')), cell(lines('', auth.last_error ? 'Reconnect required or refresh unavailable' : ''), badge(label, active ? 'warn' : auth.state === 'ready' ? 'good' : 'warn')), cell(time(auth.expires_at)), cell(lines(number(assigned.length), assigned.map(project => project.project_id).join(', '))), cell(actions));
    $('account-rows').append(row);
  }
}

function accountEditor(account = null) {
  state.editingAccount = account; $('account-form').reset(); text('account-form-error', '');
  text('account-dialog-title', account ? 'Rename upstream account' : 'Add upstream account');
  $('account-id').disabled = !!account; $('account-id').value = account?.account_id || ''; $('account-name').value = account?.display_name || '';
  $('account-provider').value = account?.provider || 'anthropic'; $('account-provider').disabled = !!account;
  $('account-dialog').showModal();
}

async function saveAccount() {
  const id = $('account-id').value.trim(); const display_name = $('account-name').value.trim();
  await api(state.editingAccount ? accountPath(id) : '/upstream-accounts', state.editingAccount ? 'PUT' : 'POST', state.editingAccount ? { display_name } : { account_id: id, display_name, provider: $('account-provider').value });
  const added = !state.editingAccount; $('account-dialog').close(); await workspace();
  if (added && $('account-provider').value === 'enmaas') tokenAccountEditor(state.accounts.find(account => account.account_id === id));
  else if (added && state.loginAvailable) await connectAccount(state.accounts.find(account => account.account_id === id));
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

async function projectEditor(project = null) {
  state.editingProject = project; $('project-form').reset(); text('project-form-error', '');
  text('project-dialog-title', project ? 'Edit project' : 'Add project');
  $('project-id').value = project?.project_id || ''; $('project-id').disabled = !!project;
  $('project-name').value = project?.display_name || '';
  await loadRoutingInventories();
  const routing = projectRouting(project); profileOptions($('project-profile'), routing.default_upstream, true);
  $('project-routes').replaceChildren();
  for (const [model, profile] of Object.entries(routing.model_overrides)) addRouteRow(model, profile);
  $('project-enabled').checked = project?.enabled ?? true;
  $('project-labels').value = project && Object.keys(project.labels || {}).length ? JSON.stringify(project.labels, null, 2) : '';
  $('project-model-mode').value = project?.model_policy?.allowed == null ? 'inherit' : 'allowlist';
  await projectModelOptions(project?.model_policy);
  await projectModelPreview();
  $('project-dialog').showModal();
}

async function saveProject() {
  const labels = $('project-labels').value.trim() ? JSON.parse($('project-labels').value) : {};
  if (!labels || Array.isArray(labels) || typeof labels !== 'object' || Object.values(labels).some(value => typeof value !== 'string')) throw new Error('Labels must be a JSON object with string values.');
  const project = { project_id: $('project-id').value.trim(), display_name: $('project-name').value.trim(), upstream_profile: $('project-profile').value || 'default', routing: draftRouting(), enabled: $('project-enabled').checked, labels, model_policy: { allowed: $('project-model-mode').value === 'inherit' ? null : selectedModels('project-model-allowed'), blocked: selectedModels('project-model-blocked') } };
  await api(state.editingProject ? `/projects/${encodeURIComponent(project.project_id)}` : '/projects', state.editingProject ? 'PUT' : 'POST', project);
  $('project-dialog').close(); await workspace(); notice('Project saved.');
}

async function loadTokens() {
  const projectID = $('token-project').value; const load = ++state.tokenLoad;
  const project = state.projects.find(project => project.project_id === projectID);
  $('issue-token').disabled = !project;
  text('token-project-note', project ? `${project.project_id} · ${routingSummary(project)}${project.enabled ? '' : ' · project disabled'}` : 'Create a project to issue credentials.');
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

function projectProfiles(project) {
  return routingProfiles(projectRouting(project)).map(id => state.profiles.find(p => p.profile_id === id)).filter(Boolean);
}
function providerName(project) {
  return projectProfiles(project).map(profile => profile.provider === 'enmaas' ? 'EnMaaS' : profile.backend === 'mock' ? 'Development simulator' : 'Anthropic').filter((name, i, all) => all.indexOf(name) === i).join(' + ') || 'No available upstream';
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

async function tokenEditor(token = null) {
  state.editingToken = token; state.tokenFormProject = $('token-project').value;
  $('token-form').reset(); text('token-form-error', '');
  text('token-dialog-title', token ? 'Edit token grants' : 'Issue token');
  text('token-target', `Project: ${state.tokenFormProject}`); $('token-issue-fields').hidden = !!token;
  $('token-client').value = token?.client_id || '';

  const project = state.projects.find(item => item.project_id === state.tokenFormProject);
  $('token-backend').value = providerName(project);
  if (token) {
    $('token-catalog').checked = token.catalog_models || token.vertex_catalog_models;
    $('token-name').required = false;
    if (token.mock_adc && !token.exchange_only) text('token-auth-flow', 'This legacy key allows both direct API access and Google token exchange. Its existing authentication permissions are preserved.');
    else tokenAuthFlow(token.exchange_only);
  } else { $('token-catalog').checked = true; tokenTypeChanged(); }
  text('token-form-note', token ? 'Editing replaces grants. The token value, type, and expiration are preserved.' : 'Credentials are local to the relay. Upstream OAuth stays on the server.');
  await tokenModelOptions(project, token);
  text('token-submit', token ? 'Save grants' : 'Issue token'); $('token-dialog').showModal();
}

async function saveToken() {
  const models = selectedModels('token-models');
  const project = state.projects.find(item => item.project_id === state.tokenFormProject);
  const profiles = projectProfiles(project);
  if (!profiles.length) throw new Error('The project’s upstream profile is unavailable.');
  const old = state.editingToken;
  const vertex = old ? !!old.exchange_only : $('token-type').value === 'vertex';
  const catalog = $('token-catalog').checked;

  const hadCatalog = old && (old.catalog_models || old.vertex_catalog_models);
  const data = {
    client_id: $('token-client').value.trim(), models, blocked_models: selectedModels('token-model-blocked'),
    backends: old ? old.backends : [...new Set(profiles.map(p => p.backend))],
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

let accountTokenTarget = null;
function tokenAccountEditor(account) {
  accountTokenTarget = account.account_id; $('api-token-value').value = ''; text('api-token-error', '');
  text('api-token-account', account.display_name); $('api-token-dialog').showModal();
}
$('api-token-dialog').addEventListener('close', () => { $('api-token-value').value = ''; accountTokenTarget = null; });
$('api-token-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, async () => {
  const token = $('api-token-value').value.trim(); $('api-token-value').value = '';
  await api(accountPath(accountTokenTarget) + '/token', 'PUT', { token });
  $('api-token-dialog').close(); await workspace(); notice('EnMaaS token saved.');
}, 'api-token-error'); });
const inventoryPath = profile => `/upstream-profiles/${encodeURIComponent(profile)}/models`;
let modelEditor = null;
function collectInventory() {
  if (!modelEditor) return;
  for (const row of $('models-rows').children) {
    const entry = modelEditor.inventory.models.find(m => m.id === row.dataset.id);
    const field = name => row.querySelector(`[data-field="${name}"]`);
    const enabled = field('enabled'), name = field('display_name'), input = field('max_input_tokens'), output = field('max_tokens'), manual = field('manual');
    entry.enabled = enabled.checked; entry.display_name = name.value.trim() || null;
    entry.max_input_tokens = input.value ? Number(input.value) : null;
    entry.max_tokens = output.value ? Number(output.value) : null; entry.manual = manual.checked;
  }
}
function renderInventory() {
  const { profile, inventory } = modelEditor; text('models-title', `Models · ${profile}`);
  text('models-status', `${inventory.configured ? 'Explicit policy' : 'Legacy inherited access'} · version ${inventory.version} · discovery ${inventory.refreshed_at ? time(inventory.refreshed_at) : 'not refreshed'}${inventory.refresh_error ? ' · ' + inventory.refresh_error : ''}`);
  $('models-rows').replaceChildren();
  for (const entry of inventory.models) {
    const row = element('tr'); row.dataset.id = entry.id;
    const enabled = element('input'); enabled.type = 'checkbox'; enabled.dataset.field = 'enabled'; enabled.checked = entry.enabled; enabled.setAttribute('aria-label', `Enable ${entry.id}`);
    const name = element('input'); name.type = 'text'; name.dataset.field = 'display_name'; name.value = entry.display_name || ''; name.placeholder = entry.discovered?.display_name || entry.id; name.maxLength = 200; name.setAttribute('aria-label', `Display name for ${entry.id}`);
    const limit = (field, caption, override, discovered) => { const label = element('label', caption); const input = element('input'); input.type = 'number'; input.dataset.field = field; input.min = '1'; input.step = '1'; input.value = override ?? ''; input.placeholder = discovered == null ? 'Unknown' : String(discovered); input.setAttribute('aria-label', `${caption} token limit for ${entry.id}`); label.append(input); return label; };
    const limits = element('div', undefined, 'model-limits'); limits.append(limit('max_input_tokens', 'Input', entry.max_input_tokens, entry.discovered?.max_input_tokens), limit('max_tokens', 'Output', entry.max_tokens, entry.discovered?.max_tokens));
    const manual = element('input'); manual.type = 'checkbox'; manual.dataset.field = 'manual'; manual.checked = entry.manual; manual.setAttribute('aria-label', `Manual entry for ${entry.id}`);
    const access = element('div', undefined, 'model-access'); const enabledLabel = element('label'), manualLabel = element('label'); enabledLabel.append(enabled, 'Enabled'); manualLabel.append(manual, 'Manual'); access.append(enabledLabel, manualLabel);
    const model = lines(entry.id, entry.discovered ? 'Discovered' : entry.manual ? 'Manual' : 'Unavailable', true); model.classList.add('model-identity');
    row.append(cell(model), cell(name), cell(limits), cell(access));
    $('models-rows').append(row);
  }
}
async function modelsEditor(profile) {
  modelEditor = { profile, inventory: await api(inventoryPath(profile)) }; text('models-error', ''); renderInventory(); $('models-dialog').showModal();
}
$('models-dialog').addEventListener('close', () => { modelEditor = null; $('manual-model-id').value = ''; });
$('models-reload').addEventListener('click', event => task(event.target, async () => { modelEditor.inventory = await api(inventoryPath(modelEditor.profile)); renderInventory(); }, 'models-error'));
$('models-refresh').addEventListener('click', event => task(event.target, async () => {
  collectInventory(); const draft = modelEditor.inventory;
  const refreshed = await api(inventoryPath(modelEditor.profile) + '/refresh', 'POST');
  if (refreshed.version !== draft.version + 1) throw new Error('Policy changed in another editor. Reload saved policy before continuing.');
  // Keep unsaved editor overrides only when no concurrent edit intervened.
  for (const entry of refreshed.models) { const old = draft.models.find(m => m.id === entry.id); if (old) Object.assign(entry, { enabled: old.enabled, manual: old.manual, display_name: old.display_name, max_input_tokens: old.max_input_tokens, max_tokens: old.max_tokens }); }
  for (const old of draft.models) if (!refreshed.models.some(m => m.id === old.id)) refreshed.models.push(old);
  modelEditor.inventory = refreshed; renderInventory();
}, 'models-error'));
$('models-save').addEventListener('click', event => task(event.target, async () => {
  collectInventory(); modelEditor.inventory = await api(inventoryPath(modelEditor.profile), 'PUT', modelEditor.inventory); renderInventory(); notice('Model policy saved. Existing credentials follow it on their next request.');
}, 'models-error'));
$('manual-model-form').addEventListener('submit', event => { event.preventDefault(); collectInventory(); const id = $('manual-model-id').value.trim();
  if (modelEditor.inventory.models.some(m => m.id === id)) { text('models-error', 'This model is already registered.'); return; }
  modelEditor.inventory.models.push({ id, enabled: false, manual: true, discovered: null, display_name: null, max_input_tokens: null, max_tokens: null }); $('manual-model-id').value = ''; renderInventory();
});
const selectedModels = id => [...$(id).selectedOptions].map(o => o.value);
function modelOptions(id, inventory, selected = []) {
  const entries = new Map(inventory.models.map(m => [m.id, m])); for (const key of selected) if (!entries.has(key)) entries.set(key, { id: key });
  $(id).replaceChildren();
  for (const entry of entries.values()) { const option = element('option', `${entry.id}${inventory.configured && !entry.enabled ? ' · upstream disabled' : ''}`); option.value = entry.id; option.selected = selected.includes(entry.id); $(id).append(option); }
}
const routingInventories = new Map();
const projectRouting = project => project?.routing || { default_upstream: project?.upstream_profile || 'default', model_overrides: {} };
const routingProfiles = config => [...new Set([config.default_upstream, ...Object.values(config.model_overrides)].filter(Boolean))];
function routingSummary(project) {
  const config = projectRouting(project); const count = Object.keys(config.model_overrides).length;
  return config.default_upstream ? `Default: ${config.default_upstream}` : `Explicit routes only · ${count}`;
}
function routingDetail(project) {
  const config = projectRouting(project); const overrides = Object.entries(config.model_overrides);
  return overrides.length ? overrides.map(([model, profile]) => `${model} → ${profile}`).join('; ') : config.default_upstream ? 'All models use the default account' : 'No models routed';
}
function profileLabel(id) {
  const account = state.accounts.find(a => a.profile_id === id);
  const profile = state.profiles.find(p => p.profile_id === id);
  return `${account?.display_name || id} · ${profile?.provider || profile?.backend || 'unavailable'}${account?.display_name ? ` (${id})` : ''}`;
}
function profileOptions(select, selected, allowNone = false, eligible = null) {
  select.replaceChildren();
  if (allowNone) { const option = element('option', 'No default — explicit routes only'); option.value = ''; select.append(option); }
  const ids = eligible || state.profiles.map(p => p.profile_id);
  if (selected && !ids.includes(selected)) ids.push(selected);
  for (const id of ids) {
    const option = element('option', profileLabel(id) + (!state.profiles.some(p => p.profile_id === id) ? ' · unavailable' : eligible && !routeEligible(id, select.dataset.model) ? ' · model disabled' : ''));
    option.value = id; select.append(option);
  }
  if (!ids.length && !allowNone) { const option = element('option', 'No account enables this model'); option.value = ''; select.append(option); }
  select.value = selected || ''; if (!select.value && !allowNone && select.options.length) select.selectedIndex = 0;
}
function routeEligible(profile, model) {
  const inventory = routingInventories.get(profile);
  return inventory && !inventory.unavailable && (!inventory.configured || inventory.models.some(m => m.id === model && m.enabled && (m.manual || m.discovered) && m.discovered?.lifecycle !== 'retired'));
}
async function loadRoutingInventories() {
  const entries = await Promise.all(state.profiles.map(async p => {
    try {
      const inventory = await api(inventoryPath(p.profile_id));
      if (!inventory.configured) {
        const catalog = await api(inventoryPath(p.profile_id) + '/available');
        inventory.models = catalog.items.map(m => ({ id: m.id, enabled: true, discovered: m }));
      }
      return [p.profile_id, inventory];
    } catch (_) { return [p.profile_id, { configured: true, models: [], unavailable: true }]; }
  }));
  routingInventories.clear(); for (const [id, inventory] of entries) routingInventories.set(id, inventory);
  $('route-model-options').replaceChildren();
  for (const id of [...new Set(entries.flatMap(([, i]) => i.models.map(m => m.id)))].sort()) { const option = element('option'); option.value = id; $('route-model-options').append(option); }
}
function addRouteRow(model = '', profile = '') {
  const row = element('div', undefined, 'route-row');
  const modelLabel = element('label', 'Model ID'); const input = element('input'); input.className = 'route-model'; input.value = model; input.required = true; input.maxLength = 200; input.setAttribute('list', 'route-model-options'); input.placeholder = 'Select or enter an exact model ID'; modelLabel.append(input);
  const profileLabelElement = element('label', 'Upstream account'); const select = element('select'); select.className = 'route-profile'; select.required = true; profileLabelElement.append(select);
  function choices() { select.dataset.model = input.value.trim(); profileOptions(select, select.value || profile, false, state.profiles.filter(p => routeEligible(p.profile_id, input.value.trim())).map(p => p.profile_id)); }
  choices();
  const remove = button('Remove', async () => { row.remove(); await routingChanged(); }); remove.type = 'button';
  row.append(modelLabel, profileLabelElement, remove); $('project-routes').append(row);
  input.addEventListener('change', () => task(null, async () => { choices(); await routingChanged(); }, 'project-form-error'));
  select.addEventListener('change', () => task(null, routingChanged, 'project-form-error'));
}
function draftRouting() {
  const model_overrides = {};
  for (const row of $('project-routes').children) {
    const model = row.querySelector('.route-model').value.trim(); const profile = row.querySelector('.route-profile').value;
    if (!model || !profile) throw new Error('Each route needs a model ID and an available account.');
    if (Object.hasOwn(model_overrides, model)) throw new Error(`Duplicate route for ${model}.`);
    model_overrides[model] = profile;
  }
  return { default_upstream: $('project-profile').value || null, model_overrides };
}
function combinedInventory(config) {
  const models = new Map();
  for (const profile of routingProfiles(config)) for (const entry of routingInventories.get(profile)?.models || []) {
    const destination = config.model_overrides[entry.id] || config.default_upstream;
    if (destination === profile) models.set(entry.id, entry);
  }
  for (const id of Object.keys(config.model_overrides)) if (!models.has(id)) models.set(id, { id });
  return { configured: false, models: [...models.values()] };
}
async function projectModelOptions(policy = null) {
  const inventory = combinedInventory(draftRouting());
  modelOptions('project-model-allowed', inventory, policy?.allowed || []); modelOptions('project-model-blocked', inventory, policy?.blocked || []);
  text('project-model-note', 'Blocks win. An empty explicit allowlist denies all. Routing does not change these permissions. Changes affect every project token.');
}
async function routingChanged() {
  await projectModelOptions({ allowed: selectedModels('project-model-allowed'), blocked: selectedModels('project-model-blocked') });
  await projectModelPreview();
}
$('project-profile').addEventListener('change', () => task(null, routingChanged, 'project-form-error'));
$('add-model-route').addEventListener('click', () => addRouteRow());
async function tokenModelOptions(project, token) {
  await loadRoutingInventories(); const inventory = combinedInventory(projectRouting(project));
  modelOptions('token-models', inventory, token?.models || []); modelOptions('token-model-blocked', inventory, token?.blocked_models || []);
  text('token-model-note', 'Blocks win. Tokens inherit project routing and cannot expand its model ceiling. Existing unavailable grants remain visible.');
  await tokenModelPreview();
}
let previewGeneration = 0;
async function tokenModelPreview() {
  const generation = ++previewGeneration;
  const project = state.projects.find(p => p.project_id === state.tokenFormProject); if (!project) return;
  const old = state.editingToken; const vertex = old ? !!old.exchange_only : $('token-type').value === 'vertex';
  const catalog = $('token-catalog').checked; const hadCatalog = old && (old.catalog_models || old.vertex_catalog_models);
  const result = await api('/model-access', 'POST', { routing: projectRouting(project), backends: old?.backends || [...new Set(projectProfiles(project).map(p => p.backend))], project_policy: project.model_policy || {}, models: selectedModels('token-models'), blocked_models: selectedModels('token-model-blocked'),
    catalog_models: catalog && (hadCatalog ? old.catalog_models : !vertex), vertex_catalog_models: catalog && (hadCatalog ? old.vertex_catalog_models : vertex) });
  if (generation !== previewGeneration) return;
  text('token-model-effective', result.items.map(m => `${m.id} → ${m.upstream_profile || 'unrouted'}: ${m.reason || [m.messages ? 'Messages allowed' : null, m.vertex ? 'Vertex allowed' : null].filter(Boolean).join(', ')}`).join('\n'));
}
for (const id of ['token-models', 'token-model-blocked', 'token-catalog', 'token-type']) $(id).addEventListener('change', () => task(null, tokenModelPreview, 'token-form-error'));

$('add-account').addEventListener('click', () => accountEditor());
$('account-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, saveAccount, 'account-form-error'); });
$('login-form').addEventListener('submit', event => { event.preventDefault(); task(event.submitter, submitLoginCode, 'login-error'); });
$('copy-login-url').addEventListener('click', event => task(event.currentTarget, async () => {
  if (navigator.clipboard?.writeText) { await navigator.clipboard.writeText($('login-url').value); text('login-state', 'Link copied. Open it in your authenticated browser.'); }
  else { $('login-url').focus(); $('login-url').select(); }
}, 'login-error'));
$('login-dialog').addEventListener('close', () => clearLogin(true));
$('add-project').addEventListener('click', event => task(event.currentTarget, () => projectEditor()));
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

let projectPreviewGeneration = 0;
async function projectModelPreview() {
  const generation = ++projectPreviewGeneration;
  const policy = { allowed: $('project-model-mode').value === 'inherit' ? null : selectedModels('project-model-allowed'), blocked: selectedModels('project-model-blocked') };
  const result = await api('/model-access', 'POST', { routing: draftRouting(), project_policy: policy, catalog_models: true, vertex_catalog_models: true });
  if (generation !== projectPreviewGeneration) return;
  const config = draftRouting(); text('project-routing-note', config.default_upstream ? `Default: ${profileLabel(config.default_upstream)} · ${Object.keys(config.model_overrides).length} overrides` : `Explicit routes only · ${Object.keys(config.model_overrides).length} routes`);
  text('project-model-effective', result.items.map(m => `${m.id} → ${m.upstream_profile || 'unrouted'}: ${m.reason || 'policy permits'}`).join('\n'));
}
for (const id of ['project-model-mode', 'project-model-allowed', 'project-model-blocked']) $(id).addEventListener('change', () => task(null, projectModelPreview, 'project-form-error'));
