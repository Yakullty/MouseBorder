const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = id => document.getElementById(id);
const STATUS = { connected: '已连接', connecting: '连接中', retrying: '重连中' };
const HOOK = { running: '运行中', failed: '未启动', stopped: '已停止' };
const CONTROL = { local: '本机控制', remote: '控制对端', receiving: '被对端控制' };
let current = {};

function deviceRow(d, connected) {
  const row = document.createElement('div');
  row.className = 'device';
  const who = document.createElement('div');
  who.className = 'who';
  const name = document.createElement('b');
  name.textContent = d.label || d.host;
  const host = document.createElement('small');
  host.textContent = d.host;
  who.append(name, host);
  const acts = document.createElement('div');
  acts.className = 'acts';
  const go = document.createElement('button');
  const isPeer = connected && current.peer === d.host;
  go.className = isPeer ? 'mini' : 'button';
  go.textContent = isPeer ? '断开' : '连接';
  go.onclick = () => invoke(isPeer ? 'disconnect' : 'connect_device', isPeer ? {} : { host: d.host });
  const forget = document.createElement('button');
  forget.className = 'mini';
  forget.textContent = '忘记';
  forget.onclick = () => invoke('forget_device', { host: d.host });
  acts.append(go, forget);
  row.append(who, acts);
  return row;
}

function paint(s) {
  current = { ...current, ...s };
  const connected = current.status === 'connected';
  $('hostname').textContent = current.hostname || 'MouseBorder';
  $('pin').textContent = current.pin || '------';
  $('status').textContent = STATUS[current.status] || '等待配对';
  $('status').className = 'badge ' + (connected ? 'on' : '');
  $('control').textContent = CONTROL[current.control] || CONTROL.local;
  $('control').className = 'badge ' + (current.control && current.control !== 'local' ? 'act' : '');
  $('disconnect').classList.toggle('hidden', !connected);
  $('edgeEnabled').checked = current.edge_enabled !== false;
  $('clipboard').checked = current.clipboard_enabled !== false;
  if (current.edge) $('edge').value = current.edge;
  $('hookState').textContent = HOOK[current.hook] || '未启动';
  $('hookState').className = 'badge ' + (current.hook === 'running' ? 'on' : '');
  const devices = current.devices || [];
  $('devices').classList.toggle('hidden', devices.length === 0);
  const list = $('deviceList');
  list.textContent = '';
  for (const d of devices) list.appendChild(deviceRow(d, connected));
  for (const [id, text] of [['notice', current.notice], ['error', current.error]]) {
    $(id).textContent = text || '';
    $(id).classList.toggle('hidden', !text);
  }
}

invoke('get_state').then(paint);
listen('state', e => paint(e.payload));
$('refreshPin').onclick = async () => { $('pin').textContent = await invoke('refresh_pin'); };
$('disconnect').onclick = () => invoke('disconnect');
$('connect').onclick = async () => {
  const host = $('ip').value.trim(), pin = $('pairPin').value.trim();
  if (!host || !/^[0-9]{6}$/.test(pin)) { paint({ error: '请输入对方 IP 和六位配对码' }); return; }
  await invoke('connect_peer', { host, pin });
};
$('edge').addEventListener('change', () => invoke('set_edge', { side: $('edge').value }));
$('edgeEnabled').addEventListener('change', () => invoke('set_enabled', { enabled: $('edgeEnabled').checked }));
$('clipboard').addEventListener('change', () => invoke('set_clipboard', { enabled: $('clipboard').checked }));
$('retryHook').addEventListener('click', async () => { $('hookState').textContent = '重试中'; $('hookState').className = 'badge'; await invoke('retry_hook'); });