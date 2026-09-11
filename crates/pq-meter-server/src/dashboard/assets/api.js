// Data access is independent of views.
async function get(path, expectedVersion) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch(path, { cache: 'no-store', signal: controller.signal });
    if (!response.ok) throw new Error(`Receiver returned HTTP ${response.status}`);
    const payload = await response.json();
    if (payload.schema_version !== expectedVersion) throw new Error('Unsupported dashboard API version');
    return payload;
  } finally {
    clearTimeout(timeout);
  }
}

export const fetchState = () => get('/api/v1/state', 1);

// The chart series is kept apart from the live snapshot: it grows with the window, while
// the snapshot is polled every second and has to stay small.
export const fetchHistory = () => get('/api/v1/history', 1);

export async function renameDevice(id, name) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch(`/api/v1/devices/${encodeURIComponent(id)}/label`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name }),
      signal: controller.signal,
    });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || 'Could not save the device name.');
    return payload.name;
  } finally {
    clearTimeout(timeout);
  }
}

export async function syncSettings(config) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch('/api/v1/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config),
      signal: controller.signal,
    });
    const payload = await response.json();
    if (!response.ok) throw new Error(payload.error || 'Failed to queue settings.');
    return payload;
  } finally {
    clearTimeout(timeout);
  }
}
