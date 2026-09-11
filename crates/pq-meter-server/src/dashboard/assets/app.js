import { fetchHistory, fetchState, renameDevice } from './api.js';
import { createOverview } from './overview.js';

// The shell owns polling/lifecycle; view modules only render a supplied read model.
const overview = createOverview({ onRename: openRename });
const refresh = document.getElementById('refresh');
let snapshot = null;
let receivedAt = 0;
let error = null;
let pending = false;
let timer;

// The series is larger than the snapshot and moves more slowly, so it is fetched less
// often. A failure to load it must not cost the live view.
const HISTORY_EVERY = 2;
let sinceHistory = HISTORY_EVERY;

async function update() {
  if (pending) return;
  clearTimeout(timer);
  pending = true;
  refresh.disabled = true;
  try {
    if (++sinceHistory >= HISTORY_EVERY) {
      sinceHistory = 0;
      overview.historyLoading();
      try {
        overview.history(await fetchHistory());
      } catch {
        overview.historyError();
      }
    }
    snapshot = await fetchState();
    receivedAt = performance.now();
    error = null;
    overview.render(snapshot);
  } catch (failure) {
    error = failure;
  } finally {
    pending = false;
    refresh.disabled = false;
    overview.connection(snapshot, error, performance.now() - receivedAt);
    if (!document.hidden) timer = setTimeout(update, 1000);
  }
}

refresh.addEventListener('click', update);
document.addEventListener('visibilitychange', () => {
  clearTimeout(timer);
  if (!document.hidden) update();
});
update();


// Hash navigation supports direct links, reloads, and the browser's back button.
const views = {
  'power-quality': ['Power Quality', 'POWER QUALITY / LIVE STATE', 'Three-phase measurements from the meter, carried over SCION.'],
  'connected-devices': ['Connected Devices', 'DEVICES / LIVE STATE', 'Recognized devices, saved names and inferred activity.'],
  history: ['History', 'MEASUREMENTS / SAVED HISTORY', 'Measurements are saved across restarts. Charts show the latest recorded 60 seconds.'],
};
function navigate(focus = false) {
  const requested = location.hash.slice(1);
  if (requested === 'main') return;
  const selected = Object.hasOwn(views, requested) ? requested : 'power-quality';
  for (const panel of document.querySelectorAll('.dashboard-view')) panel.hidden = panel.id !== selected;
  for (const link of document.querySelectorAll('.sidebar nav a')) {
    const active = link.hash === `#${selected}`;
    link.classList.toggle('selected', active);
    if (active) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  }
  const [title, eyebrow, subtitle] = views[selected];
  document.getElementById('page-title').textContent = title;
  document.getElementById('page-eyebrow').textContent = eyebrow;
  document.getElementById('page-subtitle').textContent = subtitle;
  document.title = `${title} · PQ Monitor`;
  if (snapshot) overview.render(snapshot);
  if (focus) document.getElementById('page-title').focus({ preventScroll: true });
}
window.addEventListener('hashchange', () => navigate(true));
navigate();

const dialog = document.getElementById('rename-dialog');
const nameInput = document.getElementById('device-name');
const save = document.getElementById('rename-save');
const cancel = document.getElementById('rename-cancel');
let editingDevice = null;
let saving = false;

function openRename(device) {
  editingDevice = device.id;
  nameInput.value = device.name;
  nameInput.setCustomValidity('');
  document.getElementById('rename-device-id').textContent = device.id;
  document.getElementById('rename-error').textContent = '';
  document.getElementById('device-save-status').textContent = '';
  dialog.showModal();
  nameInput.focus();
  nameInput.select();
}
nameInput.addEventListener('input', () => nameInput.setCustomValidity(''));
cancel.addEventListener('click', () => dialog.close());
dialog.addEventListener('cancel', event => { if (saving) event.preventDefault(); });
dialog.addEventListener('close', () => {
  const button = [...document.querySelectorAll('#devices button')].find(button => button.dataset.deviceId === editingDevice);
  button?.focus();
});
document.getElementById('rename-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (saving) return;
  const name = nameInput.value.trim();
  if (!name) {
    nameInput.setCustomValidity('Enter a device name.');
    nameInput.reportValidity();
    return;
  }
  saving = true;
  save.disabled = cancel.disabled = nameInput.disabled = true;
  save.textContent = 'Saving…';
  document.getElementById('rename-error').textContent = '';
  try {
    const savedName = await renameDevice(editingDevice, name);
    // Do not claim a failed save if a subsequent polling request fails.
    document.getElementById('device-save-status').textContent = `Saved name: ${savedName}`;
    dialog.close();
    await update();
  } catch (error) {
    document.getElementById('rename-error').textContent = error.name === 'AbortError'
      ? 'The save timed out. Please retry to confirm the name.'
      : error.message || 'Could not save the device name. Please retry.';
  } finally {
    saving = false;
    save.disabled = cancel.disabled = nameInput.disabled = false;
    save.textContent = 'Save name';
  }
});
