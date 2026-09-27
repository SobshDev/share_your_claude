// Overview and Requests: one shared filter bar, one analytics report, two views of it.
import { $, api, node, number, run, state } from "./common.js";

const DAY = 86400000;
const tokenFields = ["input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens"];
const tokenLabels = ["Input", "Cache read", "Cache write", "Output"];
const filters = { person: "", model: "", days: 30, from: "", to: "", offset: 0 };
let report = null,
  metric = "observed_total_tokens",
  version = 0,
  filterBar = null,
  actions = {};

export const compact = (value) =>
  value == null
    ? "—"
    : new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value);
const utcDay = (date) => date.toISOString().slice(0, 10);
const modelName = (id) => state.models.find((m) => m.id === id)?.display_name || id;
const shortDate = (day) =>
  new Date(`${day}T00:00:00Z`).toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" });

function select(label, onChange) {
  const wrap = node("span", undefined, "select-wrap"),
    element = node("select", undefined, "select");
  element.setAttribute("aria-label", label);
  element.addEventListener("change", () => onChange(element.value));
  wrap.append(element);
  return [wrap, element];
}
function fill(element, entries, placeholder, current) {
  element.replaceChildren();
  for (const [value, label] of [["", placeholder], ...entries]) {
    const option = node("option", label);
    option.value = value;
    element.append(option);
  }
  element.value = entries.some(([value]) => value === current) ? current : "";
}
let personSelect, modelSelect, rangeButtons, customRange, fromInput, toInput;
function buildFilters() {
  filterBar = node("div", undefined, "filters");
  let personWrap, modelWrap;
  [personWrap, personSelect] = select("Person", (value) => apply({ person: value }));
  [modelWrap, modelSelect] = select("Model", (value) => apply({ model: value }));
  const range = node("div", undefined, "segmented");
  range.setAttribute("role", "radiogroup");
  range.setAttribute("aria-label", "Period");
  rangeButtons = [7, 30, 90, "custom"].map((days) => {
    const choice = node("button", days === "custom" ? "Custom" : `${days}d`);
    choice.type = "button";
    choice.setAttribute("role", "radio");
    choice.addEventListener("click", () => {
      if (days === "custom") {
        filters.days = "custom";
        syncRange();
        fromInput.focus();
      } else apply({ days });
    });
    range.append(choice);
    return [days, choice];
  });
  customRange = node("form", undefined, "custom-range");
  fromInput = node("input");
  toInput = node("input");
  for (const [input, label] of [[fromInput, "From"], [toInput, "Through"]]) {
    input.type = "date";
    input.required = true;
    input.setAttribute("aria-label", label);
  }
  const go = node("button", "Apply", "button secondary");
  go.type = "submit";
  customRange.append(fromInput, toInput, go);
  customRange.addEventListener("submit", (event) => {
    event.preventDefault();
    apply({ days: "custom", from: fromInput.value, to: toInput.value }, go);
  });
  filterBar.append(personWrap, modelWrap, range, customRange);
  syncRange();
  refreshFilterOptions();
}
function syncRange() {
  for (const [days, choice] of rangeButtons)
    choice.setAttribute("aria-checked", String(days === filters.days));
  customRange.hidden = filters.days !== "custom";
  if (filters.days === "custom" && !fromInput.value) {
    const [from, to] = period();
    fromInput.value = utcDay(from);
    toInput.value = utcDay(new Date(to - DAY));
  }
}
function apply(change, control) {
  Object.assign(filters, change, { offset: 0 });
  syncRange();
  return run(loadUsage, control);
}
/** The selected period as [from, to) in UTC, whole days. */
function period() {
  if (filters.days === "custom" && filters.from && filters.to) {
    const from = new Date(`${filters.from}T00:00:00Z`),
      to = new Date(+new Date(`${filters.to}T00:00:00Z`) + DAY);
    return [from, to];
  }
  const days = typeof filters.days === "number" ? filters.days : 30,
    to = new Date(+new Date(`${utcDay(new Date())}T00:00:00Z`) + DAY);
  return [new Date(+to - days * DAY), to];
}

export function refreshFilterOptions() {
  if (!personSelect) return;
  fill(personSelect, state.people.map((p) => [p.id, p.name]), "All people", filters.person);
  const models = new Map(state.models.map((m) => [m.id, m.display_name]));
  for (const entry of report?.model || []) if (!models.has(entry.id)) models.set(entry.id, entry.id);
  fill(modelSelect, [...models], "All models", filters.model);
}
/** Moves the one filter bar into the page being shown. */
export function mountFilters(slot) {
  if (!filterBar) buildFilters();
  slot.append(filterBar);
}
export function initializeAnalytics(handlers) {
  actions = handlers;
  document.querySelectorAll("#chart-metric [data-metric]").forEach((choice) =>
    choice.addEventListener("click", () => {
      metric = choice.dataset.metric;
      document.querySelectorAll("#chart-metric [data-metric]").forEach((other) =>
        other.setAttribute("aria-checked", String(other === choice)));
      if (report) renderChart();
    }));
  $("page-newer").addEventListener("click", (event) =>
    run(() => loadUsage(Math.max(0, filters.offset - 50)), event.currentTarget));
  $("page-older").addEventListener("click", (event) =>
    run(() => loadUsage(filters.offset + 50), event.currentTarget));
}

export async function loadUsage(offset = 0) {
  const current = ++version;
  const [from, to] = period();
  if (!(from < to)) throw new Error("Choose a start date before the end date.");
  const params = new URLSearchParams({ from: from.toISOString(), to: to.toISOString(), offset });
  if (filters.person) params.set("person_id", filters.person);
  if (filters.model) params.set("model", filters.model);
  for (const id of ["page-overview", "page-requests"]) $(id).setAttribute("aria-busy", "true");
  try {
    const result = await api(`analytics?${params}`);
    if (current !== version) return;
    report = result;
    filters.offset = result.offset;
    refreshFilterOptions();
    renderOverview();
    renderRequests();
  } finally {
    if (current === version)
      for (const id of ["page-overview", "page-requests"]) $(id).setAttribute("aria-busy", "false");
  }
}
export function filterByPerson(id) {
  return apply({ person: id });
}

/* Overview */
export function renderOverview() {
  const activeKeys = state.keys.some((key) => !key.revoked_at);
  const welcome = !activeKeys && !(report?.total[0].requests > 0);
  $("welcome").hidden = !welcome;
  $("overview-title").textContent = welcome ? "Welcome" : "Overview";
  document.querySelector("#page-overview [data-filters]").hidden = welcome;
  if (welcome) renderWelcome();
  if (!report) return;
  $("overview-content").hidden = welcome;
  const total = report.total[0];
  const figures = [
    ["fig-tokens", compact(total.observed_total_tokens), total.observed_total_tokens],
    ["fig-requests", number(total.requests), total.requests],
    ["fig-errors", number(total.errors), total.errors],
    ["fig-unmeasured", number(total.incomplete_requests), total.incomplete_requests],
  ];
  for (const [id, text, raw] of figures) {
    $(id).textContent = text;
    $(id).classList.toggle("zero", !raw);
  }
  $("fig-tokens").title = number(total.observed_total_tokens);
  renderChart();
  renderShares("people-share", report.person, (entry) => entry.label, filters.person ? null : filterByPerson);
  renderShares("models-share", report.model, (entry) => modelName(entry.id), filters.model ? null : (id) => apply({ model: id }));
  const values = tokenFields.map((field) => total[field]),
    sum = values.reduce((a, b) => a + (b || 0), 0);
  $("mix-bar").replaceChildren(
    ...values.map((value, index) => {
      const part = node("span", undefined, `c${index}`);
      part.style.flexGrow = String(value || 0);
      return part;
    }).filter(() => sum > 0),
  );
  $("mix-legend").replaceChildren(
    ...tokenLabels.map((label, index) => {
      const item = node("li"),
        swatch = node("span", undefined, `swatch c${index}`),
        value = node("b", compact(values[index]));
      swatch.setAttribute("aria-hidden", "true");
      value.title = number(values[index]);
      item.append(swatch, label, value);
      return item;
    }),
  );
}
function renderWelcome() {
  const connected = state.claude?.state === "connected",
    hasPerson = state.people.length > 0;
  const step = (index, title, done, control) => {
    const row = node("div", undefined, done || control ? "check-step" : "check-step later");
    const mark = node("span", done ? undefined : String(index), done ? "step-done" : "step-number");
    mark.setAttribute("aria-hidden", "true");
    const label = node("p", title);
    row.append(mark, label);
    if (done) {
      label.append(node("span", " (done)", "sr-only"));
      row.append(node("span", "Done", "done-label"));
    } else if (control) row.append(control);
    return row;
  };
  const link = (text, href) => {
    const element = node("a", text, "button");
    element.href = href;
    return element;
  };
  const action = (text, handler) => {
    const element = node("button", text, "button");
    element.type = "button";
    element.addEventListener("click", handler);
    return element;
  };
  $("welcome").replaceChildren(
    step(1, "Connect Claude", connected, connected ? null : link("Connect", "#connection")),
    step(2, "Add a friend", hasPerson, connected && !hasPerson ? action("Add person", () => actions.addPerson()) : null),
    step(3, "Give them a key", false, connected && hasPerson ? action("New key", () => actions.newKey(state.people[0])) : null),
  );
}
function dailySeries() {
  const byDay = new Map(report.day.map((entry) => [entry.id, entry]));
  const from = new Date(report.from), to = new Date(report.to), entries = [];
  if ((to - from) / DAY > 3660) return report.day;
  for (const day = new Date(`${utcDay(from)}T00:00:00Z`); day < to; day.setUTCDate(day.getUTCDate() + 1)) {
    const id = utcDay(day);
    entries.push(byDay.get(id) || { id, requests: 0, errors: 0, observed_total_tokens: 0 });
  }
  return entries;
}
function renderChart() {
  const chart = $("daily-chart"), entries = dailySeries();
  const rows = entries.map((entry) => {
    const row = node("tr"), day = node("th", entry.id);
    day.scope = "row";
    row.append(day, ...["observed_total_tokens", "requests", "errors"].map((field) => node("td", number(entry[field]))));
    return row;
  });
  $("daily-rows").replaceChildren(...rows);
  if (!report.total[0].requests) {
    chart.replaceChildren(node("p", "No requests in this period.", "bars-message"));
    return;
  }
  const max = Math.max(1, ...entries.map((entry) => entry[metric] || 0));
  const axis = node("div", undefined, "bars-axis");
  axis.append(node("span", metric === "observed_total_tokens" ? compact(max) : number(max)), node("span", "0"));
  axis.setAttribute("aria-hidden", "true");
  const plot = node("div", undefined, "bars-plot");
  plot.setAttribute("role", "img");
  plot.setAttribute("aria-label", "Daily chart. Exact values are in the table that follows.");
  plot.style.gap = entries.length > 60 ? "1px" : "3px";
  const today = utcDay(new Date());
  entries.forEach((entry, index) => {
    const bar = node("div", undefined, "bar");
    const value = entry[metric];
    if (value == null) bar.classList.add("empty");
    bar.style.height = `${Math.round(((value || 0) / max) * 150)}px`;
    if (index === entries.length - 1 && entry.id === today) bar.classList.add("last");
    bar.title = `${shortDate(entry.id)}: ${number(value)}`;
    plot.append(bar);
  });
  const dates = node("div", undefined, "bars-dates");
  dates.setAttribute("aria-hidden", "true");
  const first = entries[0]?.id, middle = entries[Math.floor(entries.length / 2)]?.id, last = entries.at(-1)?.id;
  dates.append(node("span", first ? shortDate(first) : ""), node("span", middle && entries.length > 2 ? shortDate(middle) : ""), node("span", last === today ? "Today" : last ? shortDate(last) : ""));
  chart.replaceChildren(axis, plot, dates);
}
function renderShares(id, entries, label, onSelect) {
  const list = $(id);
  const sorted = [...entries].sort((a, b) => (b.observed_total_tokens || 0) - (a.observed_total_tokens || 0) || b.requests - a.requests);
  const total = sorted.reduce((sum, entry) => sum + (entry.observed_total_tokens || 0), 0);
  if (!sorted.length) {
    list.replaceChildren(node("li", "—", "empty-row"));
    return;
  }
  list.replaceChildren(
    ...sorted.map((entry) => {
      const item = node("li"),
        row = node("button", undefined, "share"),
        top = node("span", undefined, "share-top"),
        track = node("span", undefined, "share-track"),
        fill = node("span", undefined, "share-fill");
      row.type = "button";
      row.disabled = !onSelect;
      if (onSelect) row.addEventListener("click", () => onSelect(entry.id));
      top.append(node("span", label(entry)), node("span", compact(entry.observed_total_tokens)));
      fill.style.width = `${total ? ((entry.observed_total_tokens || 0) / total) * 100 : 0}%`;
      track.append(fill);
      track.setAttribute("aria-hidden", "true");
      row.append(top, track);
      item.append(row);
      return item;
    }),
  );
}

/* Requests */
const clock = (date) => date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
function when(iso) {
  const date = new Date(iso), now = new Date();
  if (date.toDateString() === now.toDateString()) return clock(date);
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (date.toDateString() === yesterday.toDateString()) return `Yesterday ${clock(date)}`;
  return `${date.toLocaleDateString(undefined, { month: "short", day: "numeric" })} ${clock(date)}`;
}
function status(entry) {
  switch (entry.outcome) {
    case "in_progress": return ["Running", "running"];
    case "denied": return ["Denied", "fail"];
    case "interrupted": return ["Interrupted", "fail"];
    case "upstream_error": return [entry.http_status ? `Error ${entry.http_status}` : "Error", "fail"];
    default:
      return ["partial", "unknown"].includes(entry.usage_state) ? ["Unmeasured", "partial"] : ["Done", ""];
  }
}
const seconds = (entry) =>
  entry.finished_at
    ? `${new Intl.NumberFormat(undefined, { minimumFractionDigits: 1, maximumFractionDigits: 1 }).format(Math.max(0, new Date(entry.finished_at) - new Date(entry.started_at)) / 1000)}s`
    : "—";
function renderRequests() {
  const rows = [];
  for (const entry of report.requests) {
    const row = node("tr", undefined, "request-row"),
      toggle = node("button", when(entry.started_at), "toggle");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", "false");
    toggle.title = new Date(entry.started_at).toLocaleString();
    const whenCell = node("td", undefined, "when");
    whenCell.append(toggle);
    const who = node("td");
    who.append(node("span", entry.person_name, "who"), node("span", entry.key_label, "sub"));
    const model = node("td", modelName(entry.resolved_model || entry.requested_model));
    if (entry.response_model && entry.response_model !== entry.resolved_model) model.title = `Served by ${entry.response_model}`;
    const [text, kind] = status(entry), outcome = node("td");
    outcome.append(node("span", text, `state ${kind}`));
    row.append(whenCell, who, model, outcome, node("td", seconds(entry), "time"), node("td", number(entry.observed_total_tokens), "num"));
    const detail = node("tr", undefined, "detail-row");
    detail.hidden = true;
    const spacer = node("td");
    spacer.colSpan = 2;
    const cell = node("td");
    cell.colSpan = 4;
    const parts = node("dl", undefined, "token-parts");
    tokenFields.forEach((field, index) => {
      const part = node("div");
      part.append(node("dt", tokenLabels[index]), node("dd", number(entry[field])));
      parts.append(part);
    });
    cell.append(parts);
    detail.append(spacer, cell);
    const flip = () => {
      const open = detail.hidden;
      detail.hidden = !open;
      row.classList.toggle("open", open);
      toggle.setAttribute("aria-expanded", String(open));
    };
    row.addEventListener("click", (event) => {
      if (event.target !== toggle) flip();
    });
    toggle.addEventListener("click", flip);
    rows.push(row, detail);
  }
  $("request-rows").replaceChildren(...rows);
  const count = report.requests.length, total = report.total[0].requests;
  $("requests-empty").hidden = count > 0;
  document.querySelector(".pager").hidden = count === 0;
  $("pager-caption").textContent = `${number(report.offset + 1)}–${number(report.offset + count)} of ${number(total)}`;
  $("page-newer").disabled = report.offset === 0;
  $("page-older").disabled = !report.has_more;
}
