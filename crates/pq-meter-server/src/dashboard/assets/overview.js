import { drawChart } from './charts.js';

const number = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const time = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const integer = new Intl.NumberFormat();

const byId = (id) => document.getElementById(id);
const text = (id, value) => { byId(id).textContent = value; };

// A quantity the meter could not determine is shown as such. Rendering it as 0 would
// claim a measurement that was never taken.
const show = (value, digits = 1) =>
  value == null || !Number.isFinite(value)
    ? 'n/a'
    : new Intl.NumberFormat(undefined, { minimumFractionDigits: digits, maximumFractionDigits: digits }).format(value);

function badge(element, label, tone) {
  element.textContent = label;
  element.className = `badge ${tone}`;
}

/** Indexes the receiver's violations so a cell can ask whether it is one. */
function violationIndex(violations) {
  const index = new Map();
  for (const violation of violations ?? []) {
    index.set(`${violation.phase}:${violation.quantity}`, violation);
  }
  return index;
}

function cell(row, value, digits, unit, violation) {
  const td = document.createElement('td');
  td.className = 'numeric';
  const rendered = show(value, digits);
  td.textContent = rendered === 'n/a' ? 'n/a' : `${rendered}${unit}`;
  if (rendered === 'n/a') td.classList.add('unavailable');
  if (violation) {
    td.classList.add(violation.severity === 'violation' ? 'breach' : 'watch');
    td.title = violation.message;
  }
  row.append(td);
}

export function createOverview({ onRename } = {}) {
  let deviceSignature = '';
  let history = { samples: [], window_seconds: 60 };

  function renderCharts(limits) {
    const windowMs = (history.window_seconds || 60) * 1000;
    // Anchor the window on the newest sample so a paused gateway leaves the trace in
    // place instead of sliding it out of view against the wall clock.
    const samples = history.samples ?? [];
    const now = samples.length ? Date.parse(samples[samples.length - 1].at) : Date.now();
    const at = (sample) => Date.parse(sample.at);

    const frequency = drawChart(byId('chart-frequency'), {
      series: [{ tone: 'l1', points: samples.map(s => ({ at: at(s), value: s.frequency_hz })) }],
      band: limits ? { min: limits.frequency_min_hz, max: limits.frequency_max_hz } : null,
      window: windowMs,
      now,
    });
    text('chart-frequency-min', frequency.min == null ? '—' : `${show(frequency.min, 2)} Hz`);
    text('chart-frequency-max', frequency.max == null ? '—' : `${show(frequency.max, 2)} Hz`);

    const voltage = drawChart(byId('chart-voltage'), {
      series: [0, 1, 2].map((phase, index) => ({
        tone: `l${index + 1}`,
        // Only phases that actually carry voltage are plotted, so the two unwired ones
        // do not pin the scale to zero and flatten the one being measured.
        points: samples.map(s => ({ at: at(s), value: s.voltage_v[phase] > 1 ? s.voltage_v[phase] : null })),
      })),
      band: limits ? { min: limits.voltage_min_v, max: limits.voltage_max_v } : null,
      window: windowMs,
      now,
    });
    text('chart-voltage-min', voltage.min == null ? '—' : `${show(voltage.min)} V`);
    text('chart-voltage-max', voltage.max == null ? '—' : `${show(voltage.max)} V`);

    const newest = samples[samples.length - 1];
    text('chart-frequency-now', newest ? `${show(newest.frequency_hz, 2)} Hz` : '—');
    text('chart-voltage-now', newest ? `${show(newest.voltage_v[0])} V` : '—');
    text('chart-power-now', newest ? `${show(newest.total_power_watts)} W` : '—');

    const powerSeries = [{ tone: 'power', points: samples.map(s => ({ at: at(s), value: s.total_power_watts })) }];
    let futureMs = 0;
    const legendForecast = byId('legend-forecast');
    const powerSubhead = byId('chart-power-subhead');

    if (history.forecast && Array.isArray(history.forecast.points) && history.forecast.points.length > 0) {
      futureMs = (history.forecast.horizon_seconds || 15) * 1000;
      const forecastPoints = history.forecast.points.map(p => ({ at: Date.parse(p.at), value: p.predicted_watts }));
      if (newest) {
        forecastPoints.unshift({ at: at(newest), value: newest.total_power_watts });
      }
      powerSeries.push({ tone: 'forecast', points: forecastPoints });
      if (legendForecast) {
        legendForecast.style.display = 'inline-flex';
        legendForecast.title = `${history.forecast.model_name || 'Online ML'}${history.forecast.mae != null ? ` (MAE: ${show(history.forecast.mae, 2)} W)` : ''}`;
      }
      if (powerSubhead) {
        const lastForecast = history.forecast.points[history.forecast.points.length - 1];
        powerSubhead.textContent = `Recorded ${history.window_seconds || 60} s + ${Math.round(futureMs / 1000)} s forecast (${show(lastForecast.predicted_watts)} W)`;
      }
    } else {
      if (legendForecast) legendForecast.style.display = 'none';
      if (powerSubhead) powerSubhead.textContent = 'Latest recorded 60 s · negative is export';
    }

    const power = drawChart(byId('chart-power'), {
      series: powerSeries,
      window: windowMs,
      future: futureMs,
      now,
    });
    text('chart-power-min', power.min == null ? '—' : `${show(power.min)} W`);
    text('chart-power-max', power.max == null ? '—' : `${show(power.max)} W`);
    text('phase-summary', samples.length ? `${integer.format(samples.length)} samples in the last ${history.window_seconds} s` : '—');
  }

  function renderRecentMeasurements(series) {
    const samples = [...(series.samples ?? [])]
      .sort((left, right) => Date.parse(right.at) - Date.parse(left.at))
      .slice(0, 10);
    const body = byId('recent-measurements');
    if (!samples.length) {
      const row = document.createElement('tr');
      const empty = document.createElement('td');
      empty.colSpan = 5;
      empty.className = 'empty';
      empty.textContent = 'No measurements yet';
      row.append(empty);
      body.replaceChildren(row);
      return;
    }

    const rows = samples.map(sample => {
      const row = document.createElement('tr');
      const values = [
        new Date(sample.at).toLocaleString(),
        show(sample.total_power_watts, 1) + ' W',
        show(sample.voltage_v?.[0], 1) + ' V',
        show(sample.current_a?.[0], 2) + ' A',
        show(sample.frequency_hz, 2) + ' Hz',
      ];
      for (const value of values) {
        const cell = document.createElement('td');
        cell.textContent = value;
        row.append(cell);
      }
      return row;
    });
    body.replaceChildren(...rows);
  }

  return {
    historyLoading() {
      text('history-status', 'Loading');
      const cell = document.querySelector('#recent-measurements .empty');
      if (cell) cell.textContent = 'Loading measurements…';
    },

    history(series) {
      history = series;
      text('history-status', series.persistent ? `${integer.format(series.stored_readings)} saved` : 'Session only');
      const latest = series.samples?.at(-1);
      text('history-summary', latest
        ? `Latest recorded window · Last measurement: ${new Date(latest.at).toLocaleString()}. ${series.persistent ? 'All accepted readings are kept in the archive.' : 'History is held in memory only.'}`
        : 'No saved measurements yet. New readings will appear here.');

      const forecastCard = byId('ml-forecast-card');
      if (forecastCard) {
        if (series.forecast && Array.isArray(series.forecast.points) && series.forecast.points.length > 0) {
          forecastCard.style.display = 'block';
          text('forecast-model-name', series.forecast.model_name || 'River Streaming Regressor');
          text('forecast-horizon', `+${Math.round(series.forecast.horizon_seconds || 15)} seconds`);
          const lastPoint = series.forecast.points[series.forecast.points.length - 1];
          text('forecast-next-value', `${show(lastPoint.predicted_watts)} W`);
          text('forecast-mae', series.forecast.mae != null ? `${show(series.forecast.mae, 2)} W` : 'Calibrating…');
        } else {
          forecastCard.style.display = 'none';
        }
      }
      renderRecentMeasurements(series);
    },

    historyError() {
      text('history-status', 'Unavailable');
      text('history-summary', 'Saved history is currently unavailable. Retrying automatically.');
      const row = document.createElement('tr');
      const cell = document.createElement('td');
      cell.colSpan = 5;
      cell.className = 'empty';
      cell.textContent = 'History is currently unavailable';
      row.append(cell);
      byId('recent-measurements').replaceChildren(row);
    },

    render(snapshot) {
      const pq = snapshot.power_quality ?? {};
      const limits = snapshot.limits;
      const violations = pq.violations ?? [];
      const index = violationIndex(violations);
      const hasReading = pq.total_power_watts != null;

      // Frequency, with the band it is judged against.
      const frequencyViolation = index.get('-:frequency_hz');
      text('frequency', show(pq.frequency_hz, 2));
      byId('frequency').className = frequencyViolation ? 'breach-text' : '';
      text('frequency-note', limits
        ? `Nominal ${show(limits.nominal_frequency_hz, 0)} Hz · allowed ${show(limits.frequency_min_hz, 1)}–${show(limits.frequency_max_hz, 1)} Hz`
        : 'Awaiting the first reading');

      // Net power, and which way it flows.
      text('net-power', show(pq.total_power_watts));
      const flow = pq.flow;
      badge(byId('flow-badge'),
        flow === 'export' ? 'Export' : flow === 'import' ? 'Import' : flow === 'balanced' ? 'Balanced' : '—',
        flow === 'export' ? 'positive' : flow === 'import' ? 'neutral' : 'neutral');
      text('net-power-note', hasReading
        ? `S ${show(pq.apparent_power_va)} VA · Q ${show(pq.reactive_power_var)} var`
        : 'Three-phase sum measured by the meter');

      // Supply status summarises the events below.
      const breaches = violations.filter(v => v.severity === 'violation').length;
      const warnings = violations.length - breaches;
      text('supply-status', !hasReading ? '—' : breaches ? `${breaches} violation${breaches === 1 ? '' : 's'}` : warnings ? `${warnings} warning${warnings === 1 ? '' : 's'}` : 'Within limits');
      byId('supply-status').className = breaches ? 'breach-text' : warnings ? 'watch-text' : '';
      text('supply-note', limits
        ? `Voltage ${show(limits.voltage_min_v, 0)}–${show(limits.voltage_max_v, 0)} V · THDᵤ max ${show(limits.thd_voltage_max_pct, 0)} %`
        : 'Checked against EN 50160 limits');

      // Phases.
      const phaseRows = (pq.phases ?? []).map(phase => {
        const row = document.createElement('tr');
        const name = document.createElement('td');
        name.textContent = phase.name;
        if (!phase.connected) {
          const note = document.createElement('span');
          note.className = 'device-id';
          note.textContent = 'no load';
          name.append(note);
          row.classList.add('idle-phase');
        }
        row.append(name);
        cell(row, phase.voltage_v, 1, ' V', index.get(`${phase.name}:voltage_v`));
        cell(row, phase.current_a, 2, ' A', null);
        cell(row, phase.real_power_w, 1, ' W', null);
        cell(row, phase.cos_phi, 2, '', null);
        cell(row, phase.thd_voltage_pct, 2, ' %', index.get(`${phase.name}:thd_voltage_pct`));
        cell(row, phase.thd_current_pct, 2, ' %', index.get(`${phase.name}:thd_current_pct`));
        return row;
      });
      if (!phaseRows.length) {
        const row = document.createElement('tr');
        const empty = document.createElement('td');
        empty.colSpan = 7;
        empty.className = 'empty';
        empty.textContent = 'Waiting for the receiver…';
        row.append(empty);
        phaseRows.push(row);
      }
      byId('phase-rows').replaceChildren(...phaseRows);

      // Events.
      badge(byId('violation-count'),
        !hasReading ? 'No data' : violations.length ? `${violations.length} active` : 'All clear',
        breaches ? 'negative' : warnings ? 'warning' : hasReading ? 'positive' : 'neutral');
      const events = violations.map(violation => {
        const item = document.createElement('li');
        item.className = violation.severity === 'violation' ? 'breach' : 'watch';
        const label = document.createElement('span');
        label.className = 'event-tag';
        label.textContent = violation.severity === 'violation' ? 'VIOLATION' : 'WARNING';
        const message = document.createElement('span');
        message.textContent = violation.message;
        item.append(label, message);
        return item;
      });
      if (!events.length) {
        const item = document.createElement('li');
        item.className = 'empty';
        item.textContent = hasReading ? 'All measured quantities are within their limits.' : 'Waiting for the receiver…';
        events.push(item);
      }
      byId('violations').replaceChildren(...events);
      if (limits) text('thd-current-note', `${show(limits.thd_current_relevant_a, 0)} A`);

      // Transport.
      const transport = snapshot.transport ?? {};
      const state = transport.state;
      text('link-state', state === 'live' ? 'Live' : state === 'stale' ? 'Stale' : 'Waiting');
      byId('link-state').className = state === 'stale' ? 'breach-text' : '';
      text('link-note', transport.seconds_since_last_reading == null
        ? 'No readings received yet'
        : `Last reading ${integer.format(transport.seconds_since_last_reading)} s ago`);
      text('transport-state', state === 'live' ? 'Live' : state === 'stale' ? 'Stale — no recent readings' : 'Waiting for the gateway');
      // Rendered as text, never markup: this string is reported by the peer.
      text('scion-path', transport.scion_path ?? (transport.gateway_reporting ? 'Not reported' : 'Gateway not reporting'));
      text('ack-latency', transport.last_ack_latency_ms == null ? '—' : `${show(transport.last_ack_latency_ms)} ms`);
      text('queued-readings', transport.queued_readings == null ? '—' : integer.format(transport.queued_readings));
      text('failover-count', transport.failover_count == null ? '—' : integer.format(transport.failover_count));
      text('modbus-reconnects', transport.modbus_reconnects == null ? '—' : integer.format(transport.modbus_reconnects));
      // Readings the gateway admits it lost. Anything above zero is a hole in the archive,
      // so it is flagged rather than shown as just another count.
      text('dropped-readings', transport.dropped_readings == null ? '—' : integer.format(transport.dropped_readings));
      byId('dropped-readings').className = transport.dropped_readings ? 'breach-text' : '';
      text('readings-count', integer.format(snapshot.readings_received));
      text('last-reading', snapshot.last_received_at ? time.format(new Date(snapshot.last_received_at)) : 'No readings yet');
      text('last-sync', `Synchronized ${time.format(new Date(snapshot.generated_at))}`);

      renderCharts(limits);

      // Connected devices.
      const active = snapshot.devices.filter(device => device.active).length;
      text('active-count', hasReading ? integer.format(active) : '—');
      text('catalog-count', ` / ${snapshot.devices.length}`);
      text('inferred-power', hasReading ? number.format(snapshot.inferred_power_watts === 0 ? 0 : snapshot.inferred_power_watts) : '—');
      text('device-summary', `${snapshot.devices.length} catalog entries`);
      text('method', snapshot.decision_method === 'settled' ? 'Settled power match' : snapshot.decision_method === 'multi-feature' ? 'Multi-feature PQ match (P-Q-THD)' : snapshot.decision_method === 'adaptive' ? 'Adaptive NILM · runtime learning' : 'Immediate power match');

      const signature = JSON.stringify([hasReading, snapshot.devices]);
      if (signature !== deviceSignature) {
        deviceSignature = signature;
        const rows = snapshot.devices.map(device => {
          const row = document.createElement('tr');
          const name = document.createElement('td');
          name.textContent = device.name;
          name.className = 'device-name';
          const id = document.createElement('span');
          id.className = 'device-id';
          id.textContent = device.id;
          name.append(id);
          const profile = document.createElement('td');
          profile.textContent = `${number.format(device.nominal_power_watts)} W`;
          const pqParts = [];
          if (device.reactive_power_var != null) pqParts.push(`${device.reactive_power_var > 0 ? '+' : ''}${number.format(device.reactive_power_var)} var`);
          if (device.thd_current_pct != null) pqParts.push(`${number.format(device.thd_current_pct)}% THD`);
          if (pqParts.length) {
            const detail = document.createElement('span');
            detail.className = 'device-id';
            detail.textContent = pqParts.join(' · ');
            profile.append(detail);
          }
          const status = document.createElement('td');
          const state = document.createElement('span');
          badge(state, !hasReading ? 'Awaiting data' : device.active ? 'Active' : 'Not detected', device.active ? 'positive' : 'neutral');
          status.append(state);
          const actions = document.createElement('td');
          const rename = document.createElement('button');
          rename.type = 'button';
          rename.textContent = 'Rename';
          rename.dataset.deviceId = device.id;
          rename.setAttribute('aria-label', `Rename ${device.name}`);
          rename.addEventListener('click', () => onRename?.(device));
          actions.append(rename);
          row.append(name, profile, status, actions);
          return row;
        });
        if (!rows.length) {
          const row = document.createElement('tr');
          const cell = document.createElement('td');
          cell.colSpan = 4;
          cell.className = 'empty';
          cell.textContent = 'No devices in the catalog.';
          row.append(cell);
          rows.push(row);
        }
        byId('devices').replaceChildren(...rows);
      }

      const change = snapshot.last_change;
      badge(byId('change-kind'), change ? (change.kind === 'added' ? 'Device added' : 'Device removed') : 'No change yet', change?.kind === 'added' ? 'positive' : 'neutral');
      text('change-device', change ? change.device_name : 'Waiting for a match');
      const pqChangeParts = [];
      if (change?.reactive_power_var != null) pqChangeParts.push(`${change.reactive_power_var > 0 ? '+' : ''}${number.format(change.reactive_power_var)} var`);
      if (change?.thd_current_pct != null) pqChangeParts.push(`${number.format(change.thd_current_pct)}% THD`);
      const pqDetail = pqChangeParts.length ? ` (${pqChangeParts.join(', ')})` : '';
      text('change-detail', change ? `${number.format(change.nominal_power_watts)} W${pqDetail} nominal power · ${change.kind === 'added' ? 'Inferred active' : 'No longer detected'}` : 'Detected additions and removals will appear here.');
      text('change-time', change ? new Date(change.received_at).toLocaleString() : '—');
    },

    connection(snapshot, error, elapsedSinceFetch = 0) {
      let label = 'Connecting', tone = 'neutral', message = 'Connecting to the local receiver…';
      if (error) {
        label = 'Disconnected'; tone = 'negative';
        message = `Cannot reach the dashboard API. ${snapshot ? 'Showing the last synchronized state. ' : ''}Retrying automatically.`;
      } else if (snapshot) {
        if (!snapshot.last_received_at) {
          label = 'Waiting for data';
          message = 'Receiver is ready. Connect the gateway to see measurements and the SCION link it delivers over.';
        } else {
          // Compare server timestamps, then advance with elapsed browser time. This avoids clock skew.
          const age = Math.max(0, Date.parse(snapshot.generated_at) - Date.parse(snapshot.last_received_at)) + elapsedSinceFetch;
          if (age > snapshot.stale_after_seconds * 1000) {
            label = 'Stale data'; tone = 'warning';
            message = `No accepted reading for ${Math.floor(age / 1000)} seconds. Showing the last known state; check the gateway connection.`;
          } else {
            label = 'Receiving data'; tone = 'positive'; message = '';
          }
        }
      }
      badge(byId('connection'), label, tone);
      const notice = byId('notice');
      notice.hidden = !message;
      if (notice.textContent !== message) notice.textContent = message;
    },
  };
}
