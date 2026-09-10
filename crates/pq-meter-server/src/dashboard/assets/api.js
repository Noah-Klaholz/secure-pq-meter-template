// Data access is independent of views. Historical queries can be added here later.
export async function fetchState() {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch('/api/v1/state', { cache: 'no-store', signal: controller.signal });
    if (!response.ok) throw new Error(`Receiver returned HTTP ${response.status}`);
    const snapshot = await response.json();
    if (snapshot.schema_version !== 1) throw new Error('Unsupported dashboard API version');
    return snapshot;
  } finally {
    clearTimeout(timeout);
  }
}
