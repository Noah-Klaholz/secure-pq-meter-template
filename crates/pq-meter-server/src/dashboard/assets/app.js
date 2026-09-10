import { fetchState } from './api.js';
import { createOverview } from './overview.js';

// The shell owns polling/lifecycle; view modules only render a supplied read model.
const overview = createOverview();
const refresh = document.getElementById('refresh');
let snapshot = null;
let receivedAt = 0;
let error = null;
let pending = false;
let timer;

async function update() {
  if (pending) return;
  clearTimeout(timer);
  pending = true;
  refresh.disabled = true;
  try {
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
