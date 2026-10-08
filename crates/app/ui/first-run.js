// The first-run wizard (#201): the databases, the engine and start-up, each
// filled in with what the bridge found, then the pairing screen. The browser
// opens oschess with the pairing link only on that last screen, never while
// the user is on the steps. The steps call the settings window's commands, so
// everything stays changeable there. «Skip setup» applies what the steps
// would have shown (the marked engine, the start-up switches) and goes
// straight to pairing; the window's ✕ changes nothing and opens nothing.
'use strict';

const STEPS = ['databases', 'engine', 'startup'];
// Databases listed before «and N more»: the whole list is in the settings.
const SHOWN = 3;

let step = null;
let view = null;
let settings = null;
let engines = null;
// The engine the rows mark: the chosen one, else the first found. «Next»
// saves it, as the user sees it marked.
let picked = null;
// The wizard's own start-up choices, applied by «Done» only: starting with
// Windows is on unless Windows' own settings turned it off.
let autostartOn = true;
let autoUpdateOn = true;
// An engine check, a Stockfish install or a saved choice is running.
let busy = false;
let installing = false;
let paired = false;
// The engine folders' answer, which «Next» on the engine step and «Skip
// setup» wait for: until then no engine is marked, found or not.
let enginesRead = null;

async function main() {
  await loadWords();
  for (const node of document.querySelectorAll('[data-icon]')) node.prepend(icon(node.dataset.icon, 20));
  document.getElementById('add-folder').prepend(icon('folderPlus'));
  document.getElementById('install-stockfish').prepend(icon('download'));
  document.getElementById('pick-other').prepend(icon('folder'));
  document.getElementById('pick-engine').prepend(icon('folder'));
  document.getElementById('engine-checking').prepend(icon('spin', 14, 2.4));
  document.querySelector('.progress-line').prepend(icon('spin', 16, 2.4));
  document.getElementById('autostart-blocked').prepend(icon('alert', 13, 2));
  document.getElementById('opening').prepend(icon('spin', 18, 2.4));
  document.getElementById('open').prepend(icon('open'));
  document.querySelector('summary').prepend(icon('chevron', 14, 2));
  document.getElementById('copy').prepend(icon('copy'));
  wire();
  // Subscribe first, then read: a connection that arrives in between is heard
  // by the listener, and one that came before is in the view read after it.
  // The app stores each view before it announces it (#130).
  await on('view', showView);
  showView(await call('view'));
  renderSettings(await call('settings'));
  show('databases');
  // The engine folders take a moment to read: ahead of the step. A failure
  // leaves no engine marked, and the wizard goes on without one.
  enginesRead = call('engines').then(renderEngines, (error) => notice(failure(error)));
  renderEngineControls();
  enginesRead.then(() => {
    enginesRead = null;
    renderEngineControls();
    footer();
  });
}

function wire() {
  on('stockfish-progress', showInstallProgress);
  document.getElementById('next').addEventListener('click', next);
  document.getElementById('back').addEventListener('click', () => show(STEPS[STEPS.indexOf(step) - 1]));
  document.getElementById('skip').addEventListener('click', () => work(complete));
  document.getElementById('add-folder').addEventListener('click', () =>
    work(() => call('add_folder').then((s) => s && renderSettings(s))));
  document.getElementById('install-stockfish').addEventListener('click', installStockfish);
  document.getElementById('pick-other').addEventListener('click', pickEngine);
  document.getElementById('pick-engine').addEventListener('click', pickEngine);
  document.getElementById('autostart').addEventListener('click', () => {
    autostartOn = !autostartOn;
    renderStartup();
  });
  document.getElementById('auto-update').addEventListener('click', () => {
    autoUpdateOn = !autoUpdateOn;
    renderStartup();
  });
  document.getElementById('open').addEventListener('click', () => act(call('open_pairing')));
  document.getElementById('close').addEventListener('click', () => window.__TAURI__.window.getCurrentWindow().close());
  document.getElementById('copy').addEventListener('click', () => act(call('copy_code').then(() => {
    const label = document.querySelector('#copy span');
    label.textContent = t('firstRun.copied');
    setTimeout(() => { label.textContent = t('firstRun.copy'); }, 2000);
  })));
}

// Shows a step, or `done`: the summary and the pairing.
function show(name) {
  step = name;
  const at = STEPS.indexOf(name);
  const last = name === 'done';
  for (const section of document.querySelectorAll('main > section')) section.hidden = section.id !== name;
  for (const [i, item] of [...document.querySelectorAll('.steps li')].entries()) {
    const done = last || i < at;
    item.classList.toggle('done', done);
    item.querySelector('b').replaceChildren(done ? icon('check', 13, 2.6) : String(i + 1));
    if (i === at) item.setAttribute('aria-current', 'step');
    else item.removeAttribute('aria-current');
  }
  document.getElementById('title').textContent = t(last ? 'wizard.done.title' : 'wizard.title');
  document.getElementById('subtitle').textContent = t(last ? 'wizard.done.subtitle' : 'wizard.subtitle');
  document.getElementById('next').textContent = t(name === 'startup' ? 'wizard.finish' : 'wizard.next');
  for (const id of ['skip', 'next']) document.getElementById(id).hidden = last;
  document.getElementById('back').hidden = last || at === 0;
  for (const id of ['close', 'open']) document.getElementById(id).hidden = !last;
  document.querySelector('.body').scrollTop = 0;
  footer();
  if (last) pair();
}

// The footer's note and which buttons work.
function footer() {
  let note = '';
  if (step === 'databases' && view && !view.databases.length && settings && !settings.extras.length) {
    note = t('wizard.databases.later');
  } else if (step === 'engine' && enginesRead) {
    note = t('wizard.engine.looking');
  } else if (step === 'engine' && installing) {
    note = t('wizard.engine.installing');
  } else if (step === 'engine' && !picked) {
    note = t('wizard.engine.later');
  }
  document.getElementById('note').textContent = note;
  for (const id of ['back', 'skip']) document.getElementById(id).disabled = busy;
  document.getElementById('next').disabled = busy || (step === 'engine' && Boolean(enginesRead));
}

// «Next» saves what the step shows: the marked engine on step 2, the
// start-up choices on step 3.
function next() {
  if (step === 'databases') show('engine');
  else if (step === 'engine') work(() => chooseMarked().then((chosen) => chosen && show('startup')));
  else if (step === 'startup') work(complete);
}

// Saves the marked engine unless it is the chosen one already. Resolves to
// whether the step may be left: a refused engine keeps the user on it.
function chooseMarked() {
  if (!picked || (engines && picked === engines.chosen)) return Promise.resolve(true);
  return choose(picked);
}

// «Done», and «Skip setup» from any step: once the engine folders answered,
// saves the marked engine (a refused one leaves none) and the start-up
// switches as they stand, then goes to pairing.
async function complete() {
  await enginesRead;
  await chooseMarked();
  await applyStartup();
  show('done');
}

// The bridge's view: the databases, and whether a browser has connected.
function showView(next) {
  view = next;
  const rows = view.databases.slice(0, SHOWN).map(databaseRow);
  const more = view.databases.length - SHOWN;
  if (more > 0) rows.push(el('div', 'row more', plural('wizard.databases.more', more)));
  if (view.problem) rows.unshift(problemRow(view.problem));
  document.getElementById('served').replaceChildren(...rows);
  const none = !view.databases.length && !view.problem;
  document.getElementById('db-found').hidden = none;
  document.getElementById('db-none').hidden = !none;
  document.getElementById('db-lead').textContent = t(none ? 'wizard.databases.leadNone' : 'wizard.databases.lead');
  // With nothing found, adding a folder is the step's main action.
  document.getElementById('add-folder').classList.toggle('accent', none);
  if (step === 'done') renderSummary();
  if (step) footer();
  showConnected(view);
}

function renderSettings(next) {
  if (!settings) {
    autostartOn = !next.autostartBlocked;
    autoUpdateOn = next.autoUpdate;
  }
  settings = next;
  const rows = settings.extras.map((extra) => {
    const remove = el('button', 'btn', t('settings.folders.remove'));
    remove.addEventListener('click', () => work(() => call('remove_database', { path: extra.path }).then(renderSettings)));
    return extraRow(extra, remove);
  });
  document.getElementById('folders').replaceChildren(...rows);
  // The Microsoft Store's package looks for updates through the Store (#153).
  document.getElementById('auto-update-hint').textContent =
    t(settings.store ? 'settings.versionRow.store' : 'settings.autoUpdate.hint');
  renderStartup();
  if (step) footer();
}

// The engines found, one of them marked; an engine chosen by its file comes first.
function renderEngines(next) {
  engines = next;
  document.getElementById('install-hint').textContent =
    t('settings.engine.installHint', { version: engines.install.version, mb: engines.install.megabytes });
  const listed = engines.found.map((f) => {
    const origin = f.source === 'bridge' ? t('settings.engine.installedByBridge') : t('settings.engine.from', { source: f.source });
    return { name: f.name, path: f.path, detail: `${origin} · ${f.path}` };
  });
  if (engines.chosen && !engines.found.some((f) => f.path === engines.chosen)) {
    listed.unshift({ name: engines.chosenName, path: engines.chosen, detail: engines.chosen });
  }
  if (!listed.some((e) => e.path === picked)) picked = engines.chosen ?? (listed.length ? listed[0].path : null);
  document.getElementById('engines').replaceChildren(...listed.map(engineRow));
  renderInstall(listed.length === 0);
  document.getElementById('engine-found').hidden = listed.length === 0;
  if (step) footer();
}

// With no engine found, installing Stockfish is the step's main action. With
// engines found but none of them the pinned Stockfish or newer, the step
// recommends installing it above them, as the settings window's offer looks
// (#287); the list keeps its own «Choose a file…».
function renderInstall(none) {
  const card = document.getElementById('engine-install');
  const recommend = !none && engines.recommendInstall;
  card.hidden = !none && !recommend;
  card.classList.toggle('offer', recommend);
  const row = card.querySelector('.row');
  row.querySelector(':scope > svg')?.remove();
  row.prepend(icon(recommend ? 'download' : 'cpu', 20));
  const version = engines.install.version;
  document.getElementById('install-title').textContent =
    recommend ? t('wizard.engine.recommend.title', { version }) : t('settings.engine.none.title');
  document.getElementById('install-lead').textContent =
    recommend ? t('wizard.engine.recommend.hint', { version }) : t('settings.engine.none.hint');
  document.getElementById('pick-other').hidden = recommend;
}

function engineRow(engine) {
  const radio = el('input');
  radio.type = 'radio';
  radio.name = 'engine';
  radio.checked = engine.path === picked;
  radio.disabled = busy || Boolean(enginesRead);
  radio.addEventListener('change', () => {
    picked = engine.path;
  });
  const text = el('div', 'text', el('div', null, engine.name), el('div', 'cap12 path', engine.detail));
  text.lastChild.title = engine.detail;
  return el('label', 'row engine', radio, text);
}

// Chooses the engine at `path` once it answers as a UCI engine, which can take
// seconds (#73). A refusal names the dictionary key of its message. Resolves
// to whether it was chosen.
function choose(path) {
  const line = document.getElementById('engine-checking');
  line.hidden = false;
  return call('choose_engine', { path })
    .then((next) => {
      picked = path;
      renderEngines(next);
      return true;
    }, (error) => {
      notice(failure(error));
      return false;
    })
    .finally(() => {
      line.hidden = true;
    });
}

// The file dialog first; the engine is checked only once one is chosen.
function pickEngine() {
  work(() => call('pick_engine').then((path) => path && choose(path)));
}

// Installs the pinned Stockfish with its progress in place, and the bridge
// chooses it. «Next» waits for it.
function installStockfish() {
  if (busy) return;
  installing = true;
  document.getElementById('install-progress').hidden = false;
  showInstallProgress({ phase: 'downloading', doneMegabytes: 0, totalMegabytes: engines ? engines.install.megabytes : 0 });
  work(() => call('install_stockfish').then(
    (next) => {
      picked = next.chosen;
      renderEngines(next);
    },
    (error) => notice(failure(error)),
  )).finally(() => {
    installing = false;
    document.getElementById('install-progress').hidden = true;
    footer();
  });
}

// The app counts the megabytes, rounded as the build's size is.
function showInstallProgress(progress) {
  const { doneMegabytes: done, totalMegabytes: total } = progress;
  document.getElementById('install-text').textContent = progress.phase === 'downloading'
    ? t('settings.engine.progress.downloading', { done, total })
    : t(`settings.engine.progress.${progress.phase}`);
  const share = progress.phase === 'downloading' ? (total ? done / total : 0) : 1;
  document.getElementById('install-bar').style.width = `${Math.min(100, Math.floor(share * 100))}%`;
}

// The switches wait while choices are saved: a change then would be lost.
function renderStartup() {
  const blocked = Boolean(settings && settings.autostartBlocked);
  toggle('autostart', autostartOn && !blocked);
  document.getElementById('autostart').disabled = blocked || busy;
  document.getElementById('autostart-blocked').hidden = !blocked;
  toggle('auto-update', autoUpdateOn);
  document.getElementById('auto-update').disabled = busy;
}

function toggle(id, onNow) {
  document.getElementById(id).setAttribute('aria-checked', String(onNow));
  document.getElementById(`${id}-state`).textContent = onNow ? t('settings.on') : t('settings.off');
}

// Applies the start-up choices that differ from how things stand.
async function applyStartup() {
  if (!settings.autostartBlocked && autostartOn !== settings.autostart) {
    renderSettings(await call('set_autostart', { on: autostartOn }));
  }
  if (autoUpdateOn !== settings.autoUpdate) renderSettings(await call('set_auto_update', { on: autoUpdateOn }));
}

// The last screen opens oschess with the pairing link once, and shows the
// code for pasting by hand.
function pair() {
  renderSummary();
  if (!paired) {
    paired = true;
    act(call('open_pairing'));
  }
  act(call('pairing_code').then((pairing) => {
    document.getElementById('code').textContent = pairing.code;
  }));
}

function renderSummary() {
  const count = view ? view.databases.length : 0;
  const chosen = engines && engines.chosen;
  const found = chosen && engines.found.find((f) => f.path === chosen);
  const engineName = found ? found.name : chosen ? engines.chosenName : t('wizard.done.noEngine');
  const engineHint = !chosen
    ? t('wizard.done.noEngineHint')
    : found && found.source === 'bridge' ? t('settings.engine.installedByBridge') : found ? t('settings.engine.from', { source: found.source }) : chosen;
  const starts = Boolean(settings && settings.autostart);
  const startsHint = settings && settings.autostartBlocked ? t('settings.autostart.blocked') : t('wizard.done.changeLater');
  document.getElementById('summary').replaceChildren(
    summaryRow('db', plural('wizard.done.databases', count), t('wizard.done.databasesHint'), count > 0),
    summaryRow('cpu', engineName, engineHint, Boolean(chosen)),
    summaryRow('power', t(starts ? 'wizard.done.autostartOn' : 'wizard.done.autostartOff'), startsHint, starts),
  );
}

function summaryRow(iconName, title, hint, fine) {
  const mark = fine ? icon('check', 18, 2.4) : icon('alert', 18, 2);
  if (!fine) mark.classList.add('warn');
  const text = el('div', 'text', el('div', null, title), el('div', 'cap12 path', hint));
  text.lastChild.title = hint;
  return el('div', 'row', icon(iconName, 20), text, mark);
}

// Swaps the waiting line for «connected» once, and folds the code away: it is
// no longer needed.
let connected = false;
function showConnected(next) {
  if (connected || !next.connected) return;
  connected = true;
  const opening = document.getElementById('opening');
  opening.classList.add('connected');
  opening.querySelector('svg').replaceWith(icon('check', 18, 2.4));
  document.getElementById('opening-title').textContent = t('firstRun.connected.title');
  document.getElementById('opening-hint').textContent = t('firstRun.connected.hint');
  document.querySelector('details').open = false;
  // Nothing is left to do here: closing becomes the main action.
  document.getElementById('open').classList.remove('accent');
  document.getElementById('close').classList.add('accent');
}

// Runs one thing at a time with the controls waiting: an engine check, an
// install, a folder dialog or the saved choices.
function work(task) {
  if (busy) return Promise.resolve();
  setBusy(true);
  return Promise.resolve().then(task).catch((error) => notice(failure(error))).finally(() => setBusy(false));
}

function setBusy(now) {
  busy = now;
  document.getElementById('add-folder').disabled = now;
  renderEngineControls();
  renderStartup();
  footer();
}

// The controls that change the engine wait for the engine folders' first
// answer as well as for any check, install or save: an answer that lands
// after a choice made meanwhile would replace it.
function renderEngineControls() {
  const locked = busy || Boolean(enginesRead);
  for (const input of document.querySelectorAll('#engines input')) input.disabled = locked;
  for (const id of ['install-stockfish', 'pick-other', 'pick-engine']) document.getElementById(id).disabled = locked;
}

function notice(text) {
  const node = document.getElementById('notice');
  node.textContent = text;
  node.hidden = false;
  clearTimeout(notice.timer);
  notice.timer = setTimeout(() => { node.hidden = true; }, 6000);
}

function act(promise) {
  return promise.catch((error) => notice(failure(error)));
}

main();
