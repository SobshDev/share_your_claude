import { $, api, node, notice, onReauth, run, setCsrf, state } from "./common.js";
import {
  compact,
  initializeAnalytics,
  loadUsage,
  mountFilters,
  refreshFilterOptions,
  renderOverview,
} from "./analytics.js";

const pages = {
  overview: "Overview",
  requests: "Requests",
  people: "People & keys",
  models: "Models",
  connection: "Claude connection",
};
const DOTS =
  '<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="3.5" cy="8" r="1.3"/><circle cx="8" cy="8" r="1.3"/><circle cx="12.5" cy="8" r="1.3"/></svg>';
const PLUS =
  '<svg viewBox="0 0 14 14" aria-hidden="true"><path d="M7 2.5v9M2.5 7h9"/></svg>';
const shortName = (name) => name.replace(/^Claude /, "");
const enabledModels = () => state.models.filter((m) => m.enabled && m.reviewed_at);
state.claude = null;
state.personTokens = new Map();

function ago(iso) {
  if (!iso) return "";
  const minutes = Math.round((Date.now() - new Date(iso)) / 60000);
  if (minutes < 1) return "Just now";
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} hr ago`;
  if (hours < 48) return "Yesterday";
  return new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
const day = (iso) => new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric" });
function iconButton(label, svg) {
  const element = node("button", undefined, "icon-button");
  element.type = "button";
  element.setAttribute("aria-label", label);
  element.insertAdjacentHTML("beforeend", svg);
  return element;
}

/* Pages */
let currentPage = null;
function showPage(focus = false) {
  const hash = location.hash.slice(1);
  if (!Object.hasOwn(pages, hash) && currentPage) return;
  const name = Object.hasOwn(pages, hash) ? hash : "overview";
  currentPage = name;
  for (const key of Object.keys(pages)) $(`page-${key}`).hidden = key !== name;
  document.querySelectorAll(".rail nav a").forEach((link) => {
    if (link.dataset.page === name) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  });
  document.title = `${pages[name]} · Shared Router`;
  const slot = $(`page-${name}`).querySelector("[data-filters]");
  if (slot) mountFilters(slot);
  if (name === "connection") prepareSignIn();
  if (focus) $(`page-${name}`).querySelector("h1").focus();
}

/* Claude connection */
let flowOpen = false,
  authorizeAt = 0;
async function connection() {
  const data = await api("me");
  setCsrf(data.csrf_token);
  state.claude = data.claude;
  renderConnection();
}
function renderConnection() {
  const status = state.claude?.state,
    connected = status === "connected",
    expired = status === "needs_reauth";
  const rail = $("rail-status");
  rail.classList.toggle("connected", connected);
  rail.classList.toggle("problem", !connected);
  $("rail-status-label").textContent = connected ? "Claude connected" : expired ? "Reconnect Claude" : "Connect Claude";
  $("connection-state").textContent = connected ? "Connected" : expired ? "Sign-in expired" : "Not connected";
  $("connection-dot").className = `status-dot ${connected ? "connected" : "problem"}`;
  const showFlow = !connected || flowOpen;
  $("oauth-form").hidden = !showFlow;
  $("reconnect").hidden = !connected || flowOpen;
  $("connection-details").hidden = !connected || flowOpen;
  $("refresh-models").disabled = !connected;
  $("refresh-models").dataset.unavailable = String(!connected);
  if (connected) {
    const left = state.claude.expires_at - Date.now() / 1000;
    const hours = Math.floor(left / 3600), minutes = Math.floor((left % 3600) / 60);
    $("detail-token").textContent = left > 60 ? `Renews in ${hours ? `${hours} h ` : ""}${minutes} min` : "Renews on the next request";
    const enabled = enabledModels().length;
    $("detail-models").textContent = `${enabled} of ${state.models.length} enabled`;
    const last = state.keys.map((k) => k.last_used_at).filter(Boolean).sort().at(-1);
    $("detail-last").textContent = last ? ago(last) : "None yet";
  }
  if (showFlow && currentPage === "connection") prepareSignIn();
  renderOverview();
}
/** Starts a sign-in when the steps are visible, so the first step is a plain link. */
function prepareSignIn() {
  if ($("oauth-form").hidden || Date.now() - authorizeAt < 9 * 60000) return;
  authorizeAt = Date.now();
  const link = $("authorize-link");
  link.setAttribute("aria-disabled", "true");
  link.removeAttribute("href");
  run(async () => {
    try {
      const result = await api("claude/login", "POST");
      link.href = result.authorize_url;
      link.removeAttribute("aria-disabled");
    } catch (error) {
      authorizeAt = 0;
      throw error;
    }
  });
}

/* Access data */
async function loadAccess() {
  const [people, keys, models, usage] = await Promise.all([
    api("people"),
    api("keys"),
    api("models"),
    api("usage?group_by=person"),
  ]);
  Object.assign(state, { people, keys, models });
  state.personTokens = new Map(usage.data.map((row) => [row.id, row.observed_total_tokens]));
  renderPeople();
  renderModels();
  refreshFilterOptions();
  renderConnection();
}

/* Menus */
let menuOwner = null;
function closeMenu(restore = false) {
  if (!menuOwner) return;
  $("menu").hidden = true;
  menuOwner.setAttribute("aria-expanded", "false");
  if (restore) menuOwner.focus();
  menuOwner = null;
}
function openMenu(owner, items) {
  if (menuOwner === owner) return closeMenu(true);
  closeMenu();
  const menu = $("menu");
  menu.replaceChildren(
    ...items.map((item) => {
      if (item === "-") return node("hr");
      const choice = node("button", item.label, item.danger ? "danger" : undefined);
      choice.type = "button";
      choice.setAttribute("role", "menuitem");
      choice.addEventListener("click", () => {
        closeMenu();
        item.action();
      });
      return choice;
    }),
  );
  menuOwner = owner;
  owner.setAttribute("aria-expanded", "true");
  menu.hidden = false;
  const box = owner.getBoundingClientRect();
  menu.style.top = `${box.bottom + scrollY + 6}px`;
  menu.style.left = `${Math.max(8, box.right + scrollX - menu.offsetWidth)}px`;
  menu.querySelector("button").focus();
}
function menuButton(label, items) {
  const control = iconButton(label, DOTS);
  control.setAttribute("aria-haspopup", "menu");
  control.setAttribute("aria-expanded", "false");
  control.addEventListener("click", () => openMenu(control, items()));
  return control;
}

/* People & keys */
function renderPeople() {
  const list = $("people-list");
  if (!state.people.length) {
    const empty = node("div", undefined, "people-empty"),
      add = node("button", "Add person", "button");
    add.type = "button";
    add.addEventListener("click", () => openPersonDialog());
    empty.append(node("span", "No one has access yet"), add);
    list.replaceChildren(empty);
    return;
  }
  list.replaceChildren(
    ...state.people.map((person) => {
      const section = node("section", undefined, "person"),
        head = node("div", undefined, "person-head"),
        title = node("h2"),
        name = node("button", person.name, "person-name");
      name.type = "button";
      name.title = "Rename";
      name.addEventListener("click", () => openPersonDialog(person));
      title.append(name);
      const tokens = state.personTokens.get(person.id);
      head.append(title, node("span", tokens ? `${compact(tokens)} tokens · 30 days` : "", "person-usage"));
      section.append(head);
      const keys = state.keys
        .filter((key) => key.person_id === person.id)
        .sort((a, b) => Boolean(a.revoked_at) - Boolean(b.revoked_at));
      for (const key of keys) section.append(keyRow(person, key));
      const add = node("button", undefined, "text-action");
      add.type = "button";
      add.insertAdjacentHTML("beforeend", PLUS);
      add.append("New key");
      add.addEventListener("click", () => openKeyDialog(person));
      const footer = node("div", undefined, "new-key");
      footer.append(add);
      section.append(footer);
      return section;
    }),
  );
}
function keyRow(person, key) {
  const row = node("div", key.revoked_at ? "" : undefined, key.revoked_at ? "key revoked" : "key");
  const id = node("div", undefined, "key-id");
  id.append(node("strong", key.label), node("code", `${key.prefix}…`));
  const chips = node("div", undefined, "chips");
  const granted = state.models.filter((m) => m.enabled && m.reviewed_at && key.models.includes(m.id));
  if (granted.length) for (const model of granted) chips.append(node("span", shortName(model.display_name), "chip"));
  else chips.append(node("span", "No models", "chip none"));
  const used = node("div", key.revoked_at ? `Revoked ${day(key.revoked_at)}` : ago(key.last_used_at) || "Never used", "key-used");
  const menu = node("div", undefined, "key-menu");
  if (!key.revoked_at)
    menu.append(
      menuButton(`Actions for ${key.label}`, () => [
        { label: "Model access", action: () => openKeyDialog(person, key) },
        { label: "Setup for opencodex", action: () => run(() => openSetup(person, key)) },
        "-",
        { label: "Revoke key", danger: true, action: () => confirmRevoke(key) },
      ]),
    );
  row.append(id, chips, used, menu);
  return row;
}

/* Sheets */
function openDialog(id, focus) {
  $(id).showModal();
  (focus ? $(focus) : $(id).querySelector("input, button"))?.focus();
}
let personTarget = null;
function openPersonDialog(person = null) {
  personTarget = person;
  $("person-dialog-title").textContent = person ? "Rename" : "Add person";
  $("person-save").textContent = person ? "Save" : "Add";
  $("person-name").value = person?.name || "";
  openDialog("person-dialog", "person-name");
}
let keyTarget = null;
function openKeyDialog(person, key = null) {
  keyTarget = { person, key };
  $("key-dialog-title").textContent = key ? `Models for ${key.label}` : `New key for ${person.name}`;
  $("key-label-field").hidden = Boolean(key);
  $("key-models-legend").classList.toggle("sr-only", Boolean(key));
  $("key-label").required = !key;
  $("key-label").value = "";
  $("key-save").textContent = key ? "Save" : "Create key";
  const models = enabledModels();
  const checks = $("key-models");
  if (!models.length) {
    const none = node("p", "No models are enabled. ", "none"),
      link = node("a", "Open Models");
    link.href = "#models";
    link.addEventListener("click", () => $("key-dialog").close());
    none.append(link);
    checks.replaceChildren(none);
  } else
    checks.replaceChildren(
      ...models.map((model) => {
        const label = node("label"),
          input = node("input");
        input.type = "checkbox";
        input.value = model.id;
        input.checked = key ? key.models.includes(model.id) : true;
        label.append(input, model.display_name);
        return label;
      }),
    );
  openDialog("key-dialog", key ? undefined : "key-label");
}
let readySecret = false,
  readyCopied = false;
async function openReady(person, label, result) {
  const config = await api(`keys/${result.id}/config`);
  config.providers["shared-claude"].apiKey = result.secret;
  readySecret = true;
  readyCopied = false;
  $("ready-title-text").textContent = `${person.name}’s ${label} key`;
  $("ready-key-field").hidden = false;
  $("ready-key").textContent = result.secret;
  $("ready-config").textContent = JSON.stringify(config, null, 2);
  openDialog("ready-dialog", "ready-done");
}
async function openSetup(person, key) {
  const config = await api(`keys/${key.id}/config`);
  readySecret = false;
  $("ready-title-text").textContent = `opencodex setup · ${key.label}`;
  $("ready-key-field").hidden = true;
  $("ready-config").textContent = JSON.stringify(config, null, 2);
  openDialog("ready-dialog", "ready-done");
}
let revokeTarget = null;
function confirmRevoke(key) {
  revokeTarget = key;
  $("confirm-title").textContent = `Revoke ${key.label}?`;
  openDialog("confirm-dialog");
  document.querySelector("#confirm-dialog [data-close]").focus();
}
let aliasTarget = null;
function openAlias(model) {
  aliasTarget = model;
  $("alias-title").textContent = `Alias for ${model.display_name}`;
  $("alias-name").value = "";
  openDialog("alias-dialog", "alias-name");
}
function sheet(formId, action) {
  $(formId).addEventListener("submit", (event) => {
    event.preventDefault();
    const submit = $(formId).querySelector('[type="submit"]');
    run(async () => {
      await action();
      $(formId).closest("dialog").close();
    }, submit);
  });
}

/* Models */
function renderModels() {
  const list = $("model-list"),
    connected = state.claude?.state === "connected";
  if (!state.models.length) {
    const empty = node("li", undefined, "empty-row");
    empty.append(node("span", "No models yet"));
    const action = connected ? node("button", "Check for new models", "button") : node("a", "Connect Claude", "button");
    if (connected) {
      action.type = "button";
      action.addEventListener("click", () => $("refresh-models").click());
    } else action.href = "#connection";
    empty.append(action);
    list.replaceChildren(empty);
    return;
  }
  const models = [...state.models].sort((a, b) => Boolean(a.reviewed_at) - Boolean(b.reviewed_at));
  list.replaceChildren(
    ...models.map((model) => {
      const row = node("li", undefined, "model-row"),
        name = node("div", undefined, "model-name"),
        title = node("span", model.display_name);
      if (!model.reviewed_at) title.append(node("span", "New", "badge"));
      name.append(title, node("code", model.id));
      const actions = node("div", undefined, "model-actions");
      if (model.enabled)
        actions.append(menuButton(`Actions for ${model.display_name}`, () => [{ label: "Add alias", action: () => openAlias(model) }]));
      const toggle = node("button", undefined, "switch");
      toggle.type = "button";
      toggle.setAttribute("role", "switch");
      toggle.setAttribute("aria-checked", String(Boolean(model.enabled)));
      toggle.setAttribute("aria-label", model.display_name);
      toggle.addEventListener("click", () =>
        run(async () => {
          await api(`models/${encodeURIComponent(model.id)}`, "PUT", { enabled: !model.enabled });
          await loadAccess();
          notice(`${model.display_name} ${model.enabled ? "disabled" : "enabled"}.`);
          document.querySelector(`[role="switch"][aria-label="${CSS.escape(model.display_name)}"]`)?.focus();
        }, toggle),
      );
      actions.append(toggle);
      row.append(name, actions);
      return row;
    }),
  );
}

/* Sign in page */
function initializeLogin() {
  $("login-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const control = event.submitter;
    control.disabled = true;
    $("login-error").textContent = "";
    try {
      await api("login", "POST", { password: $("password").value });
      location.assign("/admin");
    } catch (error) {
      $("login-error").textContent = error.status === 401 ? "That password is incorrect." : error.message;
      $("password").value = "";
      $("password").focus();
    } finally {
      control.disabled = false;
    }
  });
}

async function initialize() {
  if ($("login-form")) return initializeLogin();
  onReauth(connection);
  initializeAnalytics({
    addPerson: () => openPersonDialog(),
    newKey: (person) => openKeyDialog(person),
  });
  showPage();
  addEventListener("hashchange", () => {
    closeMenu();
    showPage(true);
  });
  document.querySelector(".skip-link").addEventListener("click", (event) => {
    event.preventDefault();
    $("main").focus();
  });
  document.addEventListener("click", (event) => {
    if (menuOwner && !$("menu").contains(event.target) && !menuOwner.contains(event.target)) closeMenu();
  });
  document.addEventListener("keydown", (event) => {
    if (!menuOwner) return;
    const items = [...$("menu").querySelectorAll("button")],
      index = items.indexOf(document.activeElement);
    if (event.key === "Escape") closeMenu(true);
    else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      items[(index + (event.key === "ArrowDown" ? 1 : items.length - 1)) % items.length].focus();
    } else if (event.key === "Tab") closeMenu();
  });
  addEventListener("resize", () => closeMenu());
  document.querySelectorAll("[data-close]").forEach((control) =>
    control.addEventListener("click", () => control.closest("dialog").close()));
  $("signout").addEventListener("click", (event) =>
    run(async () => {
      await api("logout", "POST");
      location.assign("/admin/login");
    }, event.currentTarget));
  $("add-person").addEventListener("click", () => openPersonDialog());
  sheet("person-form", async () => {
    const name = $("person-name").value;
    if (personTarget) await api(`people/${personTarget.id}`, "PATCH", { name });
    else await api("people", "POST", { name });
    await loadAccess();
    notice(personTarget ? "Name updated." : `${name} added.`);
  });
  sheet("key-form", async () => {
    const { person, key } = keyTarget;
    const models = [...$("key-models").querySelectorAll("input:checked")].map((input) => input.value);
    if (key) {
      await api(`keys/${key.id}/models`, "PUT", { models });
      await loadAccess();
      notice(`Models updated for ${key.label}.`);
      return;
    }
    const label = $("key-label").value.trim();
    const result = await api("keys", "POST", { person_id: person.id, label, models });
    await loadAccess();
    queueMicrotask(() => run(() => openReady(person, label, result)));
  });
  sheet("confirm-dialog", async () => {
    await api(`keys/${revokeTarget.id}`, "DELETE");
    await loadAccess();
    notice(`${revokeTarget.label} revoked.`);
  });
  sheet("alias-form", async () => {
    await api(`models/${encodeURIComponent(aliasTarget.id)}/aliases`, "POST", { alias: $("alias-name").value.trim() });
    notice("Alias added.");
  });
  document.querySelectorAll("[data-copy]").forEach((control) =>
    control.addEventListener("click", async () => {
      const target = $(control.dataset.copy);
      try {
        await navigator.clipboard.writeText(target.textContent);
        if (readySecret) readyCopied = true;
        control.textContent = "Copied";
        setTimeout(() => (control.textContent = "Copy"), 1600);
      } catch {
        getSelection().selectAllChildren(target);
        $("copy-status").textContent = "Press ⌘C or Ctrl+C to copy the selection.";
      }
    }));
  $("ready-done").addEventListener("click", () => $("ready-dialog").close());
  $("ready-dialog").addEventListener("cancel", (event) => {
    if (!readySecret || readyCopied) return;
    event.preventDefault();
    $("copy-status").textContent = "Copy the key first, or press Done to discard it.";
  });
  $("ready-dialog").addEventListener("close", () => {
    $("ready-key").textContent = "";
    $("ready-config").textContent = "";
    $("copy-status").textContent = "";
    readySecret = false;
  });
  $("reconnect").addEventListener("click", () => {
    flowOpen = true;
    authorizeAt = 0;
    renderConnection();
    $("authorize-link").focus();
  });
  $("redirect-url").addEventListener("input", () => {
    $("finish-oauth").disabled = !$("redirect-url").value.trim();
  });
  $("oauth-form").addEventListener("submit", (event) => {
    event.preventDefault();
    run(async () => {
      const redirect = $("redirect-url").value;
      $("redirect-url").value = "";
      $("finish-oauth").disabled = true;
      await api("claude/complete", "POST", { redirect_url: redirect });
      flowOpen = false;
      authorizeAt = 0;
      await connection();
      await loadAccess();
      notice("Claude connected.");
    }, $("finish-oauth"));
  });
  $("refresh-models").addEventListener("click", (event) =>
    run(async () => {
      const result = await api("models/refresh", "POST");
      await loadAccess();
      notice(`${result.discovered} models found.`);
    }, event.currentTarget));
  await connection();
  await loadAccess();
  await loadUsage();
}
initialize().catch((error) => notice(error.message, true));
