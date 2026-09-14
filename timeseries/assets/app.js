// Dashboard. No dependencies and no build step: one fetch loop, two canvases'
// worth of drawing, and the endpoints in ../src/web.rs.
//
// Two views. The overview is a wall of stat tiles, one per channel, each with a
// sparkline -- small multiples rather than one chart with thirty-one lines on
// it, because the channels are in different units and a shared axis would be a
// lie. The detail view is one channel at full size: the min/max band with the
// mean through it, the notes that explain it, and the same numbers as a table
// for anything a curve reads badly.
//
// Colour is read off the stylesheet rather than repeated here, so the two
// themes are defined in exactly one place.

const RANGES = [
  { key: "6h", label: "6 h", ms: 6 * 3600e3 },
  { key: "24h", label: "24 h", ms: 24 * 3600e3 },
  { key: "7d", label: "7 d", ms: 7 * 24 * 3600e3 },
  { key: "30d", label: "30 d", ms: 30 * 24 * 3600e3 },
  { key: "1y", label: "1 y", ms: 365 * 24 * 3600e3 },
  { key: "3y", label: "3 y", ms: 3 * 365 * 24 * 3600e3 },
];

const REFRESH_MS = 30000;
// `en-GB` rather than the browser's locale: a dashboard whose decimal point
// moves depending on who opens it makes two screenshots of the same reading
// disagree. Day before month, which is what everyone reading this expects.
const LOCALE = "en-GB";
const num = new Intl.NumberFormat(LOCALE, { maximumFractionDigits: 2 });
const num1 = new Intl.NumberFormat(LOCALE, { maximumFractionDigits: 1 });

const state = {
  range: "24h",
  // An absolute window, set by a zoom gesture, that overrides the preset. Null
  // means "the preset, relative to now", which is what keeps the page live: a
  // zoomed chart deliberately stops following the clock, because a window that
  // slides while you are reading it is not a window you can compare anything
  // against.
  zoom: null, // { from, to } in ms
  // Where a double-click goes back to: the view that was active before zooming
  // in, not the widest range there is. Set by choosing a preset, left alone by
  // the gestures themselves.
  zoomHome: null, // { range, zoom }
  drag: null, // { fromX, toX } while a selection is being dragged
  channels: [],
  selected: null, // { node, sensor }
  series: null,
  notes: [],
  hover: null,
  showTable: false,
};

/// Smallest window a gesture may produce. Below a minute the buckets are wider
/// than the window and the chart shows one point, which looks broken rather
/// than zoomed.
const MIN_SPAN_MS = 60e3;

const el = (id) => document.getElementById(id);
const dom = {
  ranges: el("ranges"),
  health: el("health"),
  overview: el("overview"),
  overviewHint: el("overview-hint"),
  nodes: el("nodes"),
  detail: el("detail"),
  title: el("chart-title"),
  sub: el("chart-sub"),
  stats: el("stats"),
  chart: el("chart"),
  tooltip: el("tooltip"),
  foot: el("chart-foot"),
  tableWrap: el("table-wrap"),
  table: el("value-table").querySelector("tbody"),
  toggleTable: el("toggle-table"),
  exportCsv: el("export-csv"),
  noteList: el("note-list"),
  dialog: el("note-dialog"),
};

// --- helpers ----------------------------------------------------------------

const esc = (s) =>
  String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );

const css = (name) => getComputedStyle(document.body).getPropertyValue(name).trim();

const range = () => RANGES.find((r) => r.key === state.range) || RANGES[1];

function window_() {
  if (state.zoom) return { ...state.zoom };
  const to = Date.now();
  return { from: to - range().ms, to };
}

/// Move to an absolute window, remembering where to go back to.
function zoomTo(from, to) {
  if (!(to - from >= MIN_SPAN_MS)) return;
  if (!state.zoomHome) state.zoomHome = { range: state.range, zoom: state.zoom };
  state.zoom = { from: Math.round(from), to: Math.round(to) };
  // The strip has to let go of its pressed button: none of the presets is what
  // is on screen any more, and a lit "24 h" over a six-hour window is a lie
  // told by the only control that claims to say what the window is.
  renderRanges();
  writeHash();
  loadDetail();
}

function zoomReset() {
  if (!state.zoomHome) return;
  state.range = state.zoomHome.range;
  state.zoom = state.zoomHome.zoom;
  state.zoomHome = null;
  renderRanges();
  writeHash();
  loadDetail();
}

async function getJSON(url, options) {
  const response = await fetch(url, options);
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || response.statusText);
  return body;
}

function label(c) {
  return c.name || c.sensor;
}

function withUnit(value, unit, fmt = num) {
  if (value === null || value === undefined) return "—";
  return `${fmt.format(value)}${unit ? ` ${unit}` : ""}`;
}

/// What the sparkline covers, in numbers: a line without a scale is a shape,
/// and a shape is not a reading.
function span(c) {
  if (!c.points || !c.points.length) return "no data";
  let lo = Infinity;
  let hi = -Infinity;
  for (const [, v] of c.points) {
    lo = Math.min(lo, v);
    hi = Math.max(hi, v);
  }
  return `${num1.format(lo)} – ${num1.format(hi)}`;
}

function ago(ms) {
  if (!ms) return "never";
  const mins = Math.round((Date.now() - ms) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins} min ago`;
  const hours = Math.round(mins / 60);
  if (hours < 48) return `${hours} h ago`;
  return `${Math.round(hours / 24)} d ago`;
}

function formatTime(ms, spanMs) {
  const d = new Date(ms);
  if (spanMs <= 2 * 24 * 3600e3)
    return d.toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
  if (spanMs <= 120 * 24 * 3600e3)
    return d.toLocaleDateString(LOCALE, { day: "2-digit", month: "2-digit" });
  return d.toLocaleDateString(LOCALE, { month: "2-digit", year: "2-digit" });
}

// --- routing ----------------------------------------------------------------

function readHash() {
  const parts = decodeURIComponent(location.hash.slice(1)).split("/");
  if (parts[0] === "c" && parts[1] && parts[2]) {
    state.selected = { node: parts[1], sensor: parts[2] };
  } else {
    state.selected = null;
  }
  const slot = parts[0] === "c" ? parts[3] : parts[1];
  // A zoomed window is `<from>-<to>` in epoch seconds. Seconds rather than
  // milliseconds only because three zeroes per number in a link people paste to
  // each other buy nothing; a second is far finer than any bucket here.
  const span = /^(\d{9,11})-(\d{9,11})$/.exec(slot || "");
  if (span) {
    state.zoom = { from: Number(span[1]) * 1000, to: Number(span[2]) * 1000 };
  } else if (RANGES.some((r) => r.key === slot)) {
    state.range = slot;
    state.zoom = null;
  }
}

function writeHash() {
  const slot = state.zoom
    ? `${Math.round(state.zoom.from / 1000)}-${Math.round(state.zoom.to / 1000)}`
    : state.range;
  const next = state.selected
    ? `#c/${encodeURIComponent(state.selected.node)}/${encodeURIComponent(state.selected.sensor)}/${slot}`
    : `#all/${slot}`;
  if (location.hash !== next) history.replaceState(null, "", next);
}

// --- data -------------------------------------------------------------------

async function loadOverview() {
  const { from, to } = window_();
  try {
    state.channels = await getJSON(`/api/overview?from=${from}&to=${to}&points=80`);
    dom.overviewHint.hidden = state.channels.length > 0;
  } catch (e) {
    dom.overviewHint.hidden = false;
    dom.overviewHint.innerHTML = `<span class="error">${esc(e.message)}</span>`;
    return;
  }
  renderOverview();
}

async function loadDetail() {
  if (!state.selected) return;
  const { from, to } = window_();
  const { node, sensor } = state.selected;
  const points = Math.max(200, Math.floor(dom.chart.clientWidth || 800));
  dom.foot.textContent = "Loading …";
  try {
    const query = new URLSearchParams({ node, sensor, from, to, points });
    const [series, notes] = await Promise.all([
      getJSON(`/api/series?${query}`),
      getJSON(`/api/annotations?from=${from}&to=${to}`),
    ]);
    state.series = series;
    state.notes = notes.filter((n) => !n.node || n.node === node || n.node === "fleet");
    renderDetail();
  } catch (e) {
    state.series = null;
    dom.foot.innerHTML = `<span class="error">${esc(e.message)}</span>`;
  }
}

async function loadHealth() {
  try {
    const h = await getJSON("/api/health");
    const pill = (ok, name) =>
      `<span><span class="dot ${ok ? "up" : "down"}"></span>${name} ${ok ? "connected" : "disconnected"}</span>`;
    dom.health.innerHTML =
      pill(h.broker_connected, "Broker") +
      pill(h.database_reachable, "QuestDB") +
      `<span><b>${num.format(h.rows_written)}</b> rows written</span>`;
    el("brand-sub").textContent = h.retention
      ? `raw data ${h.retention.toLowerCase()}, ${h.views.length} rollups`
      : "kept indefinitely";
  } catch (e) {
    dom.health.innerHTML = `<span class="error">${esc(e.message)}</span>`;
  }
}

// --- overview ---------------------------------------------------------------

function renderOverview() {
  const byNode = new Map();
  for (const c of state.channels) {
    if (!byNode.has(c.node)) byNode.set(c.node, []);
    byNode.get(c.node).push(c);
  }

  const parts = [];
  for (const [node, channels] of byNode) {
    const name = channels.find((c) => c.node_name)?.node_name || node;
    const online = channels.find((c) => c.online !== null && c.online !== undefined)?.online;
    // The dot is never alone: colour alone is exactly what a red/green pair
    // cannot carry.
    const state_ =
      online === undefined || online === null
        ? ""
        : `<span class="state"><span class="dot ${online ? "up" : "down"}"></span>${online ? "online" : "offline"}</span>`;
    parts.push(`<div class="node-head"><h3>${esc(name)}</h3>${state_}</div><div class="tiles">`);
    for (const c of channels) {
      const stale = c.last_at_ms && Date.now() - c.last_at_ms > 3600e3;
      parts.push(
        `<button class="tile" data-node="${esc(c.node)}" data-sensor="${esc(c.sensor)}">` +
          `<span class="label">${esc(label(c))}</span>` +
          `<span class="value">${esc(num.format(c.last_value ?? NaN).replace("NaN", "—"))}` +
          (c.unit ? `<span>${esc(c.unit)}</span>` : "") +
          `</span>` +
          `<canvas></canvas>` +
          `<span class="range${stale ? " stale" : ""}">${esc(span(c))} · ${esc(ago(c.last_at_ms))}</span>` +
          `</button>`,
      );
    }
    parts.push("</div>");
  }
  dom.nodes.innerHTML = parts.join("");

  const tiles = dom.nodes.querySelectorAll(".tile");
  tiles.forEach((tile) => {
    const c = state.channels.find(
      (x) => x.node === tile.dataset.node && x.sensor === tile.dataset.sensor,
    );
    drawSparkline(tile.querySelector("canvas"), c.points);
    tile.addEventListener("click", () => {
      state.selected = { node: c.node, sensor: c.sensor };
      writeHash();
      show();
    });
  });
}

function drawSparkline(canvas, points) {
  const ratio = window.devicePixelRatio || 1;
  const w = canvas.clientWidth || 180;
  const h = canvas.clientHeight || 40;
  canvas.width = w * ratio;
  canvas.height = h * ratio;
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, w, h);
  if (!points || points.length < 2) return;

  let lo = Infinity;
  let hi = -Infinity;
  for (const [, v] of points) {
    lo = Math.min(lo, v);
    hi = Math.max(hi, v);
  }
  if (lo === hi) {
    lo -= 1;
    hi += 1;
  }
  const t0 = points[0][0];
  const t1 = points[points.length - 1][0];
  const x = (t) => ((t - t0) / (t1 - t0 || 1)) * (w - 2) + 1;
  const y = (v) => h - 3 - ((v - lo) / (hi - lo)) * (h - 6);

  ctx.beginPath();
  points.forEach(([t, v], i) => (i ? ctx.lineTo(x(t), y(v)) : ctx.moveTo(x(t), y(v))));
  ctx.strokeStyle = css("--series");
  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  ctx.stroke();

  // The last point gets a dot: it is the number above the sparkline, and
  // showing where it sits in the line is the whole reason the line is there.
  const [lt, lv] = points[points.length - 1];
  ctx.beginPath();
  ctx.arc(x(lt), y(lv), 2.5, 0, Math.PI * 2);
  ctx.fillStyle = css("--series");
  ctx.fill();
}

// --- detail -----------------------------------------------------------------

function channel() {
  if (!state.selected) return null;
  return (
    state.channels.find(
      (c) => c.node === state.selected.node && c.sensor === state.selected.sensor,
    ) || { ...state.selected, name: state.selected.sensor, unit: "", node_name: "" }
  );
}

function renderDetail() {
  const c = channel();
  const s = state.series;
  dom.title.textContent = label(c);
  dom.sub.textContent = `${c.node_name || c.node} · ${c.sensor}${c.unit ? ` · ${c.unit}` : ""}`;

  dom.exportCsv.disabled = !s || !s.points.length;

  if (!s || !s.points.length) {
    dom.stats.innerHTML = "";
    dom.foot.textContent = s ? "No data in this range." : "";
    drawChart();
    renderNotes();
    return;
  }

  let lo = Infinity;
  let hi = -Infinity;
  let sum = 0;
  for (const p of s.points) {
    lo = Math.min(lo, p.lo);
    hi = Math.max(hi, p.hi);
    sum += p.av;
  }
  const stat = (name, value) =>
    `<div><dt>${name}</dt><dd>${esc(value)}</dd></div>`;
  dom.stats.innerHTML =
    stat("min", withUnit(lo, c.unit)) +
    stat("mean", withUnit(sum / s.points.length, c.unit)) +
    stat("max", withUnit(hi, c.unit)) +
    stat("latest", withUnit(s.points[s.points.length - 1].av, c.unit)) +
    stat("points", `${s.points.length} of ${s.bucket}`);

  // The range strip shows no pressed button while a zoom is active, which is
  // correct and also mysterious on its own. Say so, and say the way out.
  dom.foot.textContent =
    `from ${s.source}, ${new Date(s.from).toLocaleString(LOCALE)} – ${new Date(s.to).toLocaleString(LOCALE)}` +
    (state.zoom ? " · zoomed, double-click the chart to go back" : "");

  dom.table.innerHTML = s.points
    .map(
      (p) =>
        `<tr><td>${esc(new Date(p.t).toLocaleString(LOCALE))}</td><td>${esc(num1.format(p.lo))}</td>` +
        `<td>${esc(num1.format(p.av))}</td><td>${esc(num1.format(p.hi))}</td></tr>`,
    )
    .join("");

  drawChart();
  renderNotes();
}

function renderNotes() {
  if (!state.notes.length) {
    dom.noteList.innerHTML = `<li class="none">None — whatever happened in this range is written down nowhere.</li>`;
    return;
  }
  dom.noteList.innerHTML = state.notes
    .map(
      (n, i) =>
        `<li><time>${esc(new Date(n.at_ms).toLocaleString(LOCALE))}</time>` +
        `<span>${esc(n.note)}${n.node && n.node !== "fleet" ? "" : " <em>(fleet)</em>"}</span>` +
        `<button class="void" type="button" data-note="${i}" ` +
        `title="Take this note back">retract</button></li>`,
    )
    .join("");
}

// Retracted, not deleted: a note's timestamp is the table's designated column
// and cannot be changed, and QuestDB deletes no rows at all. A note filed at the
// wrong minute can therefore only be marked as not counting, with the corrected
// one written beside it. It stays in the database -- in a log of what happened,
// a visible correction is worth more than one that leaves no trace.
dom.noteList.addEventListener("click", async (ev) => {
  const button = ev.target.closest("button.void");
  if (!button) return;
  const note = state.notes[Number(button.dataset.note)];
  if (!note) return;
  if (!window.confirm(`Take this note back?\n\n${note.note}`)) return;
  button.disabled = true;
  try {
    await getJSON("/api/annotations/void", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ at_ms: note.at_ms, node: note.node }),
    });
    loadDetail();
  } catch (e) {
    button.disabled = false;
    dom.foot.innerHTML = `<span class="error">Note not retracted: ${esc(e.message)}</span>`;
  }
});

function drawChart() {
  const canvas = dom.chart;
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  canvas.width = width * ratio;
  canvas.height = height * ratio;
  const ctx = canvas.getContext("2d");
  ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
  ctx.clearRect(0, 0, width, height);

  const s = state.series;
  if (!s || !s.points.length) return;

  const pad = { left: 56, right: 14, top: 16, bottom: 28 };
  const plot = {
    x: pad.left,
    y: pad.top,
    w: Math.max(10, width - pad.left - pad.right),
    h: Math.max(10, height - pad.top - pad.bottom),
  };

  let lo = Infinity;
  let hi = -Infinity;
  for (const p of s.points) {
    lo = Math.min(lo, p.lo);
    hi = Math.max(hi, p.hi);
  }
  if (lo === hi) {
    lo -= 1;
    hi += 1;
  }
  const headroom = (hi - lo) * 0.1;
  lo -= headroom;
  hi += headroom;

  const sx = (t) => plot.x + ((t - s.from) / (s.to - s.from)) * plot.w;
  const sy = (v) => plot.y + plot.h - ((v - lo) / (hi - lo)) * plot.h;

  // Grid and axes stay recessive: they are scaffolding, not data.
  ctx.strokeStyle = css("--grid");
  ctx.fillStyle = css("--text-muted");
  ctx.lineWidth = 1;
  ctx.font = "11px system-ui, sans-serif";
  ctx.textAlign = "right";
  ctx.textBaseline = "middle";
  for (const tick of niceTicks(lo, hi, 5)) {
    const y = Math.round(sy(tick)) + 0.5;
    ctx.beginPath();
    ctx.moveTo(plot.x, y);
    ctx.lineTo(plot.x + plot.w, y);
    ctx.stroke();
    ctx.fillText(num.format(tick), plot.x - 8, y);
  }

  ctx.textBaseline = "top";
  const ticks = timeTicks(s.from, s.to, Math.max(2, Math.floor(plot.w / 110)));
  ticks.forEach((t, i) => {
    const x = sx(t);
    // Anchored inwards at the ends, or half the first and last labels hang off
    // the canvas.
    ctx.textAlign = x < plot.x + 20 ? "left" : x > plot.x + plot.w - 20 ? "right" : "center";
    ctx.fillText(formatTime(t, s.to - s.from), x, plot.y + plot.h + 7);
  });

  // The band first, the mean over it: the envelope is context for the line.
  ctx.beginPath();
  s.points.forEach((p, i) => {
    const x = sx(p.t);
    i === 0 ? ctx.moveTo(x, sy(p.hi)) : ctx.lineTo(x, sy(p.hi));
  });
  for (let i = s.points.length - 1; i >= 0; i--) ctx.lineTo(sx(s.points[i].t), sy(s.points[i].lo));
  ctx.closePath();
  ctx.fillStyle = css("--series-band");
  ctx.fill();

  ctx.beginPath();
  s.points.forEach((p, i) => {
    const x = sx(p.t);
    const y = sy(p.av);
    i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
  });
  ctx.strokeStyle = css("--series");
  ctx.lineWidth = 2;
  ctx.lineJoin = "round";
  ctx.stroke();

  // Notes, as the thing they are: a mark at a moment, not a series.
  ctx.setLineDash([3, 3]);
  ctx.strokeStyle = css("--text-muted");
  ctx.fillStyle = css("--text-muted");
  ctx.lineWidth = 1;
  for (const n of state.notes) {
    if (n.at_ms < s.from || n.at_ms > s.to) continue;
    const x = Math.round(sx(n.at_ms)) + 0.5;
    ctx.beginPath();
    ctx.moveTo(x, plot.y);
    ctx.lineTo(x, plot.y + plot.h);
    ctx.stroke();
    ctx.beginPath();
    ctx.moveTo(x - 4, plot.y);
    ctx.lineTo(x + 4, plot.y);
    ctx.lineTo(x, plot.y + 6);
    ctx.closePath();
    ctx.fill();
  }
  ctx.setLineDash([]);

  // The drag selection, over everything else: it is the thing being aimed.
  if (state.drag) {
    const a = Math.min(state.drag.fromX, state.drag.toX);
    const b = Math.max(state.drag.fromX, state.drag.toX);
    ctx.fillStyle = css("--series-band");
    ctx.fillRect(a, plot.y, b - a, plot.h);
    ctx.strokeStyle = css("--series");
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(Math.round(a) + 0.5, plot.y);
    ctx.lineTo(Math.round(a) + 0.5, plot.y + plot.h);
    ctx.moveTo(Math.round(b) + 0.5, plot.y);
    ctx.lineTo(Math.round(b) + 0.5, plot.y + plot.h);
    ctx.stroke();
  }

  if (state.hover) {
    const p = state.hover;
    ctx.strokeStyle = css("--text-muted");
    ctx.beginPath();
    ctx.moveTo(Math.round(sx(p.t)) + 0.5, plot.y);
    ctx.lineTo(Math.round(sx(p.t)) + 0.5, plot.y + plot.h);
    ctx.stroke();
    // >= 8 px so the marker is findable and the hit target is honest.
    ctx.beginPath();
    ctx.arc(sx(p.t), sy(p.av), 4.5, 0, Math.PI * 2);
    ctx.fillStyle = css("--series");
    ctx.fill();
    ctx.strokeStyle = css("--surface-1");
    ctx.lineWidth = 2;
    ctx.stroke();
  }

  canvas._scale = { sx, sy, plot };
}

/// Tick times on boundaries a person recognises -- the hour, midnight, the
/// first of the month -- rather than the span divided by eight.
///
/// Evenly spaced ticks are what produced `10.09.` twice in a row on a week-wide
/// chart: eight steps across seven days is one every 21 hours, so two of them
/// land on the same date and the axis looks broken. Snapping to the calendar
/// also means panning does not slide the labels around.
function timeTicks(from, to, maxCount) {
  const MIN = 60e3;
  const HOUR = 60 * MIN;
  const DAY = 24 * HOUR;
  const ladder = [
    5 * MIN, 15 * MIN, 30 * MIN, HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR,
    DAY, 2 * DAY, 7 * DAY, 14 * DAY,
  ];
  const span = to - from;
  const step = ladder.find((s) => span / s <= maxCount);

  const out = [];
  if (step) {
    // Align to the local clock rather than to the epoch, so a daily tick is
    // local midnight and not 01:00 in summer.
    const offset = new Date(from).getTimezoneOffset() * 60e3;
    let t = Math.ceil((from - offset) / step) * step + offset;
    for (; t <= to; t += step) out.push(t);
    return out;
  }

  // Longer than a fortnight per tick: months, which are not a fixed width.
  const start = new Date(from);
  const months = Math.max(1, Math.round(span / (30 * DAY) / maxCount));
  let d = new Date(start.getFullYear(), start.getMonth(), 1);
  while (d.getTime() <= to) {
    if (d.getTime() >= from) out.push(d.getTime());
    d = new Date(d.getFullYear(), d.getMonth() + months, 1);
  }
  return out;
}

function niceTicks(lo, hi, count) {
  if (!isFinite(lo) || !isFinite(hi) || lo === hi) return [];
  const raw = (hi - lo) / count;
  const magnitude = Math.pow(10, Math.floor(Math.log10(raw)));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * magnitude).find((s) => s >= raw) || magnitude * 10;
  const ticks = [];
  for (let v = Math.ceil(lo / step) * step; v <= hi + step / 1e6; v += step) ticks.push(v);
  return ticks;
}

// --- interaction ------------------------------------------------------------

dom.chart.addEventListener("mousemove", (event) => {
  const s = state.series;
  const scale = dom.chart._scale;
  if (!s || !s.points.length || !scale) return;
  const rect = dom.chart.getBoundingClientRect();
  const x = event.clientX - rect.left;
  const t = s.from + ((x - scale.plot.x) / scale.plot.w) * (s.to - s.from);

  let nearest = s.points[0];
  for (const p of s.points) if (Math.abs(p.t - t) < Math.abs(nearest.t - t)) nearest = p;
  state.hover = nearest;
  drawChart();

  const c = channel();
  const note = state.notes.find((n) => Math.abs(n.at_ms - nearest.t) < (s.to - s.from) / 80);
  dom.tooltip.hidden = false;
  dom.tooltip.innerHTML =
    `<strong>${esc(withUnit(nearest.av, c.unit))}</strong><br />` +
    `min ${esc(num1.format(nearest.lo))} · max ${esc(num1.format(nearest.hi))}<br />` +
    `<span style="color:var(--text-muted)">${esc(new Date(nearest.t).toLocaleString(LOCALE))}</span>` +
    (note ? `<br /><span style="color:var(--text-secondary)">${esc(note.note)}</span>` : "");
  const left = Math.min(scale.sx(nearest.t) + 14, rect.width - dom.tooltip.offsetWidth - 8);
  dom.tooltip.style.left = `${Math.max(0, left)}px`;
  dom.tooltip.style.top = `${Math.max(0, scale.sy(nearest.av) - 56)}px`;
});

dom.chart.addEventListener("mouseleave", () => {
  state.hover = null;
  dom.tooltip.hidden = true;
  drawChart();
});

// --- zoom -------------------------------------------------------------------
// Drag a span, scroll to zoom around the pointer, double-click to go back. The
// same three gestures as the gateway's plots, because a chart you have to learn
// twice is a chart nobody drags.
//
// A zoom pins an absolute window, which also stops the 30 s refresh from
// sliding it: a window that moves while you are measuring something against it
// is worse than no zoom at all.

/// Time under a pointer, clamped to the plot so a drag that leaves the canvas
/// still ends somewhere sensible.
function timeAt(event) {
  const scale = dom.chart._scale;
  const s = state.series;
  if (!scale || !s) return null;
  const rect = dom.chart.getBoundingClientRect();
  const x = Math.min(Math.max(event.clientX - rect.left, scale.plot.x), scale.plot.x + scale.plot.w);
  return { x, t: s.from + ((x - scale.plot.x) / scale.plot.w) * (s.to - s.from) };
}

dom.chart.addEventListener("mousedown", (event) => {
  if (event.button !== 0) return;
  const at = timeAt(event);
  if (!at) return;
  event.preventDefault(); // or the browser starts a text selection instead
  state.drag = { fromX: at.x, toX: at.x, fromT: at.t, toT: at.t };
});

window.addEventListener("mousemove", (event) => {
  if (!state.drag) return;
  const at = timeAt(event);
  if (!at) return;
  state.drag.toX = at.x;
  state.drag.toT = at.t;
  drawChart();
});

window.addEventListener("mouseup", () => {
  const drag = state.drag;
  state.drag = null;
  if (!drag) return;
  drawChart();
  // Below a few pixels this was a click, not a gesture. Zooming on a stray
  // click would make the chart impossible to simply look at.
  if (Math.abs(drag.toX - drag.fromX) < 6) return;
  zoomTo(Math.min(drag.fromT, drag.toT), Math.max(drag.fromT, drag.toT));
});

dom.chart.addEventListener(
  "wheel",
  (event) => {
    const at = timeAt(event);
    const s = state.series;
    if (!at || !s) return;
    event.preventDefault();
    // Around the pointer, not around the centre: zooming towards what you are
    // looking at is the whole reason to use the wheel rather than a drag.
    const factor = event.deltaY < 0 ? 0.8 : 1.25;
    const from = at.t - (at.t - s.from) * factor;
    const to = at.t + (s.to - at.t) * factor;
    if (to - from < MIN_SPAN_MS) return;
    // Zooming out past the widest preset is pointless -- there is no data there
    // and the buckets stop getting coarser.
    const widest = RANGES[RANGES.length - 1].ms;
    zoomTo(Math.max(from, Date.now() - widest), Math.min(to, Date.now()));
  },
  { passive: false },
);

dom.chart.addEventListener("dblclick", () => zoomReset());

el("back").addEventListener("click", () => {
  state.selected = null;
  writeHash();
  show();
});

el("home").addEventListener("click", () => {
  // Back to the overview, and out of any zoom: the title is "show me
  // everything", and an absolute window left over from one channel is not
  // everything.
  state.selected = null;
  state.zoom = null;
  state.zoomHome = null;
  renderRanges();
  writeHash();
  show();
});

dom.toggleTable.addEventListener("click", () => {
  state.showTable = !state.showTable;
  dom.toggleTable.setAttribute("aria-pressed", String(state.showTable));
  dom.tableWrap.hidden = !state.showTable;
});

// What is plotted, as a file. Exactly the rows behind the chart and the table --
// the buckets the server returned, not the raw readings, which for a three-year
// range would be millions. The bucket width is in the filename so a spreadsheet
// three months from now still says what a row covers.
//
// Deliberately not `toLocaleString` anywhere in here. The display formats for
// people; a file formats for whatever opens it next, and that wants ISO 8601 in
// UTC and a full-precision number with a dot in it. A CSV that rounds to one
// decimal and writes 23,7 has thrown away both the precision and the ability to
// be parsed without knowing where it came from.
function csvFilename(c, s) {
  const day = (ms) => new Date(ms).toISOString().slice(0, 10);
  const safe = (x) => String(x).replace(/[^A-Za-z0-9._-]/g, "-");
  return `${safe(c.node)}_${safe(c.sensor)}_${day(s.from)}_${day(s.to)}_${safe(s.bucket)}.csv`;
}

function toCsv(c, s) {
  const unit = c.unit ? ` (${c.unit})` : "";
  const head = ["timestamp", `min${unit}`, `mean${unit}`, `max${unit}`]
    .map((h) => (/[",\n]/.test(h) ? `"${h.replace(/"/g, '""')}"` : h))
    .join(",");
  const rows = s.points.map(
    (p) => `${new Date(p.t).toISOString()},${p.lo},${p.av},${p.hi}`,
  );
  // CRLF, because that is what RFC 4180 says and what Excel is happiest with.
  return [head, ...rows].join("\r\n") + "\r\n";
}

dom.exportCsv.addEventListener("click", () => {
  const s = state.series;
  if (!s || !s.points.length) return;
  const c = channel();
  // A byte-order mark, for one reason only: without it Excel reads the degree
  // sign in the header as two characters of noise. Every other tool ignores it.
  const blob = new Blob(["\ufeff" + toCsv(c, s)], {
    type: "text/csv;charset=utf-8",
  });
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = csvFilename(c, s);
  a.click();
  URL.revokeObjectURL(url);
});

el("add-note").addEventListener("click", () => {
  const now = new Date(Date.now() - new Date().getTimezoneOffset() * 60000);
  el("note-at").value = now.toISOString().slice(0, 16);
  el("note-node").value = state.selected ? state.selected.node : "";
  el("note-text").value = "";
  dom.dialog.showModal();
});

dom.dialog.addEventListener("close", async () => {
  if (dom.dialog.returnValue !== "save") return;
  const text = el("note-text").value.trim();
  if (!text) return;
  const at = el("note-at").value;
  try {
    await getJSON("/api/annotations", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        at_ms: at ? new Date(at).getTime() : undefined,
        node: el("note-node").value.trim(),
        note: text,
      }),
    });
    loadDetail();
  } catch (e) {
    dom.foot.innerHTML = `<span class="error">Note not saved: ${esc(e.message)}</span>`;
  }
});

window.addEventListener("resize", () => {
  if (state.selected) drawChart();
  else renderOverview();
});

window.addEventListener("hashchange", () => {
  readHash();
  renderRanges();
  show();
});

// --- start ------------------------------------------------------------------

function renderRanges() {
  dom.ranges.innerHTML = RANGES.map(
    (r) =>
      `<button data-range="${r.key}" aria-pressed="${!state.zoom && r.key === state.range}">${r.label}</button>`,
  ).join("");
  for (const button of dom.ranges.querySelectorAll("button")) {
    button.addEventListener("click", () => {
      state.range = button.dataset.range;
      // A preset is a deliberate choice of window, so it ends the zoom rather
      // than nesting inside it -- and it becomes the place a later
      // double-click returns to.
      state.zoom = null;
      state.zoomHome = null;
      writeHash();
      renderRanges();
      show();
    });
  }
}

/// Which of the two views is on screen. Kept apart from the two render
/// functions on purpose: the overview's renderer used to decide this, so
/// loading the channel list for a *detail* view -- which it needs, for the
/// units -- flipped the page back to the overview and a deep link never
/// survived its own first paint.
function applyView() {
  const detail = Boolean(state.selected);
  dom.overview.hidden = detail;
  dom.detail.hidden = !detail;
}

function show() {
  applyView();
  // The channel list carries the names and units both views label with, so it
  // is fetched either way.
  loadOverview();
  if (state.selected) loadDetail();
}

readHash();
writeHash();
renderRanges();
loadHealth();
show();
setInterval(() => {
  loadHealth();
  show();
}, REFRESH_MS);
