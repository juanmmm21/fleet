// Web de fleet: lista de tareas en directo, detalle con salida y diff, y acciones.
// Todo texto que viene de la API entra con textContent, nunca como HTML: la salida de un
// agente o un diff pueden traer cualquier cosa.
"use strict";

// Rutas relativas: igual en 127.0.0.1 que detrás de `tailscale serve` con un prefijo
const API = "v1/";
// Lo que se guarda de salida por tarea; lo viejo se ve entero en el registro del demonio
const MAX_LINES = 5000;
const LIVE_STATES = new Set(["provisioning", "agent", "preflight"]);
const FINAL_STATES = new Set(["merged", "dropped"]);

const state = {
  // Nodo que se está viendo; `null` es este mismo (ADR 0002: los demás van por /v1/nodes/…)
  node: null,
  localNode: null,
  nodes: [],
  source: null,
  tasks: new Map(),
  overlaps: [],
  selected: null,
  view: "log",
  // Salida por tarea, solo de las que ya se han abierto: líneas del registro y luego en directo
  output: new Map(),
};

const $ = (id) => document.getElementById(id);

class ApiError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function apiBase() {
  return state.node ? `${API}nodes/${encodeURIComponent(state.node)}/` : API;
}

// `root`: la petición va a este demonio aunque se esté viendo otro nodo (crear, nodos)
async function request(method, path, body, root = false) {
  const init = { method, headers: {} };
  if (body !== undefined) {
    init.headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const response = await fetch((root ? API : apiBase()) + path, init);
  const text = await response.text();
  if (!response.ok) {
    let code = "http_" + response.status;
    let message = text || response.statusText;
    try {
      const parsed = JSON.parse(text);
      if (parsed && parsed.error) {
        code = parsed.error.code;
        message = parsed.error.message;
      }
    } catch (err) {
      // No era JSON (p. ej. un proxy por delante): se enseña el texto tal cual
      console.warn("non-JSON error body", err);
    }
    throw new ApiError(code, message);
  }
  const type = response.headers.get("Content-Type") || "";
  return type.startsWith("application/json") ? JSON.parse(text) : text;
}

function showStatus(message) {
  const status = $("status");
  status.textContent = message;
  status.hidden = !message;
}

function reportError(err) {
  const message = err instanceof ApiError ? `${err.code}: ${err.message}` : String(err);
  showStatus(message);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function firstLine(text) {
  const line = text.split("\n").find((l) => l.trim() !== "") || "";
  return line.length > 120 ? line.slice(0, 117) + "…" : line;
}

function repoName(path) {
  const parts = String(path).split("/").filter(Boolean);
  return parts.length ? parts[parts.length - 1] : String(path);
}

function agentLabel(agent) {
  return agent.model ? `${agent.kind}:${agent.model}` : agent.kind;
}

function age(ms) {
  const seconds = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

function sortedTasks() {
  // Los ids se ordenan por creación: la más nueva arriba
  return [...state.tasks.values()].sort((a, b) => (a.id < b.id ? 1 : -1));
}

function renderList() {
  const list = $("task-list");
  list.replaceChildren();
  const tasks = sortedTasks();
  $("empty").hidden = tasks.length > 0;
  for (const task of tasks) {
    const item = el("li");
    item.dataset.id = task.id;
    item.title = task.prompt;
    if (task.id === state.selected) item.classList.add("selected");
    const badge = el("span", "badge", task.state);
    badge.dataset.state = task.state;
    item.append(
      badge,
      el("span", "title", firstLine(task.prompt)),
      el("span", "sub", `${repoName(task.repo)} · ${agentLabel(task.agent)} · ${age(task.updated_at_ms)}`),
    );
    item.addEventListener("click", () => select(task.id));
    list.append(item);
  }
}

function renderOverlaps() {
  const box = $("overlaps");
  const relevant = state.overlaps.filter((o) => state.tasks.has(o.a) || state.tasks.has(o.b));
  box.hidden = relevant.length === 0;
  box.replaceChildren();
  for (const overlap of relevant) {
    box.append(
      el("div", null, `${overlap.b} touches the same files as ${overlap.a}: ${overlap.files.join(", ")}`),
    );
  }
}

function renderDetail() {
  const task = state.selected && state.tasks.get(state.selected);
  $("detail").hidden = !task;
  if (!task) return;
  const badge = $("d-state");
  badge.textContent = task.state;
  badge.dataset.state = task.state;
  $("d-id").textContent = task.id;
  $("d-meta").textContent = `${task.repo} · ${task.branch} → ${task.base} · ${agentLabel(task.agent)}`;
  $("d-prompt").textContent = task.prompt;

  const failure = $("d-failure");
  failure.hidden = !(task.state === "failed" && task.failure);
  failure.textContent = task.failure || "";

  $("d-question").hidden = task.state !== "waiting";
  $("d-question-text").textContent = task.question || "";

  const preflight = $("d-preflight");
  preflight.hidden = !task.preflight;
  if (task.preflight) {
    const p = task.preflight;
    const result = p.timed_out ? "timed out" : p.exit_code === 0 ? "passed" : `exit ${p.exit_code ?? "signal"}`;
    const passed = !p.timed_out && p.exit_code === 0;
    // Con varias comprobaciones encadenadas, en verde se nombran todas y en rojo la que falló
    const chained = p.checks?.length ? (passed ? `${p.checks.join(", ")}: ` : `${p.checks.at(-1)}: `) : "";
    preflight.textContent = `Pre-flight: ${chained}${p.command.join(" ")} — ${result} in ${(p.duration_ms / 1000).toFixed(1)}s`;
  }

  $("merge").hidden = task.state !== "ready";
  $("cancel").hidden = !LIVE_STATES.has(task.state);
  $("drop").hidden = FINAL_STATES.has(task.state);

  for (const tab of document.querySelectorAll(".tabs button")) {
    tab.classList.toggle("active", tab.dataset.tab === state.view);
  }
}

function lineClass(line, view) {
  if (view === "diff") {
    if (line.startsWith("+") && !line.startsWith("+++")) return "add";
    if (line.startsWith("-") && !line.startsWith("---")) return "del";
    if (line.startsWith("@@")) return "hunk";
    return null;
  }
  return line.startsWith("[stderr] ") ? "err" : null;
}

function renderLines(lines, view) {
  const pre = $("output");
  const stick = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
  pre.replaceChildren(...lines.map((line) => el("span", lineClass(line, view), line + "\n")));
  if (view === "log" && stick) pre.scrollTop = pre.scrollHeight;
}

function appendLine(line) {
  const pre = $("output");
  const stick = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 8;
  pre.append(el("span", lineClass(line, "log"), line + "\n"));
  while (pre.childNodes.length > MAX_LINES) pre.firstChild.remove();
  if (stick) pre.scrollTop = pre.scrollHeight;
}

function splitLines(text) {
  const lines = text.split("\n");
  if (lines.length && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

async function loadView() {
  const id = state.selected;
  const view = state.view;
  if (!id) return;
  try {
    if (view === "diff") {
      // El diff se pide cada vez: cambia mientras el agente trabaja
      const patch = await request("GET", `tasks/${id}/diff`);
      if (state.selected === id && state.view === view) {
        renderLines(patch ? splitLines(patch) : ["(no changes)"], view);
      }
      return;
    }
    if (!state.output.has(id)) {
      const log = await request("GET", `tasks/${id}/log`);
      // Lo que llegara en directo mientras tanto ya está en el registro
      state.output.set(id, splitLines(log).slice(-MAX_LINES));
    }
    if (state.selected === id && state.view === view) renderLines(state.output.get(id), view);
  } catch (err) {
    if (err instanceof ApiError && err.code === "no_worktree") {
      renderLines(["(the worktree is gone: nothing to diff)"], view);
      return;
    }
    reportError(err);
  }
}

function select(id) {
  state.selected = id;
  // Un error de otra tarea ya no viene a cuento
  showStatus("");
  history.replaceState(null, "", "#" + id);
  renderList();
  renderDetail();
  loadView();
}

function upsertTask(task) {
  const previous = state.tasks.get(task.id);
  state.tasks.set(task.id, task);
  renderList();
  if (task.id === state.selected) {
    renderDetail();
    // Con cada cambio de estado el diff cambia, y el registro gana lo que no llega en directo
    // (cabeceras, pre-flight): se vuelve a pedir
    if (previous && previous.state !== task.state) {
      state.output.delete(task.id);
      loadView();
    }
  } else if (previous && previous.state !== task.state) {
    state.output.delete(task.id);
  }
}

async function refresh() {
  const [tasks, overlaps] = await Promise.all([request("GET", "tasks"), request("GET", "overlaps")]);
  state.tasks = new Map(tasks.map((t) => [t.id, t]));
  state.overlaps = overlaps;
  if (state.selected && !state.tasks.has(state.selected)) state.selected = null;
  if (!state.selected) {
    const wanted = location.hash.slice(1);
    const newest = sortedTasks()[0];
    state.selected = state.tasks.has(wanted) ? wanted : newest ? newest.id : null;
  }
  // Tras reconectar, lo que se perdió está en el registro: se vuelve a pedir
  state.output.clear();
  renderList();
  renderOverlaps();
  renderDetail();
  loadView();
}

function renderNodes() {
  const viewing = $("node-select");
  const target = $("new-task").node;
  const keepTarget = target.value;
  viewing.replaceChildren();
  target.replaceChildren(new Option("this node", ""), new Option("auto", "auto"));
  for (const node of state.nodes) {
    const label = node.online ? node.name : `${node.name} (offline)`;
    const option = new Option(label, node.local ? "" : node.name);
    option.disabled = !node.online;
    viewing.append(option);
    if (!node.local && node.online) target.append(new Option(node.name, node.name));
  }
  viewing.value = state.node || "";
  target.value = [...target.options].some((o) => o.value === keepTarget) ? keepTarget : "";
  viewing.hidden = state.nodes.length < 2;
}

async function loadNodes() {
  try {
    state.nodes = await request("GET", "nodes", undefined, true);
  } catch (err) {
    // Sin la lista se sigue viendo este nodo, que es lo que importa
    console.warn("cannot list the nodes", err);
    state.nodes = [];
  }
  renderNodes();
}

function viewNode(node) {
  state.node = node || null;
  state.tasks = new Map();
  state.output.clear();
  state.overlaps = [];
  state.selected = null;
  history.replaceState(null, "", location.pathname + location.search);
  showStatus("");
  renderNodes();
  renderList();
  renderOverlaps();
  renderDetail();
  setConnection("connecting");
  connect();
}

function setConnection(name) {
  const conn = $("conn");
  conn.dataset.state = name;
  conn.textContent = name;
}

function connect() {
  if (state.source) state.source.close();
  const source = new EventSource(apiBase() + "events");
  state.source = source;
  source.addEventListener("open", () => {
    setConnection("live");
    // Con el flujo ya abierto, para no perder lo que pase entre la lista y los eventos
    refresh().catch(reportError);
  });
  source.addEventListener("error", () => {
    // EventSource reintenta solo
    setConnection("offline");
  });
  source.addEventListener("task", (event) => upsertTask(JSON.parse(event.data).task));
  source.addEventListener("output", (event) => {
    const data = JSON.parse(event.data);
    const lines = state.output.get(data.id);
    if (!lines) return;
    const line = (data.stream === "stderr" ? "[stderr] " : "") + data.line;
    lines.push(line);
    if (lines.length > MAX_LINES) lines.splice(0, lines.length - MAX_LINES);
    if (data.id === state.selected && state.view === "log") appendLine(line);
  });
  source.addEventListener("overlaps", (event) => {
    state.overlaps = JSON.parse(event.data).overlaps;
    renderOverlaps();
  });
  source.addEventListener("lagged", () => refresh().catch(reportError));
}

async function act(button, path, body) {
  const id = state.selected;
  if (!id) return;
  button.disabled = true;
  try {
    const result = await request("POST", `tasks/${id}/${path}`, body);
    showStatus("");
    if (result && result.id && result.state) upsertTask(result);
    return true;
  } catch (err) {
    reportError(err);
    return false;
  } finally {
    button.disabled = false;
  }
}

function agentFromForm(form) {
  const kind = form.agent.value;
  const model = form.model.value.trim();
  if (kind === "ollama" && !model) throw new Error("ollama needs a model");
  return model ? { kind, model } : { kind };
}

function wire() {
  $("new-toggle").addEventListener("click", () => {
    const form = $("new-task");
    form.hidden = !form.hidden;
    if (!form.hidden) form.repo.focus();
  });

  $("new-task").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const submit = form.querySelector("button[type=submit]");
    submit.disabled = true;
    try {
      const body = { repo: form.repo.value.trim(), prompt: form.prompt.value, agent: agentFromForm(form) };
      const base = form.base.value.trim();
      if (base) body.base = base;
      if (form.node.value) body.node = form.node.value;
      const task = await request("POST", "tasks", body, true);
      showStatus("");
      form.prompt.value = "";
      form.hidden = true;
      const where = task.node && task.node !== state.localNode ? task.node : null;
      if (where !== state.node) viewNode(where);
      upsertTask(task);
      select(task.id);
    } catch (err) {
      reportError(err);
    } finally {
      submit.disabled = false;
    }
  });

  $("answer-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const form = event.currentTarget;
    const answer = form.answer.value.trim();
    if (!answer) return;
    if (await act(form.querySelector("button"), "answer", { answer })) form.answer.value = "";
  });

  $("merge").addEventListener("click", (event) => act(event.currentTarget, "merge"));
  $("cancel").addEventListener("click", (event) => act(event.currentTarget, "cancel"));
  $("drop").addEventListener("click", (event) => {
    if (confirm(`Drop task ${state.selected}? Its worktree and branch are deleted.`)) {
      act(event.currentTarget, "drop");
    }
  });

  for (const tab of document.querySelectorAll(".tabs button")) {
    tab.addEventListener("click", () => {
      state.view = tab.dataset.tab;
      renderDetail();
      loadView();
    });
  }

  $("node-select").addEventListener("change", (event) => viewNode(event.currentTarget.value));
  // Los nodos van y vienen (el Mac se duerme): la lista se rehace de vez en cuando
  setInterval(loadNodes, 30000);

  // Un enlace a `#<id>` con la página ya abierta (replaceState de `select` no lo dispara)
  window.addEventListener("hashchange", () => {
    const wanted = location.hash.slice(1);
    if (state.tasks.has(wanted) && wanted !== state.selected) select(wanted);
  });

  // Las edades de la lista se quedan viejas aunque no lleguen eventos
  setInterval(renderList, 30000);
}

async function start() {
  wire();
  try {
    const health = await request("GET", "health", undefined, true);
    state.localNode = health.node;
    $("node").textContent = `${health.node} · v${health.version}`;
  } catch (err) {
    reportError(err);
  }
  await loadNodes();
  connect();
}

start();
