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
  { key: "7d", label: "7 T", ms: 7 * 24 * 3600e3 },
  { key: "30d", label: "30 T", ms: 30 * 24 * 3600e3 },
  { key: "1y", label: "1 J", ms: 365 * 24 * 3600e3 },
  { key: "3y", label: "3 J", ms: 3 * 365 * 24 * 3600e3 },
];

const REFRESH_MS = 30000;
const num = new Intl.NumberFormat("de-DE", { maximumFractionDigits: 2 });
const num1 = new Intl.NumberFormat("de-DE", { maximumFractionDigits: 1 });

const state = {
  range: "24h",
  channels: [],
  selected: null, // { node, sensor }
  series: null,
  notes: [],
  hover: null,
  showTable: false,
};

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
  const to = Date.now();
  return { from: to - range().ms, to };
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
  if (!c.points || !c.points.length) return "keine Daten";
  let lo = Infinity;
  let hi = -Infinity;
  for (const [, v] of c.points) {
    lo = Math.min(lo, v);
    hi = Math.max(hi, v);
  }
  return `${num1.format(lo)} – ${num1.format(hi)}`;
}

function ago(ms) {
  if (!ms) return "nie";
  const mins = Math.round((Date.now() - ms) / 60000);
  if (mins < 1) return "gerade eben";
  if (mins < 60) return `vor ${mins} Min.`;
  const hours = Math.round(mins / 60);
  if (hours < 48) return `vor ${hours} Std.`;
  return `vor ${Math.round(hours / 24)} Tagen`;
}

function formatTime(ms, spanMs) {
  const d = new Date(ms);
  if (spanMs <= 2 * 24 * 3600e3)
    return d.toLocaleTimeString("de-DE", { hour: "2-digit", minute: "2-digit" });
  if (spanMs <= 120 * 24 * 3600e3)
    return d.toLocaleDateString("de-DE", { day: "2-digit", month: "2-digit" });
  return d.toLocaleDateString("de-DE", { month: "2-digit", year: "2-digit" });
}

// --- routing ----------------------------------------------------------------

function readHash() {
  const parts = decodeURIComponent(location.hash.slice(1)).split("/");
  if (parts[0] === "c" && parts[1] && parts[2]) {
    state.selected = { node: parts[1], sensor: parts[2] };
  } else {
    state.selected = null;
  }
  if (RANGES.some((r) => r.key === parts[3])) state.range = parts[3];
  else if (RANGES.some((r) => r.key === parts[1]) && parts[0] !== "c") state.range = parts[1];
}

function writeHash() {
  const next = state.selected
    ? `#c/${encodeURIComponent(state.selected.node)}/${encodeURIComponent(state.selected.sensor)}/${state.range}`
    : `#all/${state.range}`;
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
  dom.foot.textContent = "Lade …";
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
      `<span><span class="dot ${ok ? "up" : "down"}"></span>${name} ${ok ? "verbunden" : "getrennt"}</span>`;
    dom.health.innerHTML =
      pill(h.broker_connected, "Broker") +
      pill(h.database_reachable, "QuestDB") +
      `<span><b>${num.format(h.rows_written)}</b> Zeilen geschrieben</span>`;
    el("brand-sub").textContent = h.retention
      ? `Rohdaten ${h.retention.toLowerCase()}, ${h.views.length} Rollups`
      : "ohne Verfall";
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

  if (!s || !s.points.length) {
    dom.stats.innerHTML = "";
    dom.foot.textContent = s ? "Keine Daten in diesem Zeitraum." : "";
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
    stat("Mittel", withUnit(sum / s.points.length, c.unit)) +
    stat("max", withUnit(hi, c.unit)) +
    stat("zuletzt", withUnit(s.points[s.points.length - 1].av, c.unit)) +
    stat("Punkte", `${s.points.length} à ${s.bucket}`);

  dom.foot.textContent = `aus ${s.source}, Zeitraum ${new Date(s.from).toLocaleString("de-DE")} – ${new Date(s.to).toLocaleString("de-DE")}`;

  dom.table.innerHTML = s.points
    .map(
      (p) =>
        `<tr><td>${esc(new Date(p.t).toLocaleString("de-DE"))}</td><td>${esc(num1.format(p.lo))}</td>` +
        `<td>${esc(num1.format(p.av))}</td><td>${esc(num1.format(p.hi))}</td></tr>`,
    )
    .join("");

  drawChart();
  renderNotes();
}

function renderNotes() {
  if (!state.notes.length) {
    dom.noteList.innerHTML = `<li class="none">Keine — was in diesem Zeitraum passiert ist, steht nirgends.</li>`;
    return;
  }
  dom.noteList.innerHTML = state.notes
    .map(
      (n, i) =>
        `<li><time>${esc(new Date(n.at_ms).toLocaleString("de-DE"))}</time>` +
        `<span>${esc(n.note)}${n.node && n.node !== "fleet" ? "" : " <em>(Flotte)</em>"}</span>` +
        `<button class="void" type="button" data-note="${i}" ` +
        `title="Diese Notiz zurücknehmen">zurücknehmen</button></li>`,
    )
    .join("");
}

// Zurücknehmen statt Löschen: der Zeitstempel einer Notiz ist die designierte
// Spalte der Tabelle und lässt sich nicht ändern, Zeilen löschen kann QuestDB
// gar nicht. Eine auf die falsche Minute gesetzte Notiz kann also nur als
// ungültig markiert und daneben neu geschrieben werden. Sie bleibt in der
// Datenbank stehen -- bei einem Protokoll darüber, was passiert ist, ist eine
// sichtbare Korrektur mehr wert als eine spurlose.
dom.noteList.addEventListener("click", async (ev) => {
  const button = ev.target.closest("button.void");
  if (!button) return;
  const note = state.notes[Number(button.dataset.note)];
  if (!note) return;
  if (!window.confirm(`Diese Notiz zurücknehmen?\n\n${note.note}`)) return;
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
    dom.foot.innerHTML = `<span class="error">Notiz nicht zurückgenommen: ${esc(e.message)}</span>`;
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
    `<span style="color:var(--text-muted)">${esc(new Date(nearest.t).toLocaleString("de-DE"))}</span>` +
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

el("back").addEventListener("click", () => {
  state.selected = null;
  writeHash();
  show();
});

dom.toggleTable.addEventListener("click", () => {
  state.showTable = !state.showTable;
  dom.toggleTable.setAttribute("aria-pressed", String(state.showTable));
  dom.tableWrap.hidden = !state.showTable;
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
    dom.foot.innerHTML = `<span class="error">Notiz nicht gespeichert: ${esc(e.message)}</span>`;
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
    (r) => `<button data-range="${r.key}" aria-pressed="${r.key === state.range}">${r.label}</button>`,
  ).join("");
  for (const button of dom.ranges.querySelectorAll("button")) {
    button.addEventListener("click", () => {
      state.range = button.dataset.range;
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
