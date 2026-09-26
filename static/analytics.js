"use strict";
let analyticsReport = null;
let analyticsParams = null;
let analyticsVersion = 0;
const tokenFields = ["input_tokens", "cache_read_tokens", "cache_write_tokens", "output_tokens"];
const tokenLabels = ["Input", "Cache read", "Cache write", "Output"];
const compactNumber = (value) => new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value);
const duration = (ms) => ms == null ? "—" : `${new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(ms / 1000)} s`;

function populateFilter(id, entries, placeholder) {
  const select = $(id), previous = select.value;
  select.replaceChildren();
  for (const entry of [{ id: "", label: placeholder }, ...entries]) {
    const option = node("option", entry.label);
    option.value = entry.id;
    select.append(option);
  }
  if ([...select.options].some((option) => option.value === previous)) select.value = previous;
}
function updateUsageFilters() {
  populateFilter("usage-person", people.map((p) => ({ id: p.id, label: p.name })), "All users");
  // Retain historical/unresolved models discovered in reports, even if absent from the catalog.
  const known = new Map([...$("usage-model").options].filter((o) => o.value).map((o) => [o.value, o.textContent]));
  for (const model of models) known.set(model.id, model.display_name);
  populateFilter("usage-model", [...known].map(([id, label]) => ({ id, label })), "All models");
}
async function selectUsagePerson(id) {
  $("usage-person").value = id;
  location.hash = "overview";
  await loadUsage();
  $("scope-title").scrollIntoView({ block: "start" });
}
function initializeAnalytics() {
  let chartWidth = 0;
  new ResizeObserver(([entry]) => {
    const width = Math.round(entry.contentRect.width);
    if (width > 0 && width !== chartWidth && analyticsReport) {
      chartWidth = width;
      renderTimeChart($("chart-metric").value);
    }
  }).observe($("time-chart"));
  $("group-by").addEventListener("change", () => analyticsReport && renderLedger());
  $("chart-metric").addEventListener("change", () => analyticsReport && renderCharts());
  $("history-prev").addEventListener("click", () => run(() => loadUsage(Math.max(0, analyticsReport.offset - 50))));
  $("history-next").addEventListener("click", () => run(() => loadUsage(analyticsReport.offset + 50)));
  document.querySelectorAll("[data-days]").forEach((control) => control.addEventListener("click", () => run(async () => {
    const end = new Date(), start = new Date(end);
    start.setUTCDate(start.getUTCDate() - Number(control.dataset.days) + 1);
    $("date-from").value = start.toISOString().slice(0, 10);
    $("date-to").value = end.toISOString().slice(0, 10);
    await loadUsage();
  }, control)));
}
async function loadUsage(offset = null) {
  const version = ++analyticsVersion;
  $("analytics-status").classList.remove("sr-only");
  $("analytics-status").textContent = "Loading usage…";
  $("analytics-content").hidden = true;
  $("page-overview").setAttribute("aria-busy", "true");
  $("history-prev").disabled = true;
  $("history-next").disabled = true;
  try {
    let params;
    if (offset !== null && analyticsParams) params = new URLSearchParams(analyticsParams);
    else {
      const from = new Date(`${$("date-from").value}T00:00:00Z`), end = new Date(`${$("date-to").value}T00:00:00Z`);
      end.setUTCDate(end.getUTCDate() + 1);
      if (!Number.isFinite(+from) || !Number.isFinite(+end) || from >= end) throw new Error("Choose a valid date range.");
      params = new URLSearchParams({ from: from.toISOString(), to: end.toISOString() });
      if ($("usage-person").value) params.set("person_id", $("usage-person").value);
      if ($("usage-model").value) params.set("model", $("usage-model").value);
    }
    params.set("offset", offset ?? 0);
    const report = await api(`analytics?${params}`);
    if (version !== analyticsVersion) return;
    analyticsParams = params;
    analyticsReport = report;
    for (const model of report.model) {
      if (![...$("usage-model").options].some((o) => o.value === model.id)) {
        const option = node("option", model.label);
        option.value = model.id;
        $("usage-model").append(option);
      }
    }
    $("analytics-content").hidden = false;
    renderAnalytics();
    // Keep the live region in the accessibility tree; announce the result once.
    $("analytics-status").textContent = offset === null ? `Usage loaded: ${number(report.total[0].requests)} requests in this selection.` : `Request history: ${$("history-caption").textContent}.`;
    $("analytics-status").classList.add("sr-only");
  } catch (error) {
    if (version === analyticsVersion) $("analytics-status").textContent = `Could not load usage. ${error.message} Apply filters or refresh to retry.`;
    throw error;
  } finally {
    if (version === analyticsVersion) $("page-overview").setAttribute("aria-busy", "false");
  }
}
function renderAnalytics() {
  const report = analyticsReport, total = report.total[0];
  const personId = analyticsParams.get("person_id");
  $("scope-title").textContent = personId ? people.find((p) => p.id === personId)?.name || "Selected user" : "All users";
  const through = new Date(new Date(report.to).getTime() - 1).toISOString().slice(0, 10);
  $("scope-period").textContent = `${report.from.slice(0, 10)} – ${through} · UTC${analyticsParams.get("model") ? ` · ${analyticsParams.get("model")}` : " · All models"}`;
  const summary = $("usage-summary");
  summary.replaceChildren();
  const values = [
    ["Observed tokens", number(total.observed_total_tokens)], ["Requests", number(total.requests)],
    ["Users with requests", number(total.active_users)], ["Models routed", number(total.models_used)],
    ["Completed", total.requests ? `${(total.completed_requests / total.requests * 100).toFixed(1)}%` : "—"],
    ["Avg. completed duration", duration(total.average_duration_ms)],
  ];
  for (const [label, value] of values) {
    const item = node("div"); item.append(node("dt", label), node("dd", value)); summary.append(item);
  }
  $("measurement-note").textContent = `${number(total.complete_requests)} complete measurements · ${number(total.partial_requests)} partial · ${number(total.unknown_requests)} unknown. ${number(total.denied_requests)} denied · ${number(total.errors)} errors or interruptions · ${number(total.in_progress_requests)} in progress. Duration covers the full request, including streaming, across ${number(total.duration_samples)} completed requests.`;
  renderCharts();
  renderLedger();
  renderHistory();
}
function svgNode(tag, attrs = {}, text) {
  const element = document.createElementNS("http://www.w3.org/2000/svg", tag);
  for (const [name, value] of Object.entries(attrs)) element.setAttribute(name, value);
  if (text !== undefined) element.textContent = text;
  return element;
}
function shareChart(id, entries, metric, action) {
  const container = $(id); container.replaceChildren();
  const sorted = [...entries].sort((a, b) => (b[metric] || 0) - (a[metric] || 0) || a.label.localeCompare(b.label));
  const total = sorted.reduce((sum, entry) => sum + (entry[metric] || 0), 0);
  if (!sorted.length) { container.append(node("p", "No requests in this period.", "chart-empty")); return; }
  const positive = sorted.filter((entry) => entry[metric] > 0);
  const slices = positive.length > 6 ? [...positive.slice(0, 5), { label: "Other", [metric]: positive.slice(5).reduce((sum, entry) => sum + entry[metric], 0) }] : positive;
  const colorFor = (entry) => {
    const index = slices.findIndex((slice) => slice === entry);
    return index < 0 ? 5 : index;
  };
  if (total > 0) {
    const svg = svgNode("svg", { viewBox: "0 0 160 160", role: "img", "aria-label": `Share of ${metric === "requests" ? "requests" : metric === "errors" ? "errors" : id === "outcome-chart" ? "requests" : "observed tokens"}; exact values in the adjacent list.` });
    let start = -Math.PI / 2;
    for (const [index, entry] of slices.entries()) {
      const angle = entry[metric] / total * Math.PI * 2, end = start + angle;
      const shape = angle >= Math.PI * 2 - 0.000001
        ? svgNode("circle", { cx: 80, cy: 80, r: 72, class: `chart-color-${index}` })
        : svgNode("path", { d: `M80,80 L${80 + 72 * Math.cos(start)},${80 + 72 * Math.sin(start)} A72,72 0 ${angle > Math.PI ? 1 : 0},1 ${80 + 72 * Math.cos(end)},${80 + 72 * Math.sin(end)} Z`, class: `chart-color-${index}` });
      shape.append(svgNode("title", {}, `${entry.label}: ${number(entry[metric])} (${(entry[metric] / total * 100).toFixed(1)}%)`));
      svg.append(shape); start = end;
    }
    container.append(svg);
  } else container.append(node("p", sorted.some((e) => e[metric] === null) ? "No observed values available." : "No activity for this metric.", "chart-empty"));
  const list = node("ul", undefined, "chart-legend");
  for (const entry of sorted) {
    const item = node("li"), name = action ? button(entry.label, () => action(entry.id), "text-button") : node("span", entry.label);
    const swatch = node("span", undefined, `chart-swatch chart-color-${colorFor(entry)}`); swatch.setAttribute("aria-hidden", "true");
    const label = node("div", undefined, "legend-label"); label.append(swatch, name);
    const value = node("span", `${number(entry[metric])}${total > 0 && entry[metric] != null ? ` · ${(entry[metric] / total * 100).toFixed(1)}%` : ""}`, "legend-value");
    item.append(label, value); list.append(item);
  }
  container.append(list);
  if (positive.length > 6) {
    const note = node("li", "Other combines the remaining entries; every value is listed above.", "helper");
    list.append(note);
  }
}
function renderCharts() {
  const report = analyticsReport, metric = $("chart-metric").value, total = report.total[0];
  shareChart("model-chart", report.model, metric, async (id) => { $("usage-model").value = id; await loadUsage(); });
  shareChart("person-chart", report.person, metric, selectUsagePerson);
  $("person-chart-note").textContent = `Share of ${$("chart-metric").selectedOptions[0].textContent.toLowerCase()} in this selection. Select a user to explore.`;
  shareChart("token-chart", tokenFields.map((field, index) => ({ label: tokenLabels[index], value: total[field] })), "value");
  shareChart("outcome-chart", [["Completed", "completed_requests"], ["Denied", "denied_requests"], ["Errors / interrupted", "errors"], ["In progress", "in_progress_requests"]].map(([label, field]) => ({ label, value: total[field] })), "value");
  renderTimeChart(metric);
}
function dailySeries() {
  const byDay = new Map(analyticsReport.day.map((entry) => [entry.id, entry]));
  const start = new Date(analyticsReport.from), end = new Date(analyticsReport.to);
  // Keep a very long custom range bounded; missing dates are still visible as gaps.
  const days = Math.ceil((end - start) / 86400000);
  if (days > 3660) return analyticsReport.day;
  const entries = [];
  for (const day = new Date(start); day < end; day.setUTCDate(day.getUTCDate() + 1)) {
    const id = day.toISOString().slice(0, 10);
    entries.push(byDay.get(id) || { id, label: id, requests: 0, errors: 0, incomplete_requests: 0, observed_total_tokens: 0 });
  }
  return entries;
}
function renderTimeChart(metric) {
  const container = $("time-chart"), rows = $("daily-rows"), entries = dailySeries();
  container.replaceChildren(); rows.replaceChildren();
  for (const entry of entries) {
    const row = node("tr");
    const day = node("th", entry.label); day.scope = "row"; row.append(day);
    for (const field of ["observed_total_tokens", "requests", "errors", "incomplete_requests"]) row.append(node("td", number(entry[field])));
    rows.append(row);
  }
  if (!analyticsReport.total[0].requests) { container.append(node("p", "No requests in this period. Change the filters to explore another period.", "chart-empty")); return; }
  const width = Math.max(300, container.clientWidth), height = 240, left = 66, right = 24, top = 16, bottom = 36;
  const max = entries.reduce((max, entry) => Math.max(max, entry[metric] || 0), 1);
  const startTime = new Date(analyticsReport.from).getTime(), endTime = new Date(analyticsReport.to).getTime() - 86400000;
  const x = (entry) => endTime <= startTime ? width / 2 : left + (new Date(`${entry.id}T00:00:00Z`).getTime() - startTime) / (endTime - startTime) * (width - left - right);
  const y = (value) => top + (1 - value / max) * (height - top - bottom);
  const svg = svgNode("svg", { viewBox: `0 0 ${width} ${height}`, role: "img", "aria-label": `Daily ${$("chart-metric").selectedOptions[0].textContent.toLowerCase()}. Missing measurements are gaps. Expand View daily values for exact counts.` });
  for (let tick = 0; tick <= 4; tick++) {
    const value = max * tick / 4;
    svg.append(svgNode("line", { x1: left, x2: width - right, y1: y(value), y2: y(value), class: "chart-gridline" }), svgNode("text", { x: left - 10, y: y(value) + 4, "text-anchor": "end", class: "chart-axis" }, compactNumber(value)));
  }
  let segment = [];
  const flush = () => { if (segment.length) svg.append(svgNode("polyline", { points: segment.join(" "), class: "chart-line" })); segment = []; };
  let previousDay = null;
  for (const entry of entries) {
    const currentDay = new Date(`${entry.id}T00:00:00Z`).getTime();
    if (previousDay !== null && currentDay - previousDay > 86400000) flush();
    if (entry[metric] == null) flush();
    else segment.push(`${x(entry)},${y(entry[metric])}`);
    previousDay = currentDay;
  }
  flush();
  // Points with native tooltips stay useful without an external chart library.
  for (const entry of entries.filter((e) => e[metric] != null && (e.requests > 0 || entries.length <= 90))) {
    const point = svgNode("circle", { cx: x(entry), cy: y(entry[metric]), r: 3.5, class: entry.incomplete_requests ? "chart-point incomplete" : "chart-point" });
    point.append(svgNode("title", {}, `${entry.id}: ${number(entry[metric])} · ${entry.incomplete_requests} incomplete measurements`)); svg.append(point);
  }
  const first = analyticsReport.from.slice(0, 10), last = new Date(new Date(analyticsReport.to).getTime() - 1).toISOString().slice(0, 10);
  svg.append(svgNode("text", { x: left, y: height - 6, class: "chart-axis" }, first), svgNode("text", { x: width - right, y: height - 6, "text-anchor": "end", class: "chart-axis" }, last));
  container.append(svg, node("p", "Missing token measurements appear as gaps; amber points include incomplete usage. Days without requests are zero. Model and user shares follow the chart metric.", "helper"));
}
function renderLedger() {
  const group = $("group-by").value, entries = analyticsReport[group], total = analyticsReport.total[0];
  $("group-heading").textContent = $("group-by").selectedOptions[0].textContent;
  $("ledger-caption").textContent = `Token ledger by ${$("group-by").selectedOptions[0].textContent.toLowerCase()}`;
  const rows = $("usage-rows"); rows.replaceChildren();
  for (const entry of [...entries, ...(entries.length ? [{ ...total, label: "Total for selection", isTotal: true }] : [])]) {
    const row = node("tr", undefined, entry.isTotal ? "total-row" : "");
    const label = node("th"); label.scope = "row";
    if (group === "person" && !entry.isTotal) label.append(button(entry.label, () => selectUsagePerson(entry.id), "text-button"));
    else label.textContent = entry.label;
    row.append(label);
    for (const field of [...tokenFields, "observed_total_tokens", "requests"]) row.append(node("td", number(entry[field])));
    const status = node("td");
    status.append(node("span", entry.incomplete_requests ? `${number(entry.incomplete_requests)} incomplete` : entry.denied_requests === entry.requests ? "Denied" : entry.errors ? "Errors reported" : "Recorded", entry.incomplete_requests || entry.errors ? "status warning" : "status"));
    status.append(node("span", `${number(entry.denied_requests)} denied · ${number(entry.errors)} errors`, "status-detail"));
    row.append(status); rows.append(row);
  }
  $("usage-empty").hidden = entries.length > 0;
  $("report-caption").textContent = `${number(total.requests)} requests · ${number(total.incomplete_requests)} incomplete measurements`;
}
function renderHistory() {
  const report = analyticsReport, rows = $("request-rows"); rows.replaceChildren();
  for (const entry of report.requests) {
    const row = node("tr"), model = node("td", entry.resolved_model || entry.requested_model);
    model.title = `Requested: ${entry.requested_model}\nResolved: ${entry.resolved_model || "Unknown"}\nResponse: ${entry.response_model || "Unknown"}`;
    if (entry.response_model && entry.response_model !== entry.resolved_model) model.append(node("span", `Response: ${entry.response_model}`, "status-detail"));
    const person = node("td", entry.person_name); person.append(node("span", entry.key_label, "status-detail"));
    const outcome = entry.outcome.replaceAll("_", " ");
    const started = node("th", entry.started_at.replace("T", " ").replace("Z", "")); started.scope = "row";
    row.append(started, person, model, node("td", `${outcome} · ${entry.http_status ?? "—"}`), node("td", duration(entry.finished_at ? Math.max(0, new Date(entry.finished_at) - new Date(entry.started_at)) : null)));
    for (const field of [...tokenFields, "observed_total_tokens"]) row.append(node("td", number(entry[field])));
    const measurement = node("td"); measurement.append(node("span", entry.usage_state.replaceAll("_", " "), ["partial", "unknown"].includes(entry.usage_state) ? "status warning" : "status"));
    row.append(measurement); rows.append(row);
  }
  $("history-prev").disabled = report.offset === 0;
  $("history-next").disabled = !report.has_more;
  $("history-caption").textContent = report.requests.length ? `${number(report.offset + 1)}–${number(report.offset + report.requests.length)} of ${number(report.total[0].requests)} requests` : "No requests in this selection";
}
