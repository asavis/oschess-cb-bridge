// The settings window: the databases oschess sees and the extra folders, then
// autostart, updates, the pairing code and the port.
'use strict';

let current = null;

async function main() {
  await loadWords();
  for (const node of document.querySelectorAll('[data-icon]')) {
    node.prepend(icon(node.dataset.icon, node.classList.contains('row') ? 20 : 16));
  }
  document.getElementById('add-folder').prepend(icon('folderPlus'));
  document.getElementById('copy-code').prepend(icon('copy'));
  document.getElementById('code-warning').prepend(icon('alert', 13, 2));
  for (const nav of document.querySelectorAll('.nav')) nav.addEventListener('click', () => show(nav.dataset.section));
  wire();
  show(new URLSearchParams(location.search).get('section') || 'databases');
  on('section', show);
  renderServed(await call('view'));
  on('view', renderServed);
  renderSettings(await call('settings'));
}

// `code` opens the general section with the pairing code shown.
function show(section) {
  const name = section === 'code' ? 'general' : section === 'general' ? 'general' : 'databases';
  for (const node of document.querySelectorAll('main > section')) node.hidden = node.id !== name;
  for (const nav of document.querySelectorAll('.nav')) {
    if (nav.dataset.section === name) nav.setAttribute('aria-current', 'page');
    else nav.removeAttribute('aria-current');
  }
  if (section === 'code') revealCode(true);
}

function renderServed(view) {
  const rows = view.databases.map((db) => {
    const line = stateLine(db);
    const name = el('div', 'path', db.name);
    name.title = db.name;
    return el('div', 'row', icon('db', 20), el('div', 'text', name, line ? el('div', 'cap12', line) : null),
      progressBar(db), stateChip(db));
  });
  if (view.problem) {
    const { title, body } = problemText(view.problem);
    rows.unshift(el('div', 'row', icon('alert', 20), el('div', 'text', el('div', null, title), el('div', 'cap12', body))));
  } else if (!rows.length) {
    rows.push(el('div', 'row empty-row', t('settings.served.none')));
  }
  document.getElementById('served').replaceChildren(...rows);
}

function renderSettings(settings) {
  current = settings;
  document.getElementById('version').textContent = t('settings.version', { version: settings.version });
  document.getElementById('version-title').textContent = t('settings.versionRow.title', { version: settings.version });
  // A build without a real updater key does not look for updates.
  document.getElementById('version-hint').textContent =
    settings.updates ? t('settings.versionRow.hint') : t('settings.versionRow.off');
  document.getElementById('check-updates').disabled = !settings.updates;
  document.getElementById('port-title').textContent = t('settings.port.title', { port: settings.port });
  toggle('autostart', settings.autostart);
  toggle('auto-update', settings.autoUpdate);
  const rows = settings.extras.map((extra) => {
    const detail = !extra.present
      ? t('settings.folders.missing')
      : extra.folder ? plural('settings.folders.count', extra.databases ?? 0) : t('settings.folders.file');
    const path = el('div', 'path selectable', extra.path);
    path.title = extra.path;
    const remove = el('button', 'btn', t('settings.folders.remove'));
    remove.addEventListener('click', () => act(call('remove_database', { path: extra.path }).then(renderSettings)));
    return el('div', 'row', icon(extra.folder ? 'folder' : 'db', 20), el('div', 'text', path, el('div', 'cap12', detail)), remove);
  });
  if (!rows.length) rows.push(el('div', 'row empty-row', t('settings.folders.none')));
  document.getElementById('folders').replaceChildren(...rows);
}

function toggle(id, onNow) {
  document.getElementById(id).setAttribute('aria-checked', String(onNow));
  document.getElementById(`${id}-state`).textContent = onNow ? t('settings.on') : t('settings.off');
}

function wire() {
  document.getElementById('add-folder').addEventListener('click', () =>
    act(call('add_folder').then((settings) => settings && renderSettings(settings))));
  document.getElementById('autostart').addEventListener('click', () =>
    act(call('set_autostart', { on: !current.autostart }).then(renderSettings)));
  document.getElementById('auto-update').addEventListener('click', () =>
    act(call('set_auto_update', { on: !current.autoUpdate }).then(renderSettings)));
  // The outcome comes as a Windows notification.
  document.getElementById('check-updates').addEventListener('click', () =>
    act(call('check_updates').then(() => notice(t('settings.versionRow.checking')))));

  document.getElementById('show-code').addEventListener('click', () =>
    revealCode(document.getElementById('code-line').hidden));
  document.getElementById('copy-code').addEventListener('click', () => act(call('copy_code').then(() => {
    const label = document.querySelector('#copy-code span');
    label.textContent = t('settings.code.copied');
    setTimeout(() => { label.textContent = t('settings.code.copy'); }, 2000);
  })));
  document.getElementById('new-code').addEventListener('click', () => {
    document.getElementById('confirm').hidden = false;
  });
  document.getElementById('new-code-no').addEventListener('click', () => {
    document.getElementById('confirm').hidden = true;
  });
  document.getElementById('new-code-yes').addEventListener('click', () =>
    act(call('new_code').then(restarting)));

  const form = document.getElementById('port-form');
  const input = document.getElementById('port-input');
  const error = document.getElementById('port-error');
  document.getElementById('port-change').addEventListener('click', () => {
    form.hidden = false;
    input.value = String(current.port);
    input.focus();
    input.select();
  });
  document.getElementById('port-cancel').addEventListener('click', () => {
    form.hidden = true;
    error.hidden = true;
  });
  form.addEventListener('submit', (e) => {
    e.preventDefault();
    call('set_port', { port: input.value }).then(restarting, (message) => {
      error.textContent = t(String(message));
      error.hidden = false;
    });
  });
}

async function revealCode(shown) {
  const line = document.getElementById('code-line');
  const button = document.getElementById('show-code');
  if (shown) {
    try {
      const pairing = await call('pairing_code');
      document.getElementById('code').textContent = pairing.code;
    } catch (message) {
      notice(t('settings.error', { message }));
      return;
    }
  }
  line.hidden = !shown;
  button.textContent = shown ? t('settings.code.hide') : t('settings.code.show');
}

function restarting() {
  notice(t('settings.restarting'));
}

function notice(text) {
  const node = document.getElementById('notice');
  node.textContent = text;
  node.hidden = false;
  clearTimeout(notice.timer);
  notice.timer = setTimeout(() => { node.hidden = true; }, 6000);
}

function act(promise) {
  return promise.catch((message) => notice(t('settings.error', { message })));
}

main();
