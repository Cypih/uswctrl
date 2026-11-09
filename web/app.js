// uswctl web page script: login, state refresh, port and switch-wide edits, actions.
'use strict';

const REFRESH_MS = 5000;
const TOKEN_KEY = 'uswctl.token';
const STP_VERSIONS = ['stp', 'rstp'];
const STP_PRIORITY_STEP = 4096;
const STP_PRIORITY_MAX = 61440;
const BPDU_ACTIONS = ['flooding', 'filtering'];
const EDGE_MODES = ['auto', 'enabled', 'disabled'];
const VLAN_ID_MAX = 4094;
const EVENTS_SHOWN = 12;
const LOCATE_DURATION_S = 300;
const LOCATE_KEY = 'locate';
const MODEL_NAMES = {
  USPM16P: 'USW Pro Max 16 PoE',
  USWED77: 'USW Pro XG 10 PoE',
  USPXG10P: 'USW Pro XG 10 PoE',
};
const ACTION_LABELS = {
  link_down: 'Link down',
  link_up: 'Link up',
  poe_off: 'PoE off',
  poe_on: 'PoE on',
  poe_cycle: 'PoE cycle',
};
const PORT_FIELD_LABELS = {
  name: 'name',
  native_vlan: 'native VLAN',
  tagged_vlans: 'tagged VLANs',
  poe: 'PoE',
  stp: 'STP',
  edge: 'edge',
  bpdu_guard: 'BPDU guard',
  bpdu_filter: 'BPDU filter',
  enabled: 'port enabled',
};
const GLOBAL_FIELD_LABELS = {
  stp_version: 'STP version',
  stp_priority: 'bridge priority',
  bpdu_action: 'BPDU action',
  vlans: 'VLAN list',
};

const app = {
  token: null,
  stopped: true,
  timer: null,
  switches: [],
  current: null,
  device: null,
  reservations: [],
  updated_at: null,
  refresh_error: null,
  port_rows: new Map(),
  port_signatures: new Map(),
  editor: null,
  global_edit: null,
  dialog_busy: false,
  render_error: null,
  // Keys "switch/port/action" of the action requests in flight. A refresh rebuilds the
  // row, so the busy state lives here and not on the button node.
  actions_running: new Set(),
  // Names of the switches whose locate stop is in flight.
  locate_stopping: new Set(),
  // The nodes of the locate banner. The banner is built once, and a refresh changes only
  // its texts, so that focus, hover, the blink and the live region stay as they are.
  locate_banner: null,
  // Only the newest refresh may write its result, so that a slow answer cannot replace
  // the state that a later refresh read after an action.
  refresh_seq: 0,
};

// ---------- DOM ----------

function node_build(tag, props, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'class') node.className = value;
    else if (key === 'text') node.textContent = value;
    else if (key.startsWith('on')) node.addEventListener(key.slice(2), value);
    else if (key in node && typeof value !== 'string') node[key] = value;
    else node.setAttribute(key, value === true ? '' : value);
  }
  node_append(node, children);
  return node;
}

function node_append(node, children) {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    if (Array.isArray(child)) node_append(node, child);
    else node.append(child instanceof Node ? child : String(child));
  }
}

function icon_build(name) {
  const paths = {
    lock: 'M5 7V5a3 3 0 0 1 6 0v2h.5A1.5 1.5 0 0 1 13 8.5v5a1.5 1.5 0 0 1-1.5 1.5h-7A1.5 1.5 0 0 1 3 13.5v-5A1.5 1.5 0 0 1 4.5 7H5Zm1.5 0h3V5a1.5 1.5 0 0 0-3 0v2Z',
    close: 'M4.3 4.3a1 1 0 0 1 1.4 0L8 6.6l2.3-2.3a1 1 0 1 1 1.4 1.4L9.4 8l2.3 2.3a1 1 0 0 1-1.4 1.4L8 9.4l-2.3 2.3a1 1 0 0 1-1.4-1.4L6.6 8 4.3 5.7a1 1 0 0 1 0-1.4Z',
    chevron: 'M6 4l4 4-4 4',
  };
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 16 16');
  svg.setAttribute('aria-hidden', 'true');
  svg.classList.add('icon', 'icon-' + name);
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
  path.setAttribute('d', paths[name]);
  svg.append(path);
  return svg;
}

function focus_inside_check(node) {
  return node !== null && node.contains(document.activeElement);
}

// Rebuilds a section only when its data changed, so that a refresh keeps hover, focus
// and scroll position in sections that did not change.
function section_update(node, data, build) {
  const signature = JSON.stringify(data);
  if (node.dataset.signature === signature) return;
  node.dataset.signature = signature;
  // `replaceChildren` would turn a null child into the text "null".
  node.replaceChildren();
  node_append(node, [build()]);
}

// ---------- formatting ----------

function time_parse(value) {
  if (!value) return null;
  return new Date(value.secs_since_epoch * 1000 + Math.floor(value.nanos_since_epoch / 1e6));
}

function clock_format(date) {
  const now = new Date();
  const time = date.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (date.toDateString() === now.toDateString()) return time;
  return date.toLocaleDateString([], { month: 'short', day: 'numeric' }) + ' ' + time;
}

function duration_format(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return `${d} d ${h} h`;
  if (h > 0) return `${h} h ${m} min`;
  if (m > 0) return `${m} min`;
  return `${s} s`;
}

function age_format(date) {
  return duration_format((Date.now() - date.getTime()) / 1000) + ' ago';
}

function speed_format(mbps) {
  if (mbps >= 1000) return (mbps / 1000) + 'G';
  return mbps + 'M';
}

// Contiguous ids show as a range: 2 3 4 5 9 -> "2-5, 9".
function vlans_format(ids) {
  const sorted = [...ids].sort((a, b) => a - b);
  const parts = [];
  let start = null;
  let prev = null;
  for (const id of sorted) {
    if (start !== null && id === prev + 1) {
      prev = id;
      continue;
    }
    if (start !== null) parts.push(start === prev ? `${start}` : `${start}-${prev}`);
    start = id;
    prev = id;
  }
  if (start !== null) parts.push(start === prev ? `${start}` : `${start}-${prev}`);
  return parts.join(', ');
}

function model_name_get(code) {
  return MODEL_NAMES[code] || code;
}

function port_label_get(port) {
  return `port ${port.idx} (${port.cli_name})`;
}

function sentence_capitalize(text) {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

function port_poe_check(port) {
  return port.live.poe_class !== null || port.live.poe_power_w !== null;
}

function event_describe(event) {
  const kind = event.kind;
  if (kind === 'Reboot') {
    return event.requested
      ? { text: 'Reboot on request', tone: '' }
      : { text: 'Unexpected reboot', tone: 'warn' };
  }
  if (kind === 'CfgChangedExternally') {
    return { text: 'system.cfg changed outside uswctl', tone: 'warn' };
  }
  if (kind === 'OverlayRepaired') return { text: 'Overlay applied again after a boot', tone: '' };
  if (kind === 'Unreachable') return { text: 'Switch not reachable', tone: 'danger' };
  if (kind && kind.InformConnected !== undefined) {
    return { text: `Inform connected to ${kind.InformConnected || '(no address)'}`, tone: '' };
  }
  if (kind && kind.OfflineLogAdded !== undefined) {
    return { text: `Offline log added: ${kind.OfflineLogAdded}`, tone: '' };
  }
  return { text: JSON.stringify(kind), tone: '' };
}

// ---------- API ----------

class ApiError extends Error {
  constructor(status, kind, message) {
    super(message);
    this.status = status;
    this.kind = kind;
  }
}

async function api_call(method, path, body) {
  const headers = { Authorization: 'Bearer ' + app.token };
  const init = { method, headers };
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  let response;
  try {
    response = await fetch(path, init);
  } catch (e) {
    throw new ApiError(0, 'network', `The daemon did not answer (${e.message}).`);
  }
  const text = await response.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch (e) {
    data = null;
  }
  if (response.status === 401) {
    session_end('The daemon did not accept the token. Enter it again.');
    throw new ApiError(401, 'unauthorized', 'The daemon did not accept the token.');
  }
  if (!response.ok) {
    const kind = (data && data.error) || `http_${response.status}`;
    const message = (data && data.message) || text || response.statusText;
    throw new ApiError(response.status, kind, message);
  }
  return data;
}

function switch_path_build(name) {
  return '/api/switches/' + encodeURIComponent(name);
}

// ---------- login ----------

function token_load() {
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch (e) {
    return null;
  }
}

function token_store(token) {
  try {
    if (token) localStorage.setItem(TOKEN_KEY, token);
    else localStorage.removeItem(TOKEN_KEY);
  } catch (e) {
    // The page still works for this visit without storage; it asks again on the next load.
  }
}

function login_show(message) {
  document.getElementById('connect').hidden = true;
  document.getElementById('main').hidden = true;
  document.getElementById('signout').hidden = true;
  const tabs = document.getElementById('tabs');
  tabs.replaceChildren();
  delete tabs.dataset.signature;
  document.getElementById('sync').textContent = '';
  document.getElementById('login').hidden = false;
  document.getElementById('login-error').textContent = message || '';
  const input = document.getElementById('login-token');
  input.value = '';
  input.focus();
}

function session_end(message) {
  app.stopped = true;
  clearTimeout(app.timer);
  app.token = null;
  token_store(null);
  if (app.dialog_busy || document.getElementById('dialog').open) dialog_close();
  editor_drop();
  app.global_edit = null;
  login_show(message);
}

// The page must refuse the agent token (spec section 10), so the role decides before the
// token is kept.
async function session_start(token, from_storage) {
  app.token = token;
  let who;
  try {
    who = await api_call('GET', '/api/whoami');
  } catch (e) {
    if (e.status === 401) return;
    // A daemon restart must not throw the user back to the token card: the stored token
    // is still good, so the page waits for the daemon.
    if (from_storage) {
      connect_wait_show(e.message);
      app.timer = setTimeout(() => session_start(token, true), REFRESH_MS);
      return;
    }
    login_show(`The daemon did not answer: ${e.message}`);
    return;
  }
  document.getElementById('connect').hidden = true;
  if (!who || who.role !== 'user') {
    app.token = null;
    token_store(null);
    login_show(
      from_storage
        ? 'The stored token is not the user token. Enter the user token.'
        : 'This is the agent token. The page needs the user token.',
    );
    return;
  }
  token_store(token);
  document.getElementById('login').hidden = true;
  document.getElementById('main').hidden = false;
  document.getElementById('signout').hidden = false;
  app.stopped = false;
  await state_refresh();
}

function connect_wait_show(message) {
  document.getElementById('login').hidden = true;
  document.getElementById('main').hidden = true;
  const node = document.getElementById('connect');
  node.hidden = false;
  document.getElementById('connect-detail').textContent = message;
}

// ---------- refresh ----------

function refresh_schedule() {
  clearTimeout(app.timer);
  if (app.stopped) return;
  app.timer = setTimeout(state_refresh, REFRESH_MS);
}

async function state_refresh() {
  if (app.stopped) return;
  clearTimeout(app.timer);
  const seq = ++app.refresh_seq;
  try {
    const [switches, reservations] = await Promise.all([
      api_call('GET', '/api/switches'),
      api_call('GET', '/api/reservations'),
    ]);
    let current = app.current;
    if (!switches.some((sw) => sw.name === current)) {
      const wanted = decodeURIComponent(location.hash.slice(1));
      const known = switches.find((sw) => sw.name === wanted);
      current = known ? known.name : (switches[0] || {}).name || null;
    }
    const device = current ? await api_call('GET', switch_path_build(current)) : null;
    if (seq !== app.refresh_seq) return;
    app.switches = switches;
    app.reservations = reservations;
    app.current = current;
    app.device = device;
    app.updated_at = new Date();
    app.refresh_error = null;
  } catch (e) {
    if (e.status === 401 || seq !== app.refresh_seq) return;
    app.refresh_error = e.message;
  }
  // One bad render must not stop the refresh: the next answer can render again.
  try {
    page_render();
    if (app.render_error) {
      app.render_error = null;
      sync_render();
    }
  } catch (e) {
    app.render_error = e.message;
    console.error(e);
    sync_render();
  } finally {
    refresh_schedule();
  }
}

function sync_render() {
  const node = document.getElementById('sync');
  if (app.refresh_error) {
    node.className = 'sync sync-error';
    node.textContent = `Refresh failed: ${app.refresh_error}`;
    return;
  }
  if (app.render_error) {
    node.className = 'sync sync-error';
    node.textContent = `Display failed: ${app.render_error}`;
    return;
  }
  node.className = 'sync';
  node.textContent = app.updated_at ? `Updated ${app.updated_at.toLocaleTimeString()}` : '';
  if (app.updated_at) node.title = app.updated_at.toLocaleString();
}

// ---------- page ----------

function page_render() {
  sync_render();
  tabs_render();
  if (!app.device) return;
  summary_render();
  locate_render();
  ports_render();
  global_render();
  events_render();
  reservations_render();
}

function tabs_render() {
  const node = document.getElementById('tabs');
  const data = { switches: app.switches.map((sw) => [sw.name, sw.model, sw.firmware_tested]), current: app.current };
  section_update(node, data, () =>
    app.switches.map((sw) =>
      node_build(
        'button',
        {
          type: 'button',
          role: 'tab',
          class: 'tab',
          'aria-selected': sw.name === app.current ? 'true' : 'false',
          onclick: () => switch_select(sw.name),
        },
        node_build('span', { class: 'tab-name', text: sw.name }),
        node_build('span', { class: 'tab-model', text: model_name_get(sw.model) }),
        sw.firmware_tested ? null : node_build('span', { class: 'dot dot-warn', title: 'Firmware not tested' }),
      ),
    ),
  );
}

async function switch_select(name) {
  if (name === app.current) return;
  if (!(await edits_discard_confirm())) return;
  editor_drop();
  app.global_edit = null;
  app.current = name;
  history.replaceState(null, '', '#' + encodeURIComponent(name));
  app.device = null;
  app.port_rows.clear();
  app.port_signatures.clear();
  document.querySelector('#ports tbody').replaceChildren();
  await state_refresh();
}

function summary_render() {
  const device = app.device;
  const sw = app.switches.find((s) => s.name === app.current) || {};
  const overlay = time_parse(device.overlay_last_run);
  const data = {
    name: app.current,
    model: sw.model,
    firmware: device.firmware,
    tested: device.firmware_tested,
    uptime: Math.floor(device.uptime_s / 60),
    overlay: overlay ? Math.floor(overlay.getTime() / 60000) : null,
    overlay_age: overlay ? Math.floor((Date.now() - overlay.getTime()) / 60000) : null,
    mem: device.health.mem_avail_kb,
    mem_series: device.mem_series.length,
    inform: device.health.inform_status,
    lock: device.health.lock,
    failure: device.last_failure,
  };
  section_update(document.getElementById('summary'), data, () => {
    const facts = [
      fact_build(
        'Firmware',
        node_build('span', { class: 'mono', text: device.firmware || 'unknown' }),
        device.firmware_tested
          ? null
          : node_build('span', {
              class: 'badge badge-warn',
              text: 'not tested',
              title: 'The daemon refuses settings changes on this firmware until it is tested. Link and PoE actions still work.',
            }),
      ),
      fact_build('Uptime', duration_format(device.uptime_s)),
      fact_build(
        'Overlay last run',
        overlay ? node_build('span', { title: overlay.toLocaleString(), text: age_format(overlay) }) : node_build('span', { class: 'muted', text: 'not since the daemon started' }),
      ),
      fact_build('Memory available', `${Math.round(device.health.mem_avail_kb / 1024)} MiB`, sparkline_build(device.mem_series)),
      fact_build('Inform', node_build('span', { class: 'ellipsis', title: device.health.inform_status, text: device.health.inform_status || 'unknown' })),
    ];
    if (device.health.lock) {
      facts.push(
        fact_build(
          'Agent lock',
          node_build('span', { class: 'badge badge-warn', text: 'held' }),
          node_build('span', { class: 'mono ellipsis', title: device.health.lock, text: device.health.lock }),
        ),
      );
    }
    return [
      node_build(
        'div',
        { class: 'summary-head' },
        node_build('h1', { class: 'summary-title' }, app.current, node_build('span', { class: 'summary-model', text: model_name_get(sw.model) })),
        node_build(
          'div',
          { class: 'summary-actions' },
          node_build('button', { type: 'button', class: 'btn', text: 'Backups', onclick: backups_show }),
          node_build('button', { type: 'button', class: 'btn btn-danger-quiet', text: 'Reboot', onclick: reboot_run }),
        ),
      ),
      node_build('dl', { class: 'facts' }, facts),
      device.last_failure
        ? node_build(
            'p',
            { class: 'failure' },
            node_build('span', { class: 'dot dot-danger' }),
            node_build('span', { class: 'failure-label', text: 'Last failure' }),
            node_build('span', { class: 'failure-text', title: device.last_failure, text: device.last_failure }),
          )
        : null,
    ];
  });
}

function fact_build(label, ...value) {
  return node_build('div', { class: 'fact' }, node_build('dt', { text: label }), node_build('dd', null, value));
}

function sparkline_build(series) {
  if (series.length < 2) return null;
  const values = series.map((point) => point[1]);
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const w = 64;
  const h = 16;
  const points = values
    .map((v, i) => `${((i / (values.length - 1)) * w).toFixed(1)},${(h - 1 - ((v - min) / span) * (h - 2)).toFixed(1)}`)
    .join(' ');
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', `0 0 ${w} ${h}`);
  svg.setAttribute('class', 'sparkline');
  svg.setAttribute('role', 'img');
  svg.setAttribute('aria-label', `Memory available over ${series.length} samples`);
  const line = document.createElementNS('http://www.w3.org/2000/svg', 'polyline');
  line.setAttribute('points', points);
  svg.append(line);
  return svg;
}

// ---------- port table ----------

function port_reservation_get(idx) {
  return app.reservations.find((r) => r.switch === app.current && r.port === idx) || null;
}

// The user token passes a reservation, so the page warns before it acts on the port of
// another owner. `what` is 'change' or 'action'.
function reservation_warning_get(port, what) {
  const reservation = port_reservation_get(port.idx);
  if (!reservation) return null;
  const until = clock_format(time_parse(reservation.expires));
  return `Reserved by ${reservation.owner} until ${until}. This ${what} can break its test.`;
}

function ports_render() {
  const tbody = document.querySelector('#ports tbody');
  const ports = app.device.ports;
  if (ports.length === 0) {
    app.port_rows.clear();
    app.port_signatures.clear();
    tbody.replaceChildren(node_build('tr', null, node_build('td', { colspan: 10, class: 'empty', text: 'No port data yet. The daemon has not read the switch.' })));
    return;
  }
  const same_shape = app.port_rows.size === ports.length && ports.every((p) => app.port_rows.has(p.idx));
  if (!same_shape) {
    app.port_rows.clear();
    app.port_signatures.clear();
    tbody.replaceChildren();
    for (const port of ports) {
      const row = port_row_build(port);
      app.port_rows.set(port.idx, row);
      app.port_signatures.set(port.idx, port_signature_build(port));
      tbody.append(row);
    }
  } else {
    for (const port of ports) {
      const signature = port_signature_build(port);
      if (app.port_signatures.get(port.idx) === signature) continue;
      const row = port_row_build(port);
      app.port_rows.get(port.idx).replaceWith(row);
      app.port_rows.set(port.idx, row);
      app.port_signatures.set(port.idx, signature);
    }
  }
  editor_sync();
}

function port_signature_build(port) {
  const reservation = port_reservation_get(port.idx);
  const left = reservation ? Math.ceil((time_parse(reservation.expires) - Date.now()) / 60000) : null;
  return JSON.stringify([
    port,
    app.device.transient[port.idx] || [],
    reservation && [reservation.id, reservation.owner, left],
    app.editor && app.editor.idx === port.idx,
    [...Object.keys(ACTION_LABELS), LOCATE_KEY].filter((a) => app.actions_running.has(action_key_build(app.current, port.idx, a))),
    port_located_check(port.idx),
  ]);
}

function port_located_check(idx) {
  const locate = app.device.locate;
  return Boolean(locate) && locate.ports.includes(idx);
}

function port_row_build(port) {
  const transient = app.device.transient[port.idx] || [];
  const open = app.editor !== null && app.editor.idx === port.idx;
  const row = node_build(
    'tr',
    {
      'data-port': String(port.idx),
      class: ['port-row', port.protected ? 'is-protected' : '', open ? 'is-open' : '', port.settings.enabled ? '' : 'is-disabled'].join(' '),
      onclick: (event) => {
        if (event.target.closest('button, a, input, select')) return;
        editor_toggle(port.idx);
      },
    },
    node_build(
      'td',
      { class: 'cell-port' },
      node_build(
        'button',
        {
          type: 'button',
          class: 'port-id',
          'aria-expanded': open ? 'true' : 'false',
          title: open ? 'Close the editor' : 'Edit the port settings',
          onclick: () => editor_toggle(port.idx),
        },
        icon_build('chevron'),
        node_build('span', { class: 'port-num', text: port.idx }),
        node_build('span', { class: 'port-cli', text: port.cli_name }),
      ),
      port.protected ? node_build('span', { class: 'protected-mark', title: 'Protected port: network infrastructure. Changes need a second confirmation.' }, icon_build('lock')) : null,
    ),
    node_build('td', { class: 'cell-name' }, node_build('span', { class: 'ellipsis', title: port.settings.name, text: port.settings.name })),
    node_build('td', null, link_cell_build(port, transient)),
    node_build('td', null, poe_cell_build(port, transient)),
    node_build('td', null, stp_state_build(port.live.stp_state)),
    node_build('td', { class: 'cell-macs' }, macs_cell_build(port)),
    node_build('td', null, vlans_cell_build(port.settings)),
    node_build('td', null, settings_chips_build(port.settings)),
    node_build('td', null, reservation_cell_build(port)),
    node_build('td', { class: 'cell-actions' }, actions_build(port)),
  );
  return row;
}

const LINK_HELD_TITLE = 'Link down by uswctl, not saved. Link up or a switch reboot ends it.';
const POE_HELD_TITLE = 'PoE off by uswctl, not saved. PoE on or a switch reboot ends it.';

// A transient action shows in the cell as a warning colour and on its button as a latched
// state, because a badge does not fit the narrow live columns.
function link_cell_build(port, transient) {
  const held = transient.includes('link_down');
  const state = port.live.up
    ? node_build('span', { class: 'mono', text: speed_format(port.live.speed_mbps) })
    : node_build('span', { class: held ? 'warn' : 'muted', text: 'down', title: held ? LINK_HELD_TITLE : null });
  return node_build('span', { class: 'link' }, node_build('span', { class: port.live.up ? 'dot dot-ok' : held ? 'dot dot-warn' : 'dot dot-off' }), state);
}

function poe_cell_build(port, transient) {
  if (!port_poe_check(port)) return node_build('span', { class: 'faint', text: 'no PoE' });
  const held = transient.includes('poe_off');
  const watts = port.live.poe_power_w || 0;
  const klass = port.live.poe_class && port.live.poe_class !== 'Unknown' ? port.live.poe_class.replace(/^Class\s*/i, 'class ') : null;
  return node_build(
    'span',
    { class: 'poe' },
    node_build('span', { class: watts > 0 ? 'mono' : 'mono faint', text: `${watts.toFixed(1)} W` }),
    held ? node_build('span', { class: 'warn', text: 'off', title: POE_HELD_TITLE }) : klass ? node_build('span', { class: 'muted', text: klass }) : null,
  );
}

function stp_state_build(state) {
  const tone = { forwarding: '', blocking: 'warn', discarding: 'warn', disabled: 'faint' }[state];
  return node_build('span', { class: tone === undefined ? '' : tone, text: state || 'unknown' });
}

function macs_cell_build(port) {
  const unique = [...new Set(port.live.macs)];
  if (unique.length === 0) return node_build('span', { class: 'faint', text: '0' });
  return node_build('button', { type: 'button', class: 'link-btn mono', text: unique.length, title: 'Show the MAC addresses', onclick: () => macs_show(port) });
}

function vlans_cell_build(settings) {
  const parts = [];
  parts.push(
    settings.native_vlan === null
      ? node_build('span', { class: 'vlan-native faint', title: 'No native VLAN', text: 'none' })
      : node_build('span', { class: 'vlan-native', title: 'Native VLAN', text: settings.native_vlan }),
  );
  if (settings.vlan_all) {
    parts.push(node_build('span', { class: 'chip chip-accent', title: 'The port carries every VLAN of the switch, also a VLAN added later', text: 'all VLANs' }));
  } else if (settings.tagged_vlans.length > 0) {
    parts.push(node_build('span', { class: 'vlan-tagged mono', title: 'Tagged VLANs', text: vlans_format(settings.tagged_vlans) }));
  }
  return node_build('span', { class: 'vlans' }, parts);
}

function settings_chips_build(settings) {
  const chips = [node_build('span', { class: settings.stp ? 'chip' : 'chip chip-off', text: settings.stp ? 'STP' : 'STP off' })];
  if (settings.edge === 'enabled') chips.push(node_build('span', { class: 'chip', text: 'edge' }));
  if (settings.edge === 'disabled') chips.push(node_build('span', { class: 'chip chip-off', text: 'edge off' }));
  if (settings.bpdu_guard) chips.push(node_build('span', { class: 'chip', text: 'BPDU guard' }));
  if (settings.bpdu_filter) chips.push(node_build('span', { class: 'chip chip-accent', text: 'BPDU filter' }));
  if (settings.poe === 'off') chips.push(node_build('span', { class: 'chip chip-off', text: 'PoE off' }));
  if (!settings.enabled) chips.push(node_build('span', { class: 'chip chip-danger', text: 'disabled', title: 'The port is disabled in the saved configuration' }));
  return node_build('span', { class: 'chips' }, chips);
}

function reservation_cell_build(port) {
  const reservation = port_reservation_get(port.idx);
  if (!reservation) return null;
  const expires = time_parse(reservation.expires);
  return node_build(
    'span',
    { class: 'resv' },
    node_build('span', { class: 'resv-text', title: `${reservation.owner}, expires ${expires.toLocaleString()}` },
      node_build('span', { class: 'resv-owner ellipsis', text: reservation.owner }),
      node_build('span', { class: 'resv-left', text: duration_format((expires - Date.now()) / 1000) + ' left' }),
    ),
    node_build('button', { type: 'button', class: 'icon-btn', title: `Release the reservation of ${reservation.owner}`, 'aria-label': 'Release the reservation', onclick: () => reservation_release(reservation) }, icon_build('close')),
  );
}

function actions_build(port) {
  const transient = app.device.transient[port.idx] || [];
  const held_titles = { link_down: LINK_HELD_TITLE, poe_off: POE_HELD_TITLE };
  const button = (action, label) => {
    const held = transient.includes(action);
    const running = app.actions_running.has(action_key_build(app.current, port.idx, action));
    return node_build('button', {
      type: 'button',
      class: 'seg-btn' + (held ? ' is-latched' : '') + (running ? ' is-busy' : ''),
      disabled: running,
      'aria-pressed': action in held_titles ? String(held) : null,
      text: label,
      title: held ? held_titles[action] : ACTION_LABELS[action] + ' on ' + port_label_get(port),
      onclick: () => action_run(port, action),
    });
  };
  const groups = [node_build('span', { class: 'seg', role: 'group', 'aria-label': 'Link' }, node_build('span', { class: 'seg-label', text: 'Link' }), button('link_down', 'down'), button('link_up', 'up'))];
  if (port_poe_check(port)) {
    groups.push(node_build('span', { class: 'seg', role: 'group', 'aria-label': 'PoE' }, node_build('span', { class: 'seg-label', text: 'PoE' }), button('poe_off', 'off'), button('poe_on', 'on'), button('poe_cycle', 'cycle')));
  }
  groups.push(locate_button_build(port));
  return node_build('span', { class: 'actions' }, groups);
}

function locate_button_build(port) {
  const located = port_located_check(port.idx);
  const running = app.actions_running.has(action_key_build(app.current, port.idx, LOCATE_KEY));
  return node_build(
    'span',
    { class: 'seg', role: 'group', 'aria-label': 'Locate' },
    node_build('button', {
      type: 'button',
      class: 'seg-btn' + (located ? ' is-locating' : '') + (running ? ' is-busy' : ''),
      disabled: running,
      'aria-pressed': String(located),
      text: 'Locate',
      title: located
        ? `The LED of ${port_label_get(port)} blinks. Stop the locate in the banner above the table.`
        : `Make the LED of ${port_label_get(port)} blink for ${duration_format(LOCATE_DURATION_S)}. It replaces a running locate.`,
      onclick: () => locate_run(port),
    }),
  );
}

function macs_show(port) {
  const counts = new Map();
  for (const mac of port.live.macs) counts.set(mac, (counts.get(mac) || 0) + 1);
  const rows = [...counts.entries()].map(([mac, count]) =>
    node_build('li', null, node_build('span', { class: 'mono', text: mac }), count > 1 ? node_build('span', { class: 'muted', text: `on ${count} VLANs` }) : null),
  );
  dialog_ask({
    title: `MAC addresses on ${port_label_get(port)}`,
    body: [node_build('p', { class: 'muted', text: `${counts.size} addresses in the last read of the switch.` }), node_build('ul', { class: 'mac-list' }, rows)],
    buttons: [{ label: 'Close', value: null }],
  }).then(dialog_close);
}

// ---------- port editor ----------

function editor_toggle(idx) {
  if (app.editor && app.editor.idx === idx) {
    editor_close();
    return;
  }
  editor_open(idx);
}

async function editor_open(idx) {
  if (app.editor && editor_patch_build(app.editor).changed.length > 0) {
    if (!(await edits_discard_confirm())) return;
  }
  const previous = app.editor ? app.editor.idx : null;
  editor_drop();
  const port = app.device.ports.find((p) => p.idx === idx);
  if (!port) return;
  app.editor = { idx, baseline: structuredClone(port.settings), node: null, stale: false };
  app.editor.node = editor_build(port);
  ports_rows_refresh(previous === null ? [idx] : [previous, idx]);
  const first = app.editor.node.querySelector('input, select');
  if (first) first.focus({ preventScroll: true });
}

async function editor_close() {
  if (app.editor && editor_patch_build(app.editor).changed.length > 0) {
    if (!(await edits_discard_confirm())) return;
  }
  const idx = app.editor ? app.editor.idx : null;
  editor_drop();
  if (idx !== null) ports_rows_refresh([idx]);
}

function editor_drop() {
  if (app.editor && app.editor.node) app.editor.node.remove();
  app.editor = null;
}

function ports_rows_refresh(indexes) {
  for (const idx of indexes) app.port_signatures.delete(idx);
  if (app.device) ports_render();
}

// Places the editor row under its port row. A clean editor follows the switch; an editor
// with edits keeps them and notes that the switch changed underneath.
function editor_sync() {
  const editor = app.editor;
  if (!editor) return;
  const port = app.device.ports.find((p) => p.idx === editor.idx);
  const row = app.port_rows.get(editor.idx);
  if (!port || !row) {
    editor_drop();
    return;
  }
  const differs = JSON.stringify(port.settings) !== JSON.stringify(editor.baseline);
  const dirty = editor_patch_build(editor).changed.length > 0;
  if (differs && !dirty && !focus_inside_check(editor.node)) {
    editor.baseline = structuredClone(port.settings);
    const fresh = editor_build(port);
    editor.node.replaceWith(fresh);
    editor.node = fresh;
  } else if (differs !== editor.stale) {
    editor.stale = differs;
    editor_footer_update(editor);
  }
  if (row.nextSibling !== editor.node) row.after(editor.node);
}

function editor_build(port) {
  const s = app.editor.baseline;
  const vlans = app.device.global.vlans;
  const poe = port_poe_check(port);
  const form = node_build('form', { class: 'editor-form', onsubmit: (event) => {
    event.preventDefault();
    editor_review();
  }, oninput: () => editor_footer_update(app.editor), onchange: () => editor_footer_update(app.editor) });

  const native = node_build('select', { name: 'native_vlan', onchange: () => editor_tagged_lock(form) },
    node_build('option', { value: '', text: 'none' }),
    vlans.map((id) => node_build('option', { value: String(id), text: String(id), selected: s.native_vlan === id })),
  );
  const tagged = node_build('div', { class: 'vlan-picks', role: 'group', 'aria-label': 'Tagged VLANs' },
    vlans.map((id) =>
      node_build('label', { class: 'pick' },
        node_build('input', { type: 'checkbox', name: 'tagged', value: String(id), checked: s.tagged_vlans.includes(id) }),
        node_build('span', { text: id }),
      ),
    ),
  );

  node_append(form, [
    node_build('div', { class: 'editor-grid' },
      field_build('Name', node_build('input', { type: 'text', name: 'name', value: s.name, maxlength: 64, spellcheck: false })),
      field_build('Native VLAN', native),
      field_build('Tagged VLANs', tagged, 'wide'),
      field_build('PoE', select_build('poe', ['auto', 'off'], s.poe, !poe), null, poe ? null : 'This port has no PoE.'),
      field_build('Edge', select_build('edge', EDGE_MODES, s.edge)),
      node_build('div', { class: 'field toggles' },
        node_build('span', { class: 'field-label', text: 'Options' }),
        node_build('div', { class: 'toggle-row' },
          toggle_build('stp', 'STP', s.stp),
          toggle_build('bpdu_guard', 'BPDU guard', s.bpdu_guard),
          toggle_build('bpdu_filter', 'BPDU filter', s.bpdu_filter),
          toggle_build('enabled', 'Port enabled', s.enabled),
        ),
      ),
    ),
    s.vlan_all
      ? node_build('p', { class: 'note' }, node_build('strong', { text: 'All VLANs. ' }), 'This port carries every VLAN of the switch. A change to its native or tagged VLANs gives it an explicit list, and a VLAN added later does not reach it.')
      : null,
    node_build('div', { class: 'editor-footer' },
      node_build('span', { class: 'editor-status' }),
      node_build('button', { type: 'button', class: 'btn btn-quiet', name: 'discard', text: 'Discard', onclick: editor_discard }),
      node_build('button', { type: 'submit', class: 'btn btn-primary', name: 'review', text: 'Review change' }),
    ),
  ]);
  editor_tagged_lock(form);
  const row = node_build('tr', { class: 'editor-row' + (port.protected ? ' is-protected' : '') }, node_build('td', { colspan: 10 }, form));
  // The footer reads the form, so it runs after the form is in the row.
  queueMicrotask(() => app.editor && editor_footer_update(app.editor));
  return row;
}

// A label around a group of checkboxes would toggle the first one on a click on its
// caption, so a group gets a plain container.
function field_build(label, control, wide, hint) {
  const single = control instanceof HTMLInputElement || control instanceof HTMLSelectElement;
  return node_build(single ? 'label' : 'div', { class: 'field' + (wide ? ' field-wide' : '') },
    node_build('span', { class: 'field-label', text: label }),
    control,
    hint ? node_build('span', { class: 'field-hint', text: hint }) : null,
  );
}

function select_build(name, values, current, disabled) {
  return node_build('select', { name, disabled: !!disabled }, values.map((v) => node_build('option', { value: v, text: v, selected: v === current })));
}

function toggle_build(name, label, checked) {
  return node_build('label', { class: 'toggle' },
    node_build('input', { type: 'checkbox', name, checked, role: 'switch' }),
    node_build('span', { class: 'toggle-track', 'aria-hidden': 'true' }),
    node_build('span', { text: label }),
  );
}

// A VLAN cannot be native and tagged on the same port, so the native VLAN is not offered
// as a tagged one.
function editor_tagged_lock(form) {
  const native = form.elements.native_vlan.value;
  for (const box of form.querySelectorAll('input[name="tagged"]')) {
    const is_native = box.value === native;
    box.disabled = is_native;
    if (is_native) box.checked = false;
    box.parentElement.classList.toggle('is-native', is_native);
  }
}

function editor_values_read(form) {
  const native = form.elements.native_vlan.value;
  return {
    name: form.elements.name.value,
    native_vlan: native === '' ? null : Number(native),
    tagged_vlans: [...form.querySelectorAll('input[name="tagged"]:checked')].map((b) => Number(b.value)).sort((a, b) => a - b),
    poe: form.elements.poe.value,
    stp: form.elements.stp.checked,
    edge: form.elements.edge.value,
    bpdu_guard: form.elements.bpdu_guard.checked,
    bpdu_filter: form.elements.bpdu_filter.checked,
    enabled: form.elements.enabled.checked,
  };
}

// The patch holds only the fields the user changed: the daemon refuses unknown fields,
// and a field sent unchanged would still count as a request to write it.
function editor_patch_build(editor) {
  const form = editor.node && editor.node.querySelector('form');
  if (!form) return { patch: {}, changed: [] };
  const values = editor_values_read(form);
  const base = editor.baseline;
  const patch = {};
  for (const key of Object.keys(PORT_FIELD_LABELS)) {
    const now = key === 'tagged_vlans' ? JSON.stringify(values[key]) : values[key];
    const before = key === 'tagged_vlans' ? JSON.stringify([...base[key]].sort((a, b) => a - b)) : base[key];
    if (now !== before) patch[key] = values[key];
  }
  return { patch, changed: Object.keys(patch) };
}

function editor_footer_update(editor) {
  if (!editor || !editor.node) return;
  const { changed } = editor_patch_build(editor);
  const status = editor.node.querySelector('.editor-status');
  const parts = [];
  if (changed.length > 0) {
    parts.push(node_build('span', { class: 'changes' }, `${changed.length} ${changed.length === 1 ? 'change' : 'changes'}: `, changed.map((k) => PORT_FIELD_LABELS[k]).join(', ')));
  } else {
    parts.push(node_build('span', { class: 'muted', text: 'No changes' }));
  }
  if (editor.stale) {
    parts.push(node_build('span', { class: 'stale', text: 'The switch settings changed after you started. The review shows the current values.' }));
  }
  status.replaceChildren(...parts);
  editor.node.querySelector('button[name="review"]').disabled = changed.length === 0;
  editor.node.querySelector('button[name="discard"]').disabled = changed.length === 0;
}

function editor_discard() {
  const editor = app.editor;
  if (!editor) return;
  const port = app.device.ports.find((p) => p.idx === editor.idx);
  editor.baseline = structuredClone(port.settings);
  editor.stale = false;
  const fresh = editor_build(port);
  editor.node.replaceWith(fresh);
  editor.node = fresh;
}

async function editor_review() {
  const editor = app.editor;
  if (!editor) return;
  const port = app.device.ports.find((p) => p.idx === editor.idx);
  const { patch } = editor_patch_build(editor);
  if (Object.keys(patch).length === 0) return;
  const vlan_change = 'native_vlan' in patch || 'tagged_vlans' in patch;
  const warnings = [];
  const reserved_text = reservation_warning_get(port, 'change');
  if (reserved_text) warnings.push(reserved_text);
  if (vlan_change && port.settings.vlan_all) {
    warnings.push(
      `${sentence_capitalize(port_label_get(port))} carries all VLANs now. This change gives it an explicit list: ` +
        `native ${vlan_value_describe(patch.native_vlan, port.settings.native_vlan)}, ` +
        `tagged ${vlans_list_describe(patch.tagged_vlans || port.settings.tagged_vlans)}. ` +
        'A VLAN added to the switch later does not reach this port.',
    );
  }
  const applied = await change_run({
    title: `Change ${port_label_get(port)} on ${app.current}`,
    subtitle: port.settings.name,
    path: `${switch_path_build(app.current)}/ports/${port.idx}`,
    patch,
    warnings,
    second: port.protected
      ? {
          text: `${sentence_capitalize(port_label_get(port))} "${port.settings.name}" is a protected port. It carries network infrastructure, and a wrong change can cut off the network.`,
          label: `Apply to protected port ${port.idx}`,
        }
      : null,
  });
  if (applied && app.editor === editor) {
    editor_drop();
    ports_rows_refresh([port.idx]);
  }
  await state_refresh();
}

function vlan_value_describe(patched, current) {
  const value = patched === undefined ? current : patched;
  return value === null ? 'none' : String(value);
}

function vlans_list_describe(ids) {
  return ids.length === 0 ? 'none' : vlans_format(ids);
}

// ---------- switch-wide settings ----------

function global_render() {
  const node = document.getElementById('global');
  const global = app.device.global;
  const edit = app.global_edit;
  const dirty = edit !== null && global_patch_build(edit).changed.length > 0;
  if (dirty || focus_inside_check(node)) {
    if (edit && JSON.stringify(edit.baseline) !== JSON.stringify(global)) {
      edit.stale = true;
      global_footer_update();
    }
    return;
  }
  app.global_edit = null;
  section_update(node, global, () => global_build(global));
}

function global_draft_get() {
  if (!app.global_edit) {
    const base = structuredClone(app.device.global);
    app.global_edit = { baseline: base, draft: structuredClone(base), stale: false };
  }
  return app.global_edit;
}

function global_build(global) {
  const priorities = [];
  for (let p = 0; p <= STP_PRIORITY_MAX; p += STP_PRIORITY_STEP) priorities.push(p);
  const draft = app.global_edit ? app.global_edit.draft : global;
  const onchange = (key, cast) => (event) => {
    global_draft_get().draft[key] = cast(event.target.value);
    global_footer_update();
  };
  const add_input = node_build('input', { type: 'number', min: 1, max: VLAN_ID_MAX, placeholder: 'VLAN id', name: 'vlan_add', class: 'vlan-add-input', onkeydown: (event) => {
    if (event.key === 'Enter') {
      event.preventDefault();
      global_vlan_add(add_input);
    }
  } });
  return [
    node_build('header', { class: 'card-head' },
      node_build('h2', { text: 'Switch-wide settings' }),
      node_build('span', { class: 'badge badge-protect', text: 'second confirmation' }),
    ),
    node_build('div', { class: 'global-grid' },
      field_build(
        'STP version',
        node_build('select', { onchange: onchange('stp_version', String) },
          STP_VERSIONS.includes(draft.stp_version) ? null : node_build('option', { value: draft.stp_version, text: 'not set', selected: true }),
          STP_VERSIONS.map((v) => node_build('option', { value: v, text: v.toUpperCase(), selected: v === draft.stp_version })),
        ),
      ),
      field_build('Bridge priority', node_build('select', { onchange: onchange('stp_priority', Number) }, priorities.map((p) => node_build('option', { value: String(p), text: String(p), selected: p === draft.stp_priority })))),
      field_build(
        'BPDU action',
        node_build('select', { onchange: onchange('bpdu_action', String) }, BPDU_ACTIONS.map((v) => node_build('option', { value: v, text: v, selected: v === draft.bpdu_action }))),
        'wide',
        'For ports with STP off: flooding sends a BPDU on to the other such ports, also across VLANs; filtering drops it.',
      ),
    ),
    node_build('div', { class: 'field' },
      node_build('span', { class: 'field-label', text: 'VLANs' }),
      node_build('div', { class: 'vlan-chips', id: 'vlan-chips' }, vlan_chips_build(global.vlans, draft.vlans)),
      node_build('div', { class: 'vlan-add' },
        add_input,
        node_build('button', { type: 'button', class: 'btn', text: 'Add', onclick: () => global_vlan_add(add_input) }),
      ),
    ),
    node_build('div', { class: 'editor-footer' },
      node_build('span', { class: 'editor-status', id: 'global-status' }, node_build('span', { class: 'muted', text: 'No changes' })),
      node_build('button', { type: 'button', class: 'btn btn-quiet', id: 'global-discard', text: 'Discard', disabled: true, onclick: global_discard }),
      node_build('button', { type: 'button', class: 'btn btn-primary', id: 'global-review', text: 'Review change', disabled: true, onclick: global_review }),
    ),
  ];
}

function vlan_chips_build(current, draft) {
  const all = [...new Set([...current, ...draft])].sort((a, b) => a - b);
  return all.map((id) => {
    const added = !current.includes(id);
    const removed = !draft.includes(id);
    const cls = 'vlan-chip' + (added ? ' is-added' : '') + (removed ? ' is-removed' : '');
    const label = removed ? `Keep VLAN ${id}` : `Remove VLAN ${id}`;
    return node_build('span', { class: cls },
      node_build('span', { class: 'mono', text: (added ? '+' : '') + id }),
      node_build('button', { type: 'button', class: 'icon-btn', title: label, 'aria-label': label, onclick: () => global_vlan_toggle(id) }, removed ? node_build('span', { class: 'undo', text: 'undo' }) : icon_build('close')),
    );
  });
}

function global_vlan_toggle(id) {
  const edit = global_draft_get();
  const vlans = edit.draft.vlans;
  edit.draft.vlans = vlans.includes(id) ? vlans.filter((v) => v !== id) : [...vlans, id].sort((a, b) => a - b);
  global_chips_refresh();
}

function global_vlan_add(input) {
  const id = Number(input.value);
  if (!Number.isInteger(id) || id < 1 || id > VLAN_ID_MAX) {
    input.setCustomValidity(`A VLAN id is a whole number from 1 to ${VLAN_ID_MAX}.`);
    input.reportValidity();
    return;
  }
  input.setCustomValidity('');
  const edit = global_draft_get();
  if (!edit.draft.vlans.includes(id)) edit.draft.vlans = [...edit.draft.vlans, id].sort((a, b) => a - b);
  input.value = '';
  global_chips_refresh();
  input.focus();
}

function global_chips_refresh() {
  const edit = app.global_edit;
  document.getElementById('vlan-chips').replaceChildren(...vlan_chips_build(edit.baseline.vlans, edit.draft.vlans));
  global_footer_update();
}

function global_patch_build(edit) {
  const patch = {};
  for (const key of Object.keys(GLOBAL_FIELD_LABELS)) {
    if (JSON.stringify(edit.draft[key]) !== JSON.stringify(edit.baseline[key])) patch[key] = edit.draft[key];
  }
  return { patch, changed: Object.keys(patch) };
}

function global_footer_update() {
  const edit = app.global_edit;
  const status = document.getElementById('global-status');
  if (!status) return;
  const changed = edit ? global_patch_build(edit).changed : [];
  const parts = [];
  if (changed.length > 0) {
    parts.push(node_build('span', { class: 'changes' }, `${changed.length} ${changed.length === 1 ? 'change' : 'changes'}: `, changed.map((k) => GLOBAL_FIELD_LABELS[k]).join(', ')));
  } else {
    parts.push(node_build('span', { class: 'muted', text: 'No changes' }));
  }
  if (edit && edit.stale) parts.push(node_build('span', { class: 'stale', text: 'The switch settings changed after you started. The review shows the current values.' }));
  status.replaceChildren(...parts);
  document.getElementById('global-review').disabled = changed.length === 0;
  document.getElementById('global-discard').disabled = changed.length === 0;
}

function global_discard() {
  app.global_edit = null;
  const node = document.getElementById('global');
  node.dataset.signature = '';
  section_update(node, app.device.global, () => global_build(app.device.global));
}

async function global_review() {
  const edit = app.global_edit;
  if (!edit) return;
  const { patch } = global_patch_build(edit);
  if (Object.keys(patch).length === 0) return;
  const warnings = [];
  if (patch.stp_version === 'stp') {
    warnings.push(
      'Legacy STP puts every port through the listening and learning states, about 30 s. ' +
        'The uplink converges again and the switch sends a topology change into the network. ' +
        'This mode is not verified on a real switch.',
    );
  }
  const applied = await change_run({
    title: `Switch-wide change on ${app.current}`,
    subtitle: model_name_get((app.switches.find((s) => s.name === app.current) || {}).model),
    path: `${switch_path_build(app.current)}/global`,
    patch,
    warnings,
    second: {
      text: `This change is switch-wide. It applies to every port of ${app.current}.`,
      label: 'Apply switch-wide',
    },
  });
  if (applied) global_discard();
  await state_refresh();
}

// ---------- change flow ----------

// Dry run, review, first confirmation, second confirmation where needed, real write,
// result. Returns true when the daemon applied the change.
async function change_run(spec) {
  dialog_busy_show(spec.title, 'Asking the switch for a dry run of the change.');
  let plan;
  try {
    plan = await api_call('PATCH', spec.path + '?dry_run=1', spec.patch);
  } catch (e) {
    if (e.status === 401) return false;
    await error_show(spec.title, 'The dry run failed. Nothing changed on the switch.', e);
    return false;
  }
  if (plan.keys.length === 0 && plan.cli_lines.length === 0) {
    await dialog_ask({
      title: spec.title,
      subtitle: spec.subtitle,
      body: node_build('p', { text: 'The switch already has these values. There is nothing to apply.' }),
      buttons: [{ label: 'Close', value: null }],
    });
    dialog_close();
    return false;
  }
  const review = await dialog_ask({
    title: spec.title,
    subtitle: spec.subtitle,
    body: [spec.warnings.map((text) => node_build('p', { class: 'callout callout-warn', text })), change_view_build(plan, 'Dry run')],
    buttons: [
      { label: 'Cancel', value: null },
      { label: spec.second ? 'Continue' : 'Apply', value: 'apply', kind: 'primary' },
    ],
  });
  if (review !== 'apply') {
    dialog_close();
    return false;
  }
  if (spec.second) {
    const second = await dialog_ask({
      title: 'Second confirmation',
      subtitle: spec.title,
      body: [node_build('p', { class: 'callout callout-protect', text: spec.second.text }), change_view_build(plan, 'Changes')],
      buttons: [
        { label: 'Cancel', value: null },
        { label: spec.second.label, value: 'apply', kind: 'danger' },
      ],
    });
    if (second !== 'apply') {
      dialog_close();
      return false;
    }
  }
  dialog_busy_show(spec.title, 'Applying the change. The switch saves it to flash and reads it back; this takes some seconds.');
  let result;
  try {
    result = await api_call('PATCH', spec.path, spec.patch);
  } catch (e) {
    if (e.status === 401) return false;
    await error_show(spec.title, write_error_lead_get(e, 'change'), e);
    return false;
  }
  await dialog_ask({
    title: spec.title,
    subtitle: spec.subtitle,
    body: [
      node_build('p', { class: result.readback_ok ? 'callout callout-ok' : 'callout callout-warn', text: result.readback_ok ? 'Applied. The read-back from the switch shows the change.' : 'Applied, but the read-back does not confirm the change.' }),
      change_view_build(result, 'Result'),
    ],
    buttons: [{ label: 'Close', value: null, kind: 'primary' }],
  });
  dialog_close();
  return true;
}

function change_view_build(result, caption) {
  const parts = [];
  if (result.keys.length > 0) {
    parts.push(
      node_build('table', { class: 'keys' },
        node_build('caption', { text: `${caption}: system.cfg keys` }),
        node_build('thead', null, node_build('tr', null, node_build('th', { text: 'Key' }), node_build('th', { text: 'Old' }), node_build('th', { text: 'New' }))),
        node_build('tbody', null, result.keys.map((k) =>
          node_build('tr', null,
            node_build('td', { class: 'mono', text: k.key }),
            node_build('td', { class: 'mono old' }, k.old === null ? node_build('span', { class: 'faint', text: 'absent' }) : k.old),
            node_build('td', { class: 'mono new' }, k.new === null ? node_build('span', { class: 'faint', text: 'removed' }) : k.new),
          ),
        )),
      ),
    );
  }
  if (result.cli_lines.length > 0) {
    parts.push(node_build('div', { class: 'cli' }, node_build('div', { class: 'cli-caption', text: `${caption}: CLI lines of the overlay` }), node_build('pre', { class: 'mono', text: result.cli_lines.join('\n') })));
  } else if (result.overlay_changed) {
    parts.push(node_build('p', { class: 'muted', text: 'The overlay file changes, with no new CLI lines.' }));
  }
  return parts;
}

const REFUSAL_KINDS = new Set([
  'locked', 'user_only', 'not_owner', 'reserved', 'no_owner', 'untested_firmware', 'settings',
  'bad_request', 'bad_query', 'port_range', 'unknown_switch', 'unknown_backup', 'not_found',
  'storage_error',
]);
const PARTIAL_KINDS = new Set(['apply_failed', 'save_failed', 'unreachable', 'read_failed']);

// The kind tells how far the daemon got, so the lead says what the switch holds now.
// `what` is 'change' or 'restore'.
function write_error_lead_get(error, what) {
  const subject = what === 'restore' ? 'the backup' : 'the change';
  if (error.kind === 'no_effect') {
    return `The daemon sent and saved ${subject}, but the read-back does not confirm it. Check the port, or restore a backup.`;
  }
  if (REFUSAL_KINDS.has(error.kind)) {
    return `The switch was not changed: the daemon refused the ${what}.`;
  }
  if (PARTIAL_KINDS.has(error.kind)) {
    return `The ${what} may be incomplete: the daemon stopped partway. Check the port, or restore a backup.`;
  }
  if (error.kind === 'backup_failed') {
    return `The switch has ${subject}, but the backup on this host failed.`;
  }
  if (error.kind === 'network') {
    return `The answer of the daemon did not arrive, so the result is not known. The next refresh shows the current state.`;
  }
  return `The daemon reported an error, so the result is not known. The next refresh shows the current state.`;
}

async function error_show(title, lead, error) {
  await dialog_ask({
    title,
    body: [node_build('p', { text: lead }), error_view_build(error)],
    buttons: [{ label: 'Close', value: null, kind: 'primary' }],
  });
  dialog_close();
}

const NO_EFFECT_MARK = 'effect of:';

function error_view_build(error) {
  return node_build('div', { class: 'callout callout-danger' },
    node_build('div', { class: 'error-kind mono', text: error.status ? `${error.status} ${error.kind}` : error.kind }),
    error_message_build(error),
  );
}

// The daemon joins the missing effects with ", "; one per line is easier to compare with
// the keys of the review.
function error_message_build(error) {
  const at = error.message.indexOf(NO_EFFECT_MARK);
  if (error.kind !== 'no_effect' || at < 0) return node_build('div', { text: error.message });
  const items = error.message.slice(at + NO_EFFECT_MARK.length).split(', ').map((t) => t.trim()).filter(Boolean);
  return [
    node_build('div', { text: error.message.slice(0, at + NO_EFFECT_MARK.length) }),
    node_build('ul', { class: 'effect-list mono' }, items.map((t) => node_build('li', { text: t }))),
  ];
}

// ---------- actions, reboot, backups, reservations ----------

function action_key_build(switch_name, idx, action) {
  return `${switch_name}/${idx}/${action}`;
}

async function action_run(port, action) {
  const switch_name = app.current;
  const key = action_key_build(switch_name, port.idx, action);
  if (app.actions_running.has(key)) return;
  const label = `${ACTION_LABELS[action]} on ${port_label_get(port)}`;
  const subtitle = `${app.current}, "${port.settings.name}"`;
  const reserved_text = reservation_warning_get(port, 'action');
  const reserved_node = reserved_text ? node_build('p', { class: 'callout callout-warn', text: reserved_text }) : null;
  const effect_node = node_build('p', { text: 'The action takes effect at once. It is not saved: a switch reboot ends it.' });
  const confirmed = port.protected || reserved_text !== null;
  if (port.protected) {
    const first = await dialog_ask({
      title: label,
      subtitle,
      body: [reserved_node, effect_node],
      buttons: [{ label: 'Cancel', value: null }, { label: 'Continue', value: 'go', kind: 'primary' }],
    });
    if (first !== 'go') return dialog_close();
    const second = await dialog_ask({
      title: 'Second confirmation',
      subtitle: label,
      body: node_build('p', { class: 'callout callout-protect', text: `${sentence_capitalize(port_label_get(port))} "${port.settings.name}" is a protected port. It carries network infrastructure, and this action can cut off the network.` }),
      buttons: [{ label: 'Cancel', value: null }, { label: `${ACTION_LABELS[action]} on protected port ${port.idx}`, value: 'go', kind: 'danger' }],
    });
    if (second !== 'go') return dialog_close();
  } else if (reserved_text) {
    const answer = await dialog_ask({
      title: label,
      subtitle,
      body: [reserved_node, effect_node],
      buttons: [{ label: 'Cancel', value: null }, { label: `${ACTION_LABELS[action]} on reserved port ${port.idx}`, value: 'go', kind: 'danger' }],
    });
    if (answer !== 'go') return dialog_close();
  }
  if (confirmed) dialog_busy_show(label, 'Sending the command to the switch.');
  app.actions_running.add(key);
  ports_rows_refresh([port.idx]);
  try {
    const result = await api_call('POST', `${switch_path_build(switch_name)}/ports/${port.idx}/actions`, { action });
    if (confirmed) dialog_close();
    toast_show({ title: `${switch_name}: ${label}`, lines: result.cli_lines, tone: 'ok' });
  } catch (e) {
    if (e.status === 401) return;
    if (confirmed) dialog_close();
    toast_show({ title: `${switch_name}: ${label} failed`, error: e, tone: 'danger' });
  } finally {
    app.actions_running.delete(key);
    if (switch_name === app.current) ports_rows_refresh([port.idx]);
  }
  await state_refresh();
}

// A locate changes only the LEDs, so it starts without a confirmation.
async function locate_run(port) {
  const switch_name = app.current;
  const key = action_key_build(switch_name, port.idx, LOCATE_KEY);
  if (app.actions_running.has(key)) return;
  const label = `Locate ${port_label_get(port)}`;
  app.actions_running.add(key);
  ports_rows_refresh([port.idx]);
  try {
    const result = await api_call('POST', `${switch_path_build(switch_name)}/locate`, { ports: [port.idx], duration_s: LOCATE_DURATION_S });
    toast_show({ title: `${switch_name}: ${label}`, lines: result.cli_lines, tone: 'ok' });
  } catch (e) {
    if (e.status === 401) return;
    toast_show({ title: `${switch_name}: ${label} failed`, error: e, tone: 'danger' });
  } finally {
    app.actions_running.delete(key);
    if (switch_name === app.current) ports_rows_refresh([port.idx]);
  }
  await state_refresh();
}

async function locate_stop() {
  const switch_name = app.current;
  if (app.locate_stopping.has(switch_name)) return;
  app.locate_stopping.add(switch_name);
  locate_render();
  try {
    const result = await api_call('DELETE', `${switch_path_build(switch_name)}/locate`);
    toast_show({ title: `${switch_name}: locate ended`, lines: result.cli_lines, tone: 'ok' });
  } catch (e) {
    if (e.status === 401) return;
    toast_show({ title: `${switch_name}: stop of the locate failed`, error: e, tone: 'danger' });
  } finally {
    app.locate_stopping.delete(switch_name);
  }
  await state_refresh();
}

function locate_banner_get() {
  if (app.locate_banner) return app.locate_banner;
  const banner = {
    ports: node_build('strong'),
    retry: node_build('span', { class: 'locate-retry' }),
    left: node_build('span', { class: 'locate-left' }),
    stop: node_build('button', { type: 'button', class: 'btn btn-small', onclick: locate_stop }),
  };
  document.getElementById('locate').replaceChildren(
    node_build('span', { class: 'locate-dot', 'aria-hidden': 'true' }),
    node_build('span', { class: 'locate-text', 'aria-live': 'polite' }, banner.ports, ' All other port LEDs are dark until the locate ends.', banner.retry),
    banner.left,
    banner.stop,
  );
  app.locate_banner = banner;
  return banner;
}

function text_set(node, text) {
  if (node.textContent !== text) node.textContent = text;
}

function locate_render() {
  const node = document.getElementById('locate');
  const locate = app.device.locate;
  node.hidden = !locate;
  if (!locate) return;
  const banner = locate_banner_get();
  const until = time_parse(locate.until);
  const left = Math.max(0, Math.ceil((until - Date.now()) / 1000));
  const ports = locate.ports.join(', ');
  const stopping = app.locate_stopping.has(app.current);
  text_set(banner.ports, locate.ports.length === 1 ? `Port ${ports} blinks blue.` : `Ports ${ports} blink blue.`);
  text_set(banner.retry, locate.end_failed ? ' Ending the locate failed; retrying.' : '');
  text_set(banner.left, locate.end_failed ? 'end time passed' : `${duration_format(left)} left`);
  banner.left.title = `Ends at ${until.toLocaleString()}`;
  text_set(banner.stop, stopping ? 'Stopping' : 'Stop');
  banner.stop.disabled = stopping;
}

async function reboot_run() {
  const name = app.current;
  const first = await dialog_ask({
    title: `Reboot ${name}`,
    body: [
      node_build('p', { text: 'The switch is gone for about 100 s. Its ports forward again from about 45 s of uptime, before the daemon runs the overlay again.' }),
      node_build('p', { class: 'muted', text: 'Transient link and PoE actions end with the reboot.' }),
    ],
    buttons: [{ label: 'Cancel', value: null }, { label: 'Continue', value: 'go', kind: 'primary' }],
  });
  if (first !== 'go') return dialog_close();
  const second = await dialog_ask({
    title: 'Second confirmation',
    subtitle: `Reboot ${name}`,
    body: node_build('p', { class: 'callout callout-protect', text: `A reboot is switch-wide. Every port of ${name} goes down, also the protected ports.` }),
    buttons: [{ label: 'Cancel', value: null }, { label: `Reboot ${name}`, value: 'go', kind: 'danger' }],
  });
  if (second !== 'go') return dialog_close();
  dialog_busy_show(`Reboot ${name}`, 'Sending the reboot command.');
  try {
    await api_call('POST', `${switch_path_build(name)}/reboot`);
    dialog_close();
    toast_show({ title: `${name}: reboot command accepted`, lines: ['The switch answers again after about 100 s.'], tone: 'ok' });
  } catch (e) {
    if (e.status === 401) return;
    await error_show(`Reboot ${name}`, 'The switch did not accept the reboot command.', e);
  }
  await state_refresh();
}

async function backups_show() {
  const name = app.current;
  dialog_busy_show(`Backups of ${name}`, 'Reading the backup list.');
  let stamps;
  try {
    stamps = await api_call('GET', `${switch_path_build(name)}/backups`);
  } catch (e) {
    if (e.status === 401) return;
    await error_show(`Backups of ${name}`, 'The backup list is not available.', e);
    return;
  }
  const sorted = [...stamps].sort().reverse();
  const choice = await dialog_ask({
    title: `Backups of ${name}`,
    subtitle: 'system.cfg and the overlay, saved after each change',
    body: sorted.length === 0
      ? node_build('p', { class: 'muted', text: 'No backups yet. The daemon saves one after each successful change.' })
      : node_build('ul', { class: 'backup-list' }, sorted.map((stamp, i) =>
          node_build('li', null,
            node_build('span', { class: 'mono', text: stamp }),
            i === 0 ? node_build('span', { class: 'badge', text: 'newest' }) : null,
            node_build('button', { type: 'button', class: 'btn btn-small', text: 'Restore', 'data-value': stamp }),
          ),
        )),
    buttons: [{ label: 'Close', value: null }],
  });
  if (!choice) return dialog_close();
  await backup_restore(name, choice);
}

async function backup_restore(name, stamp) {
  const title = `Restore ${name}`;
  const first = await dialog_ask({
    title,
    subtitle: `Backup ${stamp}`,
    body: node_build('p', { text: `The switch takes system.cfg and the overlay from backup ${stamp}, applies them and saves them to flash.` }),
    buttons: [{ label: 'Cancel', value: null }, { label: 'Continue', value: 'go', kind: 'primary' }],
  });
  if (first !== 'go') return dialog_close();
  const second = await dialog_ask({
    title: 'Second confirmation',
    subtitle: title,
    body: node_build('p', { class: 'callout callout-protect', text: `A restore is switch-wide. It replaces the settings of every port of ${name}, also the protected ports.` }),
    buttons: [{ label: 'Cancel', value: null }, { label: `Restore ${stamp}`, value: 'go', kind: 'danger' }],
  });
  if (second !== 'go') return dialog_close();
  dialog_busy_show(title, 'Restoring the backup. This takes some seconds.');
  try {
    const result = await api_call('POST', `${switch_path_build(name)}/restore`, { stamp });
    await dialog_ask({
      title,
      subtitle: `Backup ${stamp}`,
      body: [
        node_build('p', { class: result.readback_ok ? 'callout callout-ok' : 'callout callout-warn', text: result.readback_ok ? 'Restored. The read-back from the switch shows the backup.' : 'Restored, but the read-back does not confirm it.' }),
        change_view_build(result, 'Result'),
      ],
      buttons: [{ label: 'Close', value: null, kind: 'primary' }],
    });
    dialog_close();
  } catch (e) {
    if (e.status === 401) return;
    await error_show(title, write_error_lead_get(e, 'restore'), e);
  }
  await state_refresh();
}

async function reservation_release(reservation) {
  const title = `Release ${reservation.switch} port ${reservation.port}`;
  const answer = await dialog_ask({
    title,
    subtitle: `Reserved by ${reservation.owner}`,
    body: node_build('p', { text: `${reservation.owner} loses the reservation, and other agents can then change the port. A test of ${reservation.owner} on this port can fail.` }),
    buttons: [{ label: 'Cancel', value: null }, { label: 'Release', value: 'go', kind: 'danger' }],
  });
  dialog_close();
  if (answer !== 'go') return;
  try {
    await api_call('DELETE', `/api/reservations/${reservation.id}`);
    toast_show({ title: `${title}: released`, tone: 'ok' });
  } catch (e) {
    if (e.status === 401) return;
    toast_show({ title: `${title} failed`, error: e, tone: 'danger' });
  }
  await state_refresh();
}

function events_render() {
  const events = app.device.events.slice(-EVENTS_SHOWN).reverse();
  section_update(document.getElementById('events'), [events, Math.floor(Date.now() / 60000)], () => [
    node_build('header', { class: 'card-head' }, node_build('h2', { text: 'Recent events' }), node_build('span', { class: 'muted', text: `${app.device.events.length} kept` })),
    events.length === 0
      ? node_build('p', { class: 'muted empty', text: 'No events since the daemon started.' })
      : node_build('ol', { class: 'events' }, events.map((event) => {
          const at = time_parse(event.at);
          const { text, tone } = event_describe(event);
          return node_build('li', { class: tone ? 'tone-' + tone : '' },
            node_build('time', { datetime: at.toISOString(), title: at.toLocaleString(), text: clock_format(at) }),
            node_build('span', { class: 'event-text', title: text, text }),
          );
        })),
  ]);
}

function reservations_render() {
  const list = [...app.reservations].sort((a, b) => a.switch.localeCompare(b.switch) || a.port - b.port);
  section_update(document.getElementById('reservations'), [list, Math.floor(Date.now() / 60000)], () => [
    node_build('header', { class: 'card-head' }, node_build('h2', { text: 'Reservations' }), node_build('span', { class: 'muted', text: 'all switches' })),
    list.length === 0
      ? node_build('p', { class: 'muted empty', text: 'No port is reserved.' })
      : node_build('table', { class: 'resv-table' },
          node_build('thead', null, node_build('tr', null, node_build('th', { text: 'Port' }), node_build('th', { text: 'Owner' }), node_build('th', { text: 'Left' }), node_build('th'))),
          node_build('tbody', null, list.map((r) => {
            const expires = time_parse(r.expires);
            const created = time_parse(r.created);
            return node_build('tr', null,
              node_build('td', { class: 'mono', text: `${r.switch} ${r.port}` }),
              node_build('td', null, node_build('span', { title: `${r.owner}, since ${created.toLocaleString()}`, text: r.owner })),
              node_build('td', { title: `Expires ${expires.toLocaleString()}`, text: duration_format((expires - Date.now()) / 1000) }),
              node_build('td', { class: 'num' }, node_build('button', { type: 'button', class: 'btn btn-small btn-quiet', text: 'Release', onclick: () => reservation_release(r) })),
            );
          })),
        ),
  ]);
}

// ---------- dialog ----------

function dialog_get() {
  return document.getElementById('dialog');
}

// Shows one step of a flow and resolves with the value of the button the user pressed,
// or null when the user closed the dialog.
function dialog_ask({ title, subtitle, body, buttons }) {
  const dialog = dialog_get();
  app.dialog_busy = false;
  return new Promise((resolve) => {
    let done = false;
    const finish = (value) => {
      if (done) return;
      done = true;
      dialog.removeEventListener('cancel', on_cancel);
      resolve(value);
    };
    const on_cancel = (event) => {
      event.preventDefault();
      finish(null);
    };
    dialog.addEventListener('cancel', on_cancel);
    const content = node_build('div', { class: 'dialog-body', onclick: (event) => {
      const target = event.target.closest('[data-value]');
      if (target) finish(target.dataset.value);
    } }, body);
    const footer = node_build('footer', { class: 'dialog-footer' }, buttons.map((b) =>
      node_build('button', { type: 'button', class: 'btn' + (b.kind ? ' btn-' + b.kind : ''), text: b.label, onclick: () => finish(b.value) }),
    ));
    dialog.replaceChildren(
      node_build('header', { class: 'dialog-head' },
        node_build('h2', { id: 'dialog-title', text: title }),
        subtitle ? node_build('p', { class: 'dialog-subtitle', text: subtitle }) : null,
      ),
      content,
      footer,
    );
    if (!dialog.open) dialog.showModal();
    const preferred = footer.querySelector('.btn-primary') || footer.querySelector('.btn:last-child');
    if (preferred && !preferred.classList.contains('btn-danger')) preferred.focus();
    else footer.querySelector('.btn').focus();
  });
}

// While a request runs, the dialog cannot be closed, so that the user sees its result.
function dialog_busy_show(title, text) {
  const dialog = dialog_get();
  app.dialog_busy = true;
  dialog.replaceChildren(
    node_build('header', { class: 'dialog-head' }, node_build('h2', { id: 'dialog-title', text: title })),
    node_build('div', { class: 'dialog-body busy' }, node_build('span', { class: 'spinner', 'aria-hidden': 'true' }), node_build('span', { text })),
  );
  if (!dialog.open) dialog.showModal();
}

function dialog_close() {
  const dialog = dialog_get();
  app.dialog_busy = false;
  if (dialog.open) dialog.close();
  dialog.replaceChildren();
}

async function edits_discard_confirm() {
  const port_dirty = app.editor && editor_patch_build(app.editor).changed.length > 0;
  const global_dirty = app.global_edit && global_patch_build(app.global_edit).changed.length > 0;
  if (!port_dirty && !global_dirty) return true;
  const what = port_dirty ? `the edits of port ${app.editor.idx}` : 'the switch-wide edits';
  const answer = await dialog_ask({
    title: 'Discard the edits?',
    body: node_build('p', { text: `You did not apply ${what}. They are lost if you continue.` }),
    buttons: [{ label: 'Keep editing', value: null, kind: 'primary' }, { label: 'Discard', value: 'go', kind: 'danger' }],
  });
  dialog_close();
  return answer === 'go';
}

// ---------- toasts ----------

function toast_show({ title, lines, error, tone }) {
  const node = node_build('div', { class: 'toast toast-' + tone, role: tone === 'danger' ? 'alert' : 'status' },
    node_build('div', { class: 'toast-head' },
      node_build('span', { class: 'toast-title', text: title }),
      node_build('button', { type: 'button', class: 'icon-btn', 'aria-label': 'Dismiss', onclick: () => node.remove() }, icon_build('close')),
    ),
    lines && lines.length > 0 ? node_build('pre', { class: 'mono toast-lines', text: lines.join('\n') }) : null,
    error ? error_view_build(error) : null,
  );
  document.getElementById('toasts').append(node);
  // An error stays until the user dismisses it.
  if (tone !== 'danger') setTimeout(() => node.remove(), 6000);
}

// ---------- start ----------

function page_start() {
  document.getElementById('login-form').addEventListener('submit', (event) => {
    event.preventDefault();
    const token = document.getElementById('login-token').value.trim();
    if (token) session_start(token, false);
  });
  document.getElementById('signout').addEventListener('click', async () => {
    if (!(await edits_discard_confirm())) return;
    session_end('');
  });
  dialog_get().addEventListener('cancel', (event) => {
    if (app.dialog_busy) event.preventDefault();
  });
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && !app.stopped) state_refresh();
  });
  window.addEventListener('beforeunload', (event) => {
    const port_dirty = app.editor && editor_patch_build(app.editor).changed.length > 0;
    const global_dirty = app.global_edit && global_patch_build(app.global_edit).changed.length > 0;
    if (!port_dirty && !global_dirty) return;
    event.preventDefault();
    event.returnValue = '';
  });
  window.addEventListener('hashchange', () => {
    const name = decodeURIComponent(location.hash.slice(1));
    if (app.switches.some((sw) => sw.name === name)) switch_select(name);
  });
  const stored = token_load();
  if (stored) session_start(stored, true);
  else login_show('');
}

page_start();
