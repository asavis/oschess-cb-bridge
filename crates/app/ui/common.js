// Shared by the three windows: the words in the window's language, the calls
// to the app, the icons, and how a database is shown. Text that comes from the
// bridge (database names, paths) is always set as text, never as markup.
'use strict';

const LANG = new URLSearchParams(location.search).get('lang') === 'en' ? 'en' : 'uk';
let WORDS = {};

async function loadWords() {
  const response = await fetch(`i18n/${LANG}.json`);
  WORDS = await response.json();
  document.documentElement.lang = LANG;
  translate(document);
}

function t(key, values = {}) {
  let text = WORDS[key] ?? key;
  for (const [name, value] of Object.entries(values)) {
    text = text.split(`{${name}}`).join(String(value));
  }
  return text;
}

// Ukrainian: 1, 21 … one; 2–4, 22–24 … few; the rest many. English: one, many.
function pluralForm(n) {
  if (LANG === 'en') return n === 1 ? 'one' : 'many';
  const [last, lastTwo] = [n % 10, n % 100];
  if (last === 1 && lastTwo !== 11) return 'one';
  if (last >= 2 && last <= 4 && (lastTwo < 12 || lastTwo > 14)) return 'few';
  return 'many';
}

function plural(key, n, values = {}) {
  return t(`${key}.${pluralForm(n)}`, { n: number(n), ...values });
}

function number(n, digits = 0) {
  return n.toLocaleString(LANG === 'uk' ? 'uk-UA' : 'en-GB', { maximumFractionDigits: digits });
}

// Bytes as megabytes or gigabytes, with one decimal.
function bytes(n) {
  const gb = n / 2 ** 30;
  return gb >= 1 ? t('size.gb', { n: number(gb, 1) }) : t('size.mb', { n: number(Math.max(n / 2 ** 20, 0.1), 1) });
}

// A download's whole percent, never 100 before the last byte.
function percent(progress) {
  if (progress.present >= progress.total) return 100;
  return Math.min(99, Math.floor((progress.present * 100) / progress.total));
}

// The bar of a download, or nothing when it has no progress yet.
function progressBar(db) {
  if (db.state !== 'downloading' || !db.progress) return null;
  const fill = el('i');
  fill.style.width = `${percent(db.progress)}%`;
  return el('div', 'bar', fill);
}

function translate(root) {
  for (const node of root.querySelectorAll('[data-i18n]')) node.textContent = t(node.dataset.i18n);
  for (const node of root.querySelectorAll('[data-i18n-title]')) node.title = t(node.dataset.i18nTitle);
  for (const node of root.querySelectorAll('[data-i18n-label]')) node.setAttribute('aria-label', t(node.dataset.i18nLabel));
}

function call(command, args = {}) {
  return window.__TAURI__.core.invoke(command, args);
}

function on(event, handler) {
  return window.__TAURI__.event.listen(event, (e) => handler(e.payload));
}

// Lucide icons (ISC licence), drawn in the text colour.
const ICONS = {
  alert: '<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
  cloud: '<path d="M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z"/>',
  cloudDown: '<path d="M12 13v8l-4-4"/><path d="m12 21 4-4"/><path d="M4.393 15.269A7 7 0 1 1 15.71 8h1.79a4.5 4.5 0 0 1 2.436 8.284"/>',
  copy: '<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
  cpu: '<rect width="16" height="16" x="4" y="4" rx="2"/><rect width="6" height="6" x="9" y="9" rx="1"/><path d="M15 2v2"/><path d="M15 20v2"/><path d="M2 15h2"/><path d="M2 9h2"/><path d="M20 15h2"/><path d="M20 9h2"/><path d="M9 2v2"/><path d="M9 20v2"/>',
  db: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5V19A9 3 0 0 0 21 19V5"/><path d="M3 12A9 3 0 0 0 21 12"/>',
  file: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/><path d="M10 9H8"/><path d="M16 13H8"/><path d="M16 17H8"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  folderPlus: '<path d="M12 10v6"/><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  gear: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  info: '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>',
  key: '<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5"/>',
  open: '<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>',
  port: '<path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/>',
  power: '<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.77.04"/>',
  refresh: '<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>',
  spin: '<path d="M21 12a9 9 0 1 1-6.219-8.56"/>',
};

function icon(name, size = 16, width = 1.75) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  for (const [key, value] of Object.entries({
    width: size, height: size, viewBox: '0 0 24 24', fill: 'none', stroke: 'currentColor',
    'stroke-width': width, 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true',
  })) svg.setAttribute(key, String(value));
  svg.innerHTML = ICONS[name] ?? '';
  if (name === 'spin') svg.classList.add('spin');
  return svg;
}

// An element with a class and children: nodes, or strings set as text.
function el(tag, className, ...children) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  for (const child of children) {
    if (child === null || child === undefined) continue;
    node.append(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

function chip(kind, iconName, text) {
  return el('span', `chip ${kind}`, iconName ? icon(iconName, 12, 2.2) : null, text);
}

// The state of a database as a small label.
function stateChip(db) {
  switch (db.state) {
    case 'ready': return chip('ready', 'check', t('db.chip.ready'));
    case 'cloudOnly': return chip('cloudOnly', 'cloud', t('db.chip.cloudOnly'));
    case 'downloading':
      return chip('downloading', 'cloudDown', db.progress ? t('db.percent', { n: percent(db.progress) }) : t('db.chip.downloading'));
    case 'opening': return chip('neutral', 'spin', t('db.chip.opening'));
    case 'missing': return chip('neutral', 'alert', t('db.chip.missing'));
    case 'unreadable': return chip('bad', 'alert', t('db.chip.unreadable'));
    case 'unsupported':
      return db.format === 'pgn' ? chip('neutral', 'file', t('db.chip.pgn')) : chip('neutral', 'file', t('db.chip.unsupported'));
    default: return chip('neutral', null, db.state);
  }
}

// The line under a database's name.
function stateLine(db) {
  switch (db.state) {
    case 'ready': return db.records === null || db.records === undefined ? '' : plural('db.records', db.records);
    case 'cloudOnly': return db.size ? t('db.sub.cloudOnlySized', { size: bytes(db.size) }) : t('db.sub.cloudOnly');
    case 'downloading':
      return db.progress
        ? t('db.sub.downloadingOf', { present: bytes(db.progress.present), total: bytes(db.progress.total) })
        : t('db.sub.downloading');
    case 'opening': return t('db.sub.opening');
    case 'missing': return t('db.sub.missing');
    case 'unreadable': return t('db.sub.unreadable');
    case 'unsupported': return db.format === 'pgn' ? t('db.sub.pgn') : t('db.sub.unsupported');
    default: return '';
  }
}

function problemText(problem) {
  if (problem.kind === 'portBusy') {
    return { title: t('flyout.portBusy.title', { port: problem.port }), body: t('flyout.portBusy.body') };
  }
  return { title: t('flyout.stopped.title'), body: t('flyout.stopped.body') };
}
