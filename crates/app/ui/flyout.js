// The flyout by the tray mark: the bridge's state and its databases.
'use strict';

async function main() {
  await loadWords();
  document.getElementById('settings').append(icon('gear'));
  document.getElementById('open').prepend(icon('open', 16));
  document.getElementById('settings').addEventListener('click', () => call('open_settings', { section: 'databases' }));
  document.getElementById('open').addEventListener('click', () => call('open_oschess'));
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') call('hide_flyout');
  });
  // The window follows the panel's height, whatever changes it.
  const panel = document.getElementById('panel');
  new ResizeObserver(() => call('fit_flyout', { height: panel.offsetHeight })).observe(panel);
  render(await call('view'));
  on('view', render);
}

function render(view) {
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
