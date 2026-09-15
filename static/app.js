"use strict";
const $ = (id) => document.getElementById(id);
let csrf = "";
let people = [],
  keys = [],
  models = [];
const number = (value) =>
  value == null ? "—" : new Intl.NumberFormat().format(value);
const date = (value) =>
  value ? new Date(value).toLocaleString() : "Never used";
function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  if (className) element.className = className;
  return element;
}
function notice(message, error = false) {
  const box = $("notice");
  if (!box) return;
  box.textContent = message;
  box.className = error ? "notice error" : "notice";
  box.hidden = !message;
}
async function api(path, method = "GET", body) {
  const response = await fetch(`/admin/api/${path}`, {
    method,
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  if (response.status === 401 && path !== "login") {
    location.assign("/admin/login");
    throw new Error("Your session expired. Sign in again.");
  }
  const data =
    response.status === 204 ? null : await response.json().catch(() => null);
  if (!response.ok)
    throw new Error(
      data?.error?.message || `Request failed (${response.status}). Try again.`,
    );
  return data;
}
async function run(action, button) {
  if (button) button.disabled = true;
  notice("");
  try {
    await action();
  } catch (error) {
    notice(error.message || "Could not connect. Try again.", true);
  } finally {
    if (button) button.disabled = false;
  }
}
function button(label, action, className = "secondary") {
  const element = node("button", label, className);
  element.type = "button";
  element.addEventListener("click", () => run(() => action(element), element));
  return element;
}
function formAction(id, action) {
  $(id).addEventListener("submit", (event) => {
    event.preventDefault();
    run(action, event.submitter);
  });
}
function showOutput(title, description, content, copyLabel) {
  $("output-title").textContent = title;
  $("output-description").textContent = description;
  $("output-content").textContent = content;
  $("copy-output").textContent = copyLabel;
  $("copy-status").textContent = "";
  $("output-dialog").showModal();
}
function page() {
  const name = ["overview", "keys", "connection"].includes(
    location.hash.slice(1),
  )
    ? location.hash.slice(1)
    : "overview";
  document.querySelectorAll(".page").forEach((element) => {
    element.hidden = element.id !== `page-${name}`;
  });
  document.querySelectorAll("[data-page]").forEach((element) => {
    if (element.dataset.page === name)
      element.setAttribute("aria-current", "page");
    else element.removeAttribute("aria-current");
  });
  document.title = `${{ overview: "Usage overview", keys: "Friends & keys", connection: "Claude connection" }[name]} · Shared Router`;
}
async function connection() {
  const data = await api("me");
  csrf = data.csrf_token;
  const connected = data.claude?.state === "connected";
  $("connection-label").textContent = connected
    ? "Claude connected"
    : "Claude needs connection";
  document.body.classList.toggle("connected", connected);
  $("connection-title").textContent = connected
    ? "Claude is connected"
    : "Connect your Claude account";
  $("connection-detail").textContent = connected
    ? "Requests use this account. Expired access tokens refresh automatically."
    : "Sign in once to start sharing access through individual keys.";
  $("connect-claude").textContent = connected
    ? "Reconnect Claude"
    : "Connect Claude";
  $("refresh-models").disabled = !connected;
}
async function loadAccess() {
  [people, keys, models] = await Promise.all([
    api("people"),
    api("keys"),
    api("models"),
  ]);
  renderPeople();
  renderModels();
  updateUsageFilters();
}
function renderPeople() {
  const select = $("key-person"),
    previous = select.value;
  select.replaceChildren();
  const empty = node(
    "option",
    people.length ? "Select a friend" : "Add a friend first",
  );
  empty.value = "";
  select.append(empty);
  for (const person of people) {
    const option = node("option", person.name);
    option.value = person.id;
    select.append(option);
  }
  if (people.some((p) => p.id === previous)) select.value = previous;
  else if (people.length === 1) select.value = people[0].id;
  $("issue-key").disabled = !people.length;
  $("people-empty").hidden = people.length > 0;
  const list = $("people-list");
  list.replaceChildren();
  for (const person of people) {
    const section = node("section", undefined, "friend");
    const heading = node("div", undefined, "friend-heading");
    heading.append(node("h3", person.name));
    heading.append(button("View usage", () => selectUsagePerson(person.id), "text-button"));
    heading.append(
      button(
        "Rename",
        async () => {
          if (section.querySelector(".rename-form")) return;
          const form = node("form", undefined, "rename-form input-action"),
            input = node("input");
          input.value = person.name;
          input.required = true;
          input.maxLength = 100;
          input.setAttribute("aria-label", "New name");
          const save = node("button", "Save name");
          save.type = "submit";
          form.append(
            input,
            save,
            button("Cancel", async () => form.remove()),
          );
          form.addEventListener("submit", (event) => {
            event.preventDefault();
            run(async () => {
              await api(`people/${person.id}`, "PATCH", { name: input.value });
              await loadAccess();
              notice("Name updated. Usage history is preserved.");
            }, save);
          });
          heading.after(form);
          input.focus();
        },
        "text-button",
      ),
    );
    section.append(heading);
    const ownKeys = keys.filter((key) => key.person_id === person.id);
    if (!ownKeys.length)
      section.append(
        node("p", "No keys yet. Create one above when you’re ready.", "helper"),
      );
    for (const key of ownKeys) {
      const row = node("div", undefined, "key-row");
      const identity = node("div");
      identity.append(
        node("strong", key.label),
        node("code", `${key.prefix}…`),
      );
      const meta = node("div", undefined, "key-meta");
      meta.append(
        node(
          "span",
          key.revoked_at ? "Revoked" : "Active",
          key.revoked_at ? "status revoked" : "status",
        ),
        node(
          "div",
          key.revoked_at
            ? `Revoked ${date(key.revoked_at)}`
            : `${key.models.filter((id) => models.some((m) => m.id === id && m.enabled && !m.blocked)).length} models · ${date(key.last_used_at)}`,
        ),
      );
      const actions = node("div", undefined, "key-actions");
      if (!key.revoked_at) {
        actions.append(
          button("Edit access", async () => {
            if (row.nextElementSibling?.classList.contains("grant-form")) {
              row.nextElementSibling.remove();
              return;
            }
            const form = node("form", undefined, "grant-form"),
              options = node("div", undefined, "grant-options");
            form.append(
              node("h3", `Model access for ${key.label}`),
              node(
                "p",
                "Fable 5.1 cannot be granted. Save with none selected to pause model access.",
                "helper",
              ),
            );
            for (const model of models.filter(
              (m) => m.enabled && m.reviewed_at && !m.blocked,
            )) {
              const label = node("label", undefined, "check-label"),
                input = node("input");
              input.type = "checkbox";
              input.value = model.id;
              input.checked = key.models.includes(model.id);
              label.append(input, node("span", model.display_name));
              options.append(label);
            }
            if (!options.children.length)
              options.append(
                node(
                  "p",
                  "Review and enable models on the Claude connection page first.",
                  "helper",
                ),
              );
            const save = node("button", "Save access");
            save.type = "submit";
            form.append(
              options,
              save,
              button("Cancel", async () => form.remove()),
            );
            form.addEventListener("submit", (event) => {
              event.preventDefault();
              run(async () => {
                await api(`keys/${key.id}/models`, "PUT", {
                  models: Array.from(
                    options.querySelectorAll("input:checked"),
                    (input) => input.value,
                  ),
                });
                await loadAccess();
                notice("Model access updated for new requests.");
              }, save);
            });
            row.after(form);
          }),
        );
        actions.append(
          button("opencodex setup", async () => {
            const config = await api(`keys/${key.id}/config`);
            showOutput(
              "opencodex setup",
              "Merge this provider into opencodex configuration on your friend’s computer. Set SHARED_CLAUDE_API_KEY to the key you issued, then restart opencodex.",
              JSON.stringify(config, null, 2),
              "Copy configuration",
            );
          }),
        );
        actions.append(
          button(
            "Revoke",
            async (control) => {
              if (control.dataset.confirm !== "yes") {
                control.dataset.confirm = "yes";
                control.textContent = "Confirm revoke";
                return;
              }
              await api(`keys/${key.id}`, "DELETE");
              await loadAccess();
              notice(
                "Key revoked. Existing requests can finish; new requests are blocked.",
              );
            },
            "secondary danger",
          ),
        );
      }
      row.append(identity, meta, actions);
      section.append(row);
    }
    list.append(section);
  }
}
function renderModels() {
  $("model-list").replaceChildren();
  for (const model of models) {
    const row = node("div", undefined, "model-row"),
      title = node("div");
    title.append(node("strong", model.display_name), node("code", model.id));
    row.append(title);
    if (model.blocked)
      row.append(node("span", "Reserved for owner", "status warning"));
    else
      row.append(
        button(
          model.enabled
            ? "Disable"
            : model.reviewed_at
              ? "Enable"
              : "Review & enable",
          async () => {
            await api(`models/${encodeURIComponent(model.id)}`, "PUT", {
              enabled: !model.enabled,
            });
            await loadAccess();
            notice(
              model.enabled
                ? "Model disabled for all keys."
                : "Model enabled. Add it to existing keys through Edit access.",
            );
          },
        ),
      );
    $("model-list").append(row);
  }
  if (models.length < 2)
    $("model-list").append(
      node(
        "p",
        "Connect your account, then refresh the catalog to discover available models.",
        "helper",
      ),
    );
}
async function initialize() {
  if ($("login-form")) {
    $("login-form").addEventListener("submit", async (event) => {
      event.preventDefault();
      const control = event.submitter;
      control.disabled = true;
      $("login-error").textContent = "";
      try {
        await api("login", "POST", { password: $("password").value });
        location.assign("/admin");
      } catch (error) {
        $("login-error").textContent = error.message;
      } finally {
        $("password").value = "";
        control.disabled = false;
      }
    });
    return;
  }
  page();
  window.addEventListener("hashchange", page);
  const today = new Date(),
    start = new Date(today);
  start.setUTCDate(start.getUTCDate() - 29);
  $("date-from").value = start.toISOString().slice(0, 10);
  $("date-to").value = today.toISOString().slice(0, 10);
  $("signout").addEventListener("click", (event) =>
    run(async () => {
      await api("logout", "POST");
      location.assign("/admin/login");
    }, event.currentTarget),
  );
  initializeAnalytics();
  formAction("usage-filter", loadUsage);
  $("refresh-usage").addEventListener("click", (event) =>
    run(loadUsage, event.currentTarget),
  );
  formAction("person-form", async () => {
    await api("people", "POST", { name: $("person-name").value });
    $("person-form").reset();
    await loadAccess();
    notice("Friend added. You can now create their key.");
  });
  formAction("key-form", async () => {
    const result = await api("keys", "POST", {
      person_id: $("key-person").value,
      label: $("key-label").value,
    });
    $("key-label").value = "";
    showOutput(
      "Your new API key",
      "Copy this key now and share it privately with your friend. It cannot be displayed again. Revoking the key will stop future requests.",
      result.secret,
      "Copy API key",
    );
    await loadAccess();
  });
  $("connect-claude").addEventListener("click", (event) =>
    run(async () => {
      const result = await api("claude/login", "POST");
      $("authorize-link").href = result.authorize_url;
      $("oauth-form").hidden = false;
      $("redirect-url").value = "";
      $("authorize-link").focus();
    }, event.currentTarget),
  );
  formAction("oauth-form", async () => {
    const redirect = $("redirect-url").value;
    $("redirect-url").value = "";
    await api("claude/complete", "POST", { redirect_url: redirect });
    $("oauth-form").hidden = true;
    await connection();
    notice(
      "Claude connected. Refresh the model catalog to review available models.",
    );
  });
  $("refresh-models").addEventListener("click", (event) =>
    run(async () => {
      const result = await api("models/refresh", "POST");
      await loadAccess();
      notice(
        `${result.discovered} models discovered. Review new models below.`,
      );
    }, event.currentTarget),
  );
  $("close-output").addEventListener("click", () => $("output-dialog").close());
  $("output-dialog").addEventListener("close", () => {
    $("output-content").textContent = "";
    $("copy-status").textContent = "";
  });
  $("copy-output").addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText($("output-content").textContent);
      $("copy-status").textContent = "Copied to clipboard.";
    } catch {
      $("copy-status").textContent =
        "Clipboard access is unavailable. Select and copy the text above.";
    }
  });
  await connection();
  await loadAccess();
  await loadUsage();
}
initialize().catch((error) => notice(error.message, true));
