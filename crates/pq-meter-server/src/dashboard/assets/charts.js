// Rolling line charts, drawn as inline SVG.
//
// The dashboard's content security policy allows scripts from this origin only, so there
// is no charting library to load: these are a few hundred bytes of path geometry, which is
// all a 60-second trend needs.

const SVG = 'http://www.w3.org/2000/svg';

// A unitless drawing space; the SVG scales itself to whatever box the CSS gives it.
const WIDTH = 100;
const HEIGHT = 32;

const element = (name, attributes) => {
  const node = document.createElementNS(SVG, name);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
  return node;
};

const isNumber = (value) => typeof value === 'number' && Number.isFinite(value);

// Pads a domain so lines never sit exactly on the edge, and keeps a flat series readable
// by giving it a band to sit in the middle of.
function domainOf(values, band) {
  const candidates = values.filter(isNumber);
  if (band) {
    if (isNumber(band.min)) candidates.push(band.min);
    if (isNumber(band.max)) candidates.push(band.max);
  }
  if (!candidates.length) return null;
  let min = Math.min(...candidates);
  let max = Math.max(...candidates);
  if (max - min < 1e-6) {
    const nudge = Math.max(Math.abs(max) * 0.01, 0.5);
    min -= nudge;
    max += nudge;
  }
  const padding = (max - min) * 0.08;
  return { min: min - padding, max: max + padding };
}

/**
 * Draws one chart.
 *
 * `series` is a list of `{ points, tone }`, where points are `{ at, value }` and a null
 * value is a gap rather than a zero — the meter reports quantities it cannot measure, and
 * a chart must not invent a reading for them.
 */
export function drawChart(svg, { series, band, window: windowMs, now }) {
  const everyValue = series.flatMap(one => one.points.map(point => point.value));
  const domain = domainOf(everyValue, band);
  svg.setAttribute('viewBox', `0 0 ${WIDTH} ${HEIGHT}`);
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.replaceChildren();

  if (!domain) {
    svg.append(element('line', {
      x1: 0, y1: HEIGHT / 2, x2: WIDTH, y2: HEIGHT / 2,
      class: 'chart-empty-line',
    }));
    return { min: null, max: null };
  }

  const start = now - windowMs;
  const x = (at) => ((at - start) / windowMs) * WIDTH;
  const y = (value) => HEIGHT - ((value - domain.min) / (domain.max - domain.min)) * HEIGHT;

  // The band the readings are judged against, drawn behind them.
  if (band && isNumber(band.min) && isNumber(band.max)) {
    const top = y(Math.min(band.max, domain.max));
    const bottom = y(Math.max(band.min, domain.min));
    svg.append(element('rect', {
      x: 0, y: top, width: WIDTH, height: Math.max(0, bottom - top), class: 'chart-band',
    }));
  }

  for (const { points, tone } of series) {
    // A null value breaks the line instead of joining across the gap.
    let path = '';
    let pen = 'M';
    for (const point of points) {
      if (!isNumber(point.value)) { pen = 'M'; continue; }
      path += `${pen}${x(point.at).toFixed(2)},${y(point.value).toFixed(2)} `;
      pen = 'L';
    }
    if (path) svg.append(element('path', { d: path.trim(), class: `chart-line ${tone}` }));
  }

  return domain;
}
