// The first-run window. The app has already opened oschess with the pairing
// link; this window says so and offers the code for pasting by hand.
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
}

main();
