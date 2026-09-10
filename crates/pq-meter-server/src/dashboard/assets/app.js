import { fetchHistory, fetchState } from './api.js';
import { createOverview } from './overview.js';

// The shell owns polling/lifecycle; view modules only render a supplied read model.
const overview = createOverview();
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
