// The first-run window. The app has already opened oschess with the pairing
// link; this window says so and offers the code for pasting by hand. Once a
// paired browser reaches the bridge, it says oschess is connected instead.
'use strict';

async function main() {
  await loadWords();
  document.getElementById('opening').prepend(icon('spin', 18, 2.4));
  document.getElementById('open').prepend(icon('open'));
  document.querySelector('summary').prepend(icon('chevron', 14, 2));
  document.getElementById('copy').prepend(icon('copy'));
  document.getElementById('open').addEventListener('click', () => call('open_pairing'));
  document.getElementById('close').addEventListener('click', () => window.__TAURI__.window.getCurrentWindow().close());
  document.getElementById('copy').addEventListener('click', () => call('copy_code').then(() => {
    const label = document.querySelector('#copy span');
    label.textContent = t('firstRun.copied');
    setTimeout(() => { label.textContent = t('firstRun.copy'); }, 2000);
  }));
  const pairing = await call('pairing_code');
  document.getElementById('code').textContent = pairing.code;
  showConnected(await call('view'));
  on('view', showConnected);
}

// Swaps the waiting line for «connected» once, and folds the code away: it is
// no longer needed.
let connected = false;
function showConnected(view) {
  if (connected || !view.connected) return;
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

main();
