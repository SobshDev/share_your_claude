// Helpers and state shared by app.js (dashboard and login) and analytics.js.
export const $ = (id) => document.getElementById(id);
let csrf = "";
export function setCsrf(token) {
  csrf = token;
}
// Access data loaded by app.js and read by analytics.js.
export const state = { people: [], keys: [], models: [] };
export const number = (value) =>
  value == null ? "—" : new Intl.NumberFormat().format(value);
export function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = text;
  if (className) element.className = className;
  return element;
}
// Both live regions stay in the accessibility tree; new text is set on the
// next frame after clearing so a repeated message is announced again.
let noticeFrame = 0;
export function notice(message, error = false) {
  const status = $("notice"),
    alert = $("notice-error");
  if (!status || !alert) return;
  cancelAnimationFrame(noticeFrame);
  status.textContent = "";
  alert.textContent = "";
  if (message)
    noticeFrame = requestAnimationFrame(() => {
      (error ? alert : status).textContent = message;
    });
}
export async function api(path, method = "GET", body) {
  let response;
  try {
    response = await fetch(`/admin/api/${path}`, {
      method,
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
  } catch {
    throw new Error(
      "Could not reach the router. Check your connection and try again.",
    );
  }
  if (response.status === 401 && path !== "login") {
    location.assign("/admin/login");
    throw new Error("Your session expired. Sign in again.");
  }
  // An empty body (204, 201) is null; a body that is not JSON is undefined.
  const raw = await response.text().catch(() => "");
  let data;
  try {
    data = raw ? JSON.parse(raw) : null;
  } catch {
    data = undefined;
  }
  if (!response.ok) {
    const error = new Error(
      data?.error?.message || `Request failed (${response.status}). Try again.`,
    );
    error.status = response.status;
    error.type = data?.error?.type;
    throw error;
  }
  if (data === undefined)
    throw new Error("Unexpected response from the router. Try again.");
  return data;
}
// app.js registers a handler that re-reads the Claude connection state.
let reauthHandler = null;
export function onReauth(handler) {
  reauthHandler = handler;
}
export async function run(action, button) {
  // Keyboard users keep their place: re-rendering replaces the control, so
  // focus returns to its replacement (by data-focus-key) or a fallback.
  const hadFocus =
      button &&
      (document.activeElement === button ||
        button.form?.contains(document.activeElement)),
    focusKey = button?.dataset.focusKey,
    focusFallback = button?.dataset.focusFallback;
  if (button) {
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
  }
  notice("");
  try {
    await action();
  } catch (error) {
    notice(error.message || "Could not connect. Try again.", true);
    // A reauth 503 means the server just marked Claude as disconnected.
    if (error.status === 503 && error.type === "authentication_error")
      if (reauthHandler) await reauthHandler().catch(() => {});
  } finally {
    if (button) {
      button.disabled = button.dataset.unavailable === "true";
      button.removeAttribute("aria-busy");
    }
    if (hadFocus) restoreFocus(button, focusKey, focusFallback);
  }
}
function restoreFocus(button, ...keys) {
  const active = document.activeElement;
  if (active && active !== document.body && active.isConnected) return;
  const target = [
    button,
    ...keys.map(
      (key) =>
        key && document.querySelector(`[data-focus-key="${CSS.escape(key)}"]`),
    ),
  ].find(
    (element) =>
      element?.isConnected && !element.disabled && !element.closest("[hidden]"),
  );
  target?.focus();
}
export function focusKey(element, key, fallback) {
  element.dataset.focusKey = key;
  if (fallback) element.dataset.focusFallback = fallback;
  return element;
}
export function button(label, action, className = "secondary") {
  const element = node("button", label, className);
  element.type = "button";
  element.addEventListener("click", () => run(() => action(element), element));
  return element;
}
