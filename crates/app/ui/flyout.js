// The flyout by the tray mark: the bridge's state and its databases.
'use strict';

let currentView = null;

function retranslate() {
  if (currentView) render(currentView);
}

async function main() {
  // The buttons work before the words load: a failure there must not leave
  // the gear without its click (#332).
  document.getElementById('settings').addEventListener('click', () =>
    call('open_settings', { section: 'databases' }).catch(reportError));
  document.getElementById('open').addEventListener('click', () => call('open_oschess').catch(reportError));
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') call('hide_flyout');
  });
  await loadWords();
  document.getElementById('settings').append(icon('gear'));
  document.getElementById('open').prepend(icon('open', 16));
  // The window follows the panel's height, whatever changes it.
  const panel = document.getElementById('panel');
  new ResizeObserver(() => call('fit_flyout', { height: panel.offsetHeight })).observe(panel);
  render(await call('view'));
  on('view', render);
  // The app follows each window's opening up to here (#332).
  call('window_ready');
}

function render(view) {
  currentView = view;
  document.getElementById('version').textContent = view.version;
  const downloading = view.databases.filter((d) => d.state === 'downloading').length;
  // The tray mark's colour, decided by the app.
  document.getElementById('dot').className = `dot${view.mark === 'ready' ? '' : ` ${view.mark}`}`;
  let status = view.problem ? t('flyout.notWorking') : t('flyout.working');
  if (!view.problem && downloading) status += ` · ${plural('flyout.downloading', downloading)}`;
  document.getElementById('status').textContent = status;

  const card = document.getElementById('card');
  const { title, body } = view.problem
    ? problemText(view.problem)
    : { title: t('flyout.serving.title'), body: t('flyout.serving.body', { port: view.port }) };
  card.className = `card${view.problem ? ' problem' : ''}`;
  document.getElementById('card-icon').replaceChildren(icon(view.problem ? 'alert' : 'check', 16, 2.4));
  document.getElementById('card-title').textContent = title;
  document.getElementById('card-body').textContent = body;

  const none = view.databases.length === 0;
  document.getElementById('count').textContent = none ? '' : number(view.databases.length);
  document.getElementById('list').replaceChildren(...view.databases.map(row));
  document.getElementById('list-head').hidden = none && !!view.problem;
  document.getElementById('empty').hidden = !none || !!view.problem;
}

function row(db) {
  const line = stateLine(db);
  const name = el('span', 'name', db.name);
  name.title = db.name;
  return el('div', 'row',
    icon('db'),
    el('div', 'grow',
      el('div', 'row-top', name, stateChip(db)),
      line ? el('div', 'cap12', line) : null,
      progressBar(db)));
}

main();
