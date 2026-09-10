const number = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });
const integer = new Intl.NumberFormat();
const time = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit', second: '2-digit' });
const text = (id, value) => { document.getElementById(id).textContent = value; };
const watts = (value) => value == null ? '—' : number.format(value);

function badge(element, label, tone) {
  element.textContent = label;
  element.className = `badge ${tone}`;
}

export function createOverview() {
  let deviceSignature = '';
  return {
    render(snapshot) {
      const hasReading = snapshot.total_power_watts !== null;
      const active = snapshot.devices.filter(device => device.active).length;
      text('total-power', watts(snapshot.total_power_watts));
      text('active-count', hasReading ? integer.format(active) : '—');
      text('catalog-count', ` / ${snapshot.devices.length}`);
      text('inferred-power', hasReading ? watts(snapshot.inferred_power_watts) : '—');
      text('readings-count', integer.format(snapshot.readings_received));
      text('device-summary', `${snapshot.devices.length} catalog entries`);
      text('method', snapshot.decision_method === 'settled' ? 'Settled power match' : snapshot.decision_method === 'multi-feature' ? 'Multi-feature PQ match (P-Q-THD)' : 'Immediate power match');
      text('last-reading', snapshot.last_received_at ? time.format(new Date(snapshot.last_received_at)) : 'No readings yet');
      text('last-sync', `Synchronized ${time.format(new Date(snapshot.generated_at))}`);

      if (snapshot.latest_reading?.l1) {
        const l1 = snapshot.latest_reading.l1;
        const qSign = l1.reactive_power_var > 0 ? '+' : '';
        text('total-power-sub', `Q: ${qSign}${watts(l1.reactive_power_var)} var · THD: ${watts(l1.thd_current_pct)}% · cos φ: ${number.format(l1.cos_phi)}`);
      } else {
        text('total-power-sub', 'Latest accepted meter reading');
      }

      const signature = JSON.stringify([hasReading, snapshot.devices]);
      if (signature !== deviceSignature) {
        deviceSignature = signature;
        const rows = snapshot.devices.map(device => {
          const row = document.createElement('tr');
          const name = document.createElement('td');
          name.textContent = device.name;
          const id = document.createElement('span');
          id.className = 'device-id';
          id.textContent = device.id;
          name.append(id);
          const power = document.createElement('td');
          if (device.reactive_power_var != null || device.thd_current_pct != null) {
            const pqParts = [];
            if (device.reactive_power_var != null) pqParts.push(`${device.reactive_power_var > 0 ? '+' : ''}${watts(device.reactive_power_var)} var`);
            if (device.thd_current_pct != null) pqParts.push(`${watts(device.thd_current_pct)}% THD`);
            power.innerHTML = `${watts(device.nominal_power_watts)} W <span style="display:block;font-size:0.82em;color:var(--muted);margin-top:2px;">${pqParts.join(' · ')}</span>`;
          } else {
            power.textContent = `${watts(device.nominal_power_watts)} W`;
          }
          const state = document.createElement('td');
          const status = document.createElement('span');
          badge(status, !hasReading ? 'Awaiting data' : device.active ? 'Active' : 'Not detected', device.active ? 'positive' : 'neutral');
          state.append(status);
          row.append(name, power, state);
          return row;
        });
        if (!rows.length) {
          const row = document.createElement('tr');
          const cell = document.createElement('td');
          cell.colSpan = 3;
          cell.className = 'empty';
          cell.textContent = 'No devices in the catalog.';
          row.append(cell);
          rows.push(row);
        }
        document.getElementById('devices').replaceChildren(...rows);
      }

      const change = snapshot.last_change;
      badge(document.getElementById('change-kind'), change ? (change.kind === 'added' ? 'Device added' : 'Device removed') : 'No change yet', change?.kind === 'added' ? 'positive' : 'neutral');
      text('change-device', change ? change.device_name : 'Waiting for a match');
      const pqChangeParts = [];
      if (change?.reactive_power_var != null) pqChangeParts.push(`${change.reactive_power_var > 0 ? '+' : ''}${watts(change.reactive_power_var)} var`);
      if (change?.thd_current_pct != null) pqChangeParts.push(`${watts(change.thd_current_pct)}% THD`);
      const pqDetail = pqChangeParts.length ? ` (${pqChangeParts.join(', ')})` : '';
      text('change-detail', change ? `${watts(change.nominal_power_watts)} W${pqDetail} nominal power · ${change.kind === 'added' ? 'Inferred active' : 'No longer detected'}` : 'Detected additions and removals will appear here.');
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
          message = 'Receiver is ready. Connect the gateway to see measurements and inferred device activity.';
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
      badge(document.getElementById('connection'), label, tone);
      const notice = document.getElementById('notice');
      notice.hidden = !message;
      if (notice.textContent !== message) notice.textContent = message;
    },
  };
}
