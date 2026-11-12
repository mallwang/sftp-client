import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { open, save, ask } from "@tauri-apps/plugin-dialog";

// ---------- Types (mirror of the Rust structs) ----------
type Auth =
  | { type: "password"; password: string }
  | { type: "key"; keyPath: string; passphrase?: string };

interface ConnectInfo { home: string; fingerprint: string; newHostKey: boolean }
interface Entry {
  name: string; path: string; isDir: boolean; isSymlink: boolean;
  size: number; modified: number | null; permissions: string | null;
}
interface FolderSize { size: number; files: number; folders: number; skipped: number }
interface Progress {
  id: string; transferred: number; total: number;
  current: string; fileIndex: number; fileCount: number;
}

/** A running or queued upload/download. */
interface Transfer {
  id: string;
  batch: number;
  kind: "upload" | "download";
  name: string;
  /** Upload: target folder on the server. Download: local target folder. */
  targetDir: string;
  /** Upload only: full remote path of the file/folder being created. */
  remotePath: string;
  /** Upload: local source. Download: remote source. */
  source: string;
  /** Download only: full local file path. */
  localPath?: string;
  /** Set when the user declined to resume after a connection loss (partial file stays). */
  partialKept?: boolean;
  state: "queued" | "running";
  cancelRequested: boolean;
  transferred: number;
  total: number;
  current: string;
  fileIndex: number;
  fileCount: number;
  samples: { t: number; b: number }[];
  /** "45 MB/s · noch 4 Min 12 s" – one cached value shared by status bar and list row. */
  rateText: string;
  rateAt: number;
  startedAt?: number;
}

// ---------- DOM helpers ----------
const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const input = (id: string) => $<HTMLInputElement>(id);

const loginView = $("login-view");
const browserView = $("browser-view");
const statusEl = $("status");
const progressEl = $<HTMLProgressElement>("progress");
const transferLineEl = $("transfer-line");
const transferDetailsEl = $("transfer-details");

let currentPath = "/";
let lastEntries: Entry[] = [];
let lastMessage = "";
let lastMessageIsError = false;

// ---------- Utilities ----------
const nf = (digits: number) => new Intl.NumberFormat("de-DE", { maximumFractionDigits: digits });

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${nf(v < 10 ? 1 : 0).format(v)} ${units[i]}`;
}

function formatDuration(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} Min ${s % 60} s`;
  const h = Math.floor(s / 3600);
  return `${h} Std ${Math.floor((s % 3600) / 60)} Min`;
}

function formatDate(ts: number | null): string {
  return ts == null ? "" : new Date(ts * 1000).toLocaleString("de-DE");
}

const joinRemote = (dir: string, name: string) => (dir.endsWith("/") ? dir + name : `${dir}/${name}`);
const localBasename = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() ?? p;
const localDirname = (p: string) => p.replace(/[\\/][^\\/]*$/, "") || p;
const errorText = (e: unknown) => (typeof e === "string" ? e : e instanceof Error ? e.message : String(e));

// ---------- Log ("Protokoll") ----------
const LOG_LIMIT = 2000;
const logListEl = $("log-list");

/** Records what the user did (and what went wrong) in the side panel. */
function log(text: string, level: "info" | "error" = "info") {
  const stick = logListEl.scrollTop + logListEl.clientHeight >= logListEl.scrollHeight - 24;
  const li = document.createElement("li");
  if (level === "error") li.className = "error";
  const time = document.createElement("time");
  time.textContent = new Date().toLocaleTimeString("de-DE");
  const msg = document.createElement("span");
  msg.textContent = text;
  li.append(time, msg);
  logListEl.append(li);
  while (logListEl.children.length > LOG_LIMIT) logListEl.firstElementChild?.remove();
  if (stick) logListEl.scrollTop = logListEl.scrollHeight;
}

function toggleLog(show = $("log-panel").hidden) {
  $("log-panel").hidden = !show;
  $("log-btn").classList.toggle("active", show);
  if (show) logListEl.scrollTop = logListEl.scrollHeight;
}

/** Errors stay visible until dismissed, so they can't be overlooked. */
function showError(title: string, message: string) {
  log(`${title}: ${message}`, "error");
  const box = $("toasts");
  while (box.children.length >= 4) box.firstElementChild?.remove();
  const toast = document.createElement("div");
  toast.className = "toast";
  toast.setAttribute("role", "alert");
  const msg = document.createElement("div");
  msg.className = "msg";
  const b = document.createElement("b");
  b.textContent = title;
  msg.append(b, message);
  const close = document.createElement("button");
  close.textContent = "✕";
  close.title = "Schließen";
  close.addEventListener("click", () => toast.remove());
  toast.append(msg, close);
  box.append(toast);
}

function setStatus(text: string, isError = false) {
  lastMessage = text;
  lastMessageIsError = isError;
  renderFooter();
}

/** Simple input dialog, since window.prompt is not available in Tauri. */
function promptDialog(text: string, value = ""): Promise<string | null> {
  const dlg = $<HTMLDialogElement>("prompt-dialog");
  const field = input("prompt-input");
  $("prompt-text").textContent = text;
  field.value = value;
  return new Promise((resolve) => {
    dlg.addEventListener(
      "close",
      () => resolve(dlg.returnValue === "ok" && field.value.trim() ? field.value.trim() : null),
      { once: true },
    );
    dlg.returnValue = "";
    $("prompt-cancel").onclick = () => dlg.close("cancel");
    dlg.showModal();
    field.select();
  });
}

/** Runs an action with error handling, status updates and a visible error message. */
async function run<T>(label: string, action: () => Promise<T>, logText?: string): Promise<T | undefined> {
  setStatus(`${label} …`);
  try {
    const result = await action();
    setStatus(`${label} – fertig`);
    if (logText) log(logText);
    return result;
  } catch (e) {
    const text = errorText(e);
    setStatus(`${label} fehlgeschlagen: ${text}`, true);
    showError(`${label} fehlgeschlagen`, text);
    return undefined;
  }
}

// ---------- Login ----------
/** A saved connection. Passwords and passphrases are never stored – only these details. */
interface Profile {
  host: string;
  port: number;
  username: string;
  auth: "password" | "key";
  /** Path of the key file (not its content). */
  keyPath: string;
  lastUsed: number;
}
const HISTORY_KEY = "sftp-client:history";
const LEGACY_PROFILE_KEY = "sftp-client:last-profile";
const HISTORY_MAX = 10;

const sameProfile = (a: Profile, b: Profile) => a.host === b.host && a.port === b.port && a.username === b.username;

function readHistory(): Profile[] {
  try {
    const list = JSON.parse(localStorage.getItem(HISTORY_KEY) ?? "null");
    if (Array.isArray(list)) return list.filter((p) => p && typeof p.host === "string" && typeof p.username === "string");
    // One-time migration of the former single "last profile".
    const old = JSON.parse(localStorage.getItem(LEGACY_PROFILE_KEY) ?? "null");
    if (old?.host) return [{ host: old.host, port: Number(old.port) || 22, username: old.username ?? "", auth: old.auth === "key" ? "key" : "password", keyPath: old.keyPath ?? "", lastUsed: Date.now() }];
  } catch { /* broken data: start empty */ }
  return [];
}

function writeHistory(list: Profile[]) {
  localStorage.setItem(HISTORY_KEY, JSON.stringify(list.slice(0, HISTORY_MAX)));
  localStorage.removeItem(LEGACY_PROFILE_KEY);
}

function rememberProfile(p: Profile) {
  writeHistory([p, ...readHistory().filter((x) => !sameProfile(x, p))]);
}

function applyProfile(p: Profile) {
  input("host").value = p.host;
  input("port").value = String(p.port);
  input("username").value = p.username;
  input("key-path").value = p.keyPath;
  const radio = document.querySelector<HTMLInputElement>(`input[name="auth"][value="${p.auth}"]`);
  if (radio) radio.checked = true;
  updateAuthMode();
}

function renderHistory() {
  const list = readHistory();
  const box = $("history");
  const ul = $("history-list");
  ul.replaceChildren();
  box.hidden = list.length === 0;
  for (const p of list) {
    const li = document.createElement("li");
    const pick = document.createElement("button");
    pick.type = "button";
    pick.className = "history-item";
    pick.title = `${p.username}@${p.host}:${p.port} – zuletzt ${new Date(p.lastUsed).toLocaleString("de-DE")}`;
    const main = document.createElement("span");
    main.className = "history-main";
    main.textContent = `${p.username}@${p.host}`;
    const meta = document.createElement("span");
    meta.className = "history-meta";
    meta.textContent = `Port ${p.port} · ${p.auth === "key" ? "SSH-Key" : "Passwort"}`;
    pick.append(main, meta);
    pick.addEventListener("click", () => {
      applyProfile(p);
      // Continue where the secret is needed.
      input(p.auth === "key" ? "passphrase" : "password").focus();
    });
    const remove = document.createElement("button");
    remove.type = "button";
    remove.className = "history-remove";
    remove.textContent = "✕";
    remove.title = "Aus dem Verlauf entfernen";
    remove.addEventListener("click", () => {
      writeHistory(readHistory().filter((x) => !sameProfile(x, p)));
      renderHistory();
    });
    li.append(pick, remove);
    ul.append(li);
  }
}

$("history-clear").addEventListener("click", () => {
  writeHistory([]);
  renderHistory();
});

function loadProfile() {
  const last = readHistory()[0];
  if (last) applyProfile(last);
  else updateAuthMode();
  renderHistory();
}

function authMode(): "password" | "key" {
  return (document.querySelector<HTMLInputElement>('input[name="auth"]:checked')!.value as "password" | "key");
}

function updateAuthMode() {
  const mode = authMode();
  $("auth-password").hidden = mode !== "password";
  $("auth-key").hidden = mode !== "key";
}

document.querySelectorAll('input[name="auth"]').forEach((r) => r.addEventListener("change", updateAuthMode));

$("pick-key").addEventListener("click", async () => {
  const file = await open({ title: "Private Key auswählen", multiple: false, directory: false });
  if (typeof file === "string") input("key-path").value = file;
});

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const errEl = $("login-error");
  const btn = $<HTMLButtonElement>("connect-btn");
  errEl.hidden = true;

  const host = input("host").value.trim();
  const port = Number(input("port").value);
  const username = input("username").value.trim();
  const mode = authMode();
  const auth: Auth =
    mode === "password"
      ? { type: "password", password: input("password").value }
      : { type: "key", keyPath: input("key-path").value || "~/.ssh/id_ed25519", passphrase: input("passphrase").value || undefined };

  btn.disabled = true;
  btn.textContent = "Verbinde …";
  try {
    const info = await invoke<ConnectInfo>("connect", { host, port, username, auth });
    // Never store passwords/passphrases – only connection details.
    rememberProfile({ host, port, username, auth: mode, keyPath: input("key-path").value, lastUsed: Date.now() });
    input("password").value = "";
    input("passphrase").value = "";

    log(`Verbunden mit ${username}@${host}:${port}${info.newHostKey ? " (neuer Host-Key gespeichert)" : ""}`);
    $("conn-label").textContent = `${username}@${host}`;
    loginView.hidden = true;
    browserView.hidden = false;
    await navigate(info.home);
    setStatus(
      info.newHostKey
        ? `Neuer Host-Key gespeichert: ${info.fingerprint}`
        : `Verbunden (Host-Key ${info.fingerprint})`,
    );
  } catch (e) {
    errEl.textContent = errorText(e);
    errEl.hidden = false;
    log(`Anmeldung bei ${username}@${host}:${port} fehlgeschlagen: ${errorText(e)}`, "error");
  } finally {
    btn.disabled = false;
    btn.textContent = "Verbinden";
  }
});

// ---------- Browser ----------
async function navigate(path: string) {
  const before = currentPath;
  let target = path;
  const entries = await run("Lade Verzeichnis", async () => {
    const resolved = await invoke<string>("resolve_path", { path });
    const list = await invoke<Entry[]>("list_dir", { path: resolved });
    currentPath = resolved;
    target = resolved;
    return list;
  });
  if (entries) resetFolderSizes(target !== before);
  if (entries && target !== before) {
    selected.clear();
    selectionAnchor = null;
  } else if (entries) {
    for (const p of [...selected]) if (!entries.some((en) => en.path === p)) selected.delete(p);
  }
  if (entries) log(target === before ? `Aktualisiert: ${target} (${entries.length} Einträge)` : `Ordner geöffnet: ${target}`);
  input("path").value = currentPath;
  if (entries) {
    lastEntries = entries;
    renderFiles();
  }
}

const refresh = () => navigate(currentPath);

function button(label: string, onClick: () => void, cls = ""): HTMLButtonElement {
  const b = document.createElement("button");
  b.textContent = label;
  if (cls) b.className = cls;
  b.addEventListener("click", (e) => { e.stopPropagation(); onClick(); });
  b.addEventListener("dblclick", (e) => e.stopPropagation());
  return b;
}

interface GhostRefs { tr: HTMLTableRowElement; size: HTMLElement; state: HTMLElement }
const ghostRefs = new Map<string, GhostRefs>();
/** Real rows currently on screen by remote path (used to show download progress in place). */
const rowRefs = new Map<string, { tr: HTMLTableRowElement; size: HTMLElement }>();

/** Draws the file list: running/queued uploads first as "ghost" rows, then the real entries. */
function renderFiles() {
  const tbody = $("files");
  tbody.replaceChildren();
  ghostRefs.clear();
  sizeCells.clear();
  rowRefs.clear();

  const ghosts = transfers.filter((t) => t.kind === "upload" && t.targetDir === currentPath);
  // The half-written file already exists on the server – the ghost row replaces it.
  const ghostPaths = new Set(ghosts.map((t) => t.remotePath));
  const entries = lastEntries.filter((e) => !ghostPaths.has(e.path));

  if (ghosts.length === 0 && entries.length === 0) {
    tbody.innerHTML = `<tr><td colspan="5" class="empty">Ordner ist leer</td></tr>`;
    return;
  }
  for (const t of ghosts) tbody.append(ghostRow(t));
  for (const entry of entries) tbody.append(entryRow(entry));
}

// ---------- Selection (click, Ctrl+click, Shift+click, Ctrl+A) ----------
const selected = new Set<string>(); // by remote path
let selectionAnchor: string | null = null;

const entryRows = () => Array.from($("files").querySelectorAll<HTMLTableRowElement>("tr[data-path]"));

function paintSelection() {
  for (const tr of entryRows()) tr.classList.toggle("sel", selected.has(tr.dataset.path!));
}

function clearSelection() {
  selected.clear();
  selectionAnchor = null;
  paintSelection();
}

function selectRow(path: string, e: { ctrlKey: boolean; metaKey: boolean; shiftKey: boolean }) {
  if (e.shiftKey && selectionAnchor) {
    const paths = entryRows().map((r) => r.dataset.path!);
    const a = paths.indexOf(selectionAnchor), b = paths.indexOf(path);
    if (a >= 0 && b >= 0) {
      if (!(e.ctrlKey || e.metaKey)) selected.clear();
      for (const p of paths.slice(Math.min(a, b), Math.max(a, b) + 1)) selected.add(p);
    }
  } else if (e.ctrlKey || e.metaKey) {
    if (!selected.delete(path)) selected.add(path);
    selectionAnchor = path;
  } else {
    selected.clear();
    selected.add(path);
    selectionAnchor = path;
  }
  paintSelection();
}

const selectedEntries = () => lastEntries.filter((e) => selected.has(e.path));

// ---------- Folder sizes (calculated on demand via right-click) ----------
interface SizeJob {
  id: string;
  path: string;
  state: "running" | "done";
  size: number;
  files: number;
  folders: number;
  skipped: number;
}
const sizeJobs = new Map<string, SizeJob>(); // by folder path
const sizeCells = new Map<string, HTMLElement>(); // by folder path (cells currently on screen)
let sizeCounter = 0;

function sizeCellText(job: SizeJob): { text: string; title: string } {
  const count = `${nf(0).format(job.files)} Dateien`;
  if (job.state === "running") {
    return { text: `… ${formatSize(job.size)} (${count})`, title: "Wird berechnet – Rechtsklick → Berechnung abbrechen" };
  }
  const skipped = job.skipped > 0 ? `\n${job.skipped} Ordner waren nicht lesbar und sind nicht enthalten.` : "";
  return {
    text: `${job.skipped > 0 ? "≥ " : ""}${formatSize(job.size)}`,
    title: `${count}, ${nf(0).format(job.folders)} Unterordner${skipped}`,
  };
}

function updateSizeCell(path: string) {
  if (runningDownload(path)) return;
  const cell = sizeCells.get(path);
  const job = sizeJobs.get(path);
  if (!cell || !job) return;
  const { text, title } = sizeCellText(job);
  cell.textContent = text;
  cell.title = title;
}

async function calculateFolderSize(entry: Entry) {
  if (sizeJobs.get(entry.path)?.state === "running") return;
  const job: SizeJob = { id: `s${++sizeCounter}`, path: entry.path, state: "running", size: 0, files: 0, folders: 0, skipped: 0 };
  sizeJobs.set(entry.path, job);
  updateSizeCell(entry.path);
  log(`Größe von „${entry.path}“ wird berechnet`);
  try {
    const r = await invoke<FolderSize>("folder_size", { id: job.id, path: entry.path });
    if (sizeJobs.get(entry.path) !== job) return; // dropped meanwhile (left the folder / refreshed)
    Object.assign(job, r, { state: "done" });
    log(`Größe von „${entry.path}“: ${formatSize(r.size)} (${nf(0).format(r.files)} Dateien, ${nf(0).format(r.folders)} Unterordner${r.skipped ? `, ${r.skipped} Ordner nicht lesbar` : ""})`);
  } catch (e) {
    if (sizeJobs.get(entry.path) === job) sizeJobs.delete(entry.path);
    const text = errorText(e);
    if (text === "Abgebrochen") log(`Größenberechnung von „${entry.path}“ abgebrochen`);
    else showError(`Größe von „${entry.name}“ konnte nicht berechnet werden`, text);
  }
  updateSizeCell(entry.path);
  if (!sizeJobs.has(entry.path)) renderFiles();
}

function cancelFolderSize(path: string) {
  const job = sizeJobs.get(path);
  if (job?.state === "running") void invoke("cancel_transfer", { id: job.id });
}

/** Results are valid while you stay in the folder; a refresh or leaving drops them. */
function resetFolderSizes(leftFolder: boolean) {
  for (const [path, job] of sizeJobs) {
    if (job.state === "running" && !leftFolder) continue; // keep running calculations on refresh
    if (job.state === "running") cancelFolderSize(path);
    sizeJobs.delete(path);
  }
}

listen<{ id: string; size: number; files: number }>("size-progress", ({ payload }) => {
  for (const job of sizeJobs.values()) {
    if (job.id === payload.id && job.state === "running") {
      job.size = payload.size;
      job.files = payload.files;
      updateSizeCell(job.path);
    }
  }
});

/** Explains a Unix mode string like "drwxr-xr-x" in plain German. */
function describePermissions(mode: string, isDir: boolean): string {
  const kind = mode[0] === "l" ? "Verknüpfung (Symlink)" : isDir ? "Ordner" : "Datei";
  const words = isDir
    ? { r: "Inhalt ansehen", w: "Inhalt ändern (Dateien anlegen, umbenennen, löschen)", x: "Ordner öffnen" }
    : { r: "lesen/herunterladen", w: "ändern/überschreiben", x: "ausführen" };
  const who = ["Besitzer", "Gruppe", "Alle anderen"];
  const lines = [`${kind} – Zugriffsrechte (${mode.slice(1)})`];
  for (let i = 0; i < 3; i++) {
    const [r, w, x] = mode.slice(1 + i * 3, 4 + i * 3);
    const can = [r === "r" ? words.r : "", w === "w" ? words.w : "", x === "x" || x === "s" || x === "t" ? words.x : ""].filter(Boolean);
    lines.push(`${who[i]}: ${can.length ? can.join(", ") : "kein Zugriff"}`);
  }
  lines.push("", "Besitzer = Benutzer, dem der Eintrag gehört; Gruppe = Benutzergruppe des Besitzers.");
  if (mode[0] === "l") lines.push("Bei Verknüpfungen zählen die Rechte des Ziels.");
  return lines.join("\n");
}

function entryRow(entry: Entry): HTMLTableRowElement {
  const tr = document.createElement("tr");
  if (entry.isDir) tr.classList.add("dir");
  tr.dataset.path = entry.path;
  if (selected.has(entry.path)) tr.classList.add("sel");
  tr.addEventListener("click", (e) => selectRow(entry.path, e));

  const name = document.createElement("td");
  name.className = "name";
  const icon = entry.isDir ? "📁" : entry.isSymlink ? "🔗" : "📄";
  name.innerHTML = `<span class="icon">${icon}</span>`;
  name.append(entry.name);

  const size = document.createElement("td");
  size.className = "num";
  size.textContent = entry.isDir ? "" : formatSize(entry.size);
  if (entry.isDir) {
    sizeCells.set(entry.path, size);
    updateSizeCell(entry.path);
  }

  const modified = document.createElement("td");
  modified.textContent = formatDate(entry.modified);

  const perm = document.createElement("td");
  perm.className = "perm";
  perm.textContent = entry.permissions ?? "";
  if (entry.permissions) {
    perm.title = describePermissions(entry.permissions, entry.isDir);
    perm.classList.add("has-tip");
  }

  const actions = document.createElement("td");
  actions.className = "actions";
  if (!entry.isDir) actions.append(button("Download", () => downloadFile(entry)));
  actions.append(button("Umbenennen", () => renameEntry(entry)));
  actions.append(button("Löschen", () => deleteEntry(entry), "danger"));

  tr.append(name, size, modified, perm, actions);
  tr.addEventListener("dblclick", () => (entry.isDir ? navigate(entry.path) : downloadFile(entry)));
  rowRefs.set(entry.path, { tr, size });
  paintDownload(entry.path);
  return tr;
}

function ghostRow(t: Transfer): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.className = "ghost";
  tr.dataset.ghost = t.id;

  const name = document.createElement("td");
  name.className = "name";
  name.innerHTML = `<span class="icon">⬆</span>`;
  name.append(t.name);

  const size = document.createElement("td");
  size.className = "num";
  // Spans "Geändert" + "Rechte"; max-width 0 keeps changing text from resizing the table.
  const state = document.createElement("td");
  state.colSpan = 2;
  state.className = "ghost-state";

  const actions = document.createElement("td");
  actions.className = "actions";
  actions.append(button("Abbrechen", () => cancelTransfer(t), "danger"));

  tr.append(name, size, state, actions);
  const refs = { tr, size, state };
  ghostRefs.set(t.id, refs);
  updateGhost(t);
  return tr;
}

const runningDownload = (path: string) =>
  transfers.find((t) => t.kind === "download" && t.source === path && t.state === "running");

/** Progress of a download shown on the file/folder's own row – independent of the selection. */
function paintDownload(path: string) {
  const row = rowRefs.get(path);
  const t = runningDownload(path);
  if (!row || !t) return;
  const fraction = t.total > 0 ? Math.min(1, t.transferred / t.total) : 0;
  row.tr.classList.add("downloading");
  row.tr.style.setProperty("--p", `${(fraction * 100).toFixed(1)}%`);
  row.size.textContent = t.total > 0
    ? `${formatSize(t.transferred)} / ${formatSize(t.total)} (${Math.floor(fraction * 100)} %)`
    : `${formatSize(t.transferred)} …`;
  row.size.title = "Wird heruntergeladen";
}

function updateGhost(t: Transfer) {
  const g = ghostRefs.get(t.id);
  if (!g) return;
  const fraction = t.total > 0 ? Math.min(1, t.transferred / t.total) : 0;
  g.tr.style.setProperty("--p", `${(fraction * 100).toFixed(1)}%`);
  if (t.state === "queued") {
    g.size.textContent = t.total ? formatSize(t.total) : "";
    g.state.textContent = "Wartet auf Upload …";
    return;
  }
  g.size.textContent = `${formatSize(t.transferred)} / ${formatSize(t.total)} (${Math.floor(fraction * 100)} %)`;
  g.state.textContent = t.rateText || "Wird hochgeladen …";
}

// ---------- Transfers (queue, speed, ETA) ----------
const transfers: Transfer[] = [];
let transferCounter = 0;
let batchCounter = 0;
let pumping: Promise<void> | null = null;

function speedOf(t: Transfer): number {
  const first = t.samples[0], last = t.samples[t.samples.length - 1];
  if (!first || !last || last.t <= first.t) return 0;
  return ((last.b - first.b) / (last.t - first.t)) * 1000;
}

const RATE_REFRESH_MS = 2000;

/** Recomputes speed + remaining time at most every 2 s (called once per progress event). */
function refreshRate(t: Transfer) {
  const now = Date.now();
  if (t.rateText && now - t.rateAt < RATE_REFRESH_MS) return;
  const speed = speedOf(t);
  if (speed <= 0) return;
  t.rateText = `${formatSize(speed)}/s`;
  if (t.total > 0) t.rateText += ` · noch ${formatDuration((t.total - t.transferred) / speed)}`;
  t.rateAt = now;
}

function describe(t: Transfer): string {
  const queued = transfers.filter((x) => x.state === "queued").length;
  const verb = t.kind === "upload" ? "Upload" : "Download";
  const prep = t.kind === "upload" ? "nach" : "in";
  let text = `${verb} „${t.name}“ ${prep} ${t.targetDir}`;
  if (t.fileCount > 1) text += ` – Datei ${Math.max(1, t.fileIndex)}/${t.fileCount}: ${t.current}`;
  if (queued > 0) text += ` (+${queued} in Warteschlange)`;
  return text;
}

function renderFooter() {
  const running = transfers.find((t) => t.state === "running");
  if (!running) {
    statusEl.textContent = lastMessage;
    statusEl.title = lastMessage;
    statusEl.classList.toggle("error", lastMessageIsError);
    transferLineEl.hidden = true;
    return;
  }
  const text = describe(running);
  statusEl.textContent = text;
  statusEl.title = text;
  statusEl.classList.remove("error");
  transferLineEl.hidden = false;

  const parts: string[] = [];
  if (running.total > 0) {
    progressEl.value = (running.transferred / running.total) * 100;
    parts.push(`${Math.floor((running.transferred / running.total) * 100)} %`);
    parts.push(`${formatSize(running.transferred)} / ${formatSize(running.total)}`);
  } else {
    progressEl.removeAttribute("value"); // indeterminate
    parts.push(formatSize(running.transferred));
  }
  if (running.rateText) parts.push(running.rateText);
  transferDetailsEl.textContent = parts.join(" · ");
}

listen<Progress>("transfer-progress", ({ payload }) => {
  const t = transfers.find((x) => x.id === payload.id);
  if (!t) return;
  t.transferred = payload.transferred;
  t.total = payload.total;
  t.current = payload.current;
  t.fileIndex = payload.fileIndex;
  t.fileCount = payload.fileCount;
  const now = Date.now();
  t.samples.push({ t: now, b: payload.transferred });
  while (t.samples.length > 2 && now - t.samples[0].t > 5000) t.samples.shift(); // ~5 s window
  refreshRate(t);
  updateGhost(t);
  if (t.kind === "download") paintDownload(t.source);
  renderFooter();
});

function newTransfer(p: Pick<Transfer, "batch" | "kind" | "name" | "targetDir" | "remotePath" | "source">): Transfer {
  return {
    ...p, id: `t${++transferCounter}`, state: "queued", cancelRequested: false,
    transferred: 0, total: 0, current: p.name, fileIndex: 0, fileCount: 1, samples: [], rateText: "", rateAt: 0,
  };
}

function enqueue(items: Transfer[]) {
  for (const t of items) {
    log(t.kind === "upload" ? `Upload „${t.name}“ nach ${t.targetDir} gestartet` : `Download „${t.name}“ nach ${t.targetDir} gestartet`);
  }
  transfers.push(...items);
  renderFiles();
  renderFooter();
  if (!pumping) pumping = pump().finally(() => { pumping = null; });
}

async function pump() {
  for (;;) {
    const t = transfers.find((x) => x.state === "queued");
    if (!t) break;
    await runTransfer(t);
  }
}

async function runTransfer(t: Transfer) {
  t.state = "running";
  t.startedAt = Date.now();
  t.samples = [{ t: Date.now(), b: 0 }];
  renderFiles();
  renderFooter();
  try {
    let resume = false;
    for (;;) {
      try {
        if (t.kind === "upload") await invoke("upload", { id: t.id, local: t.source, remote: t.remotePath, resume });
        else await invoke("download", { id: t.id, remote: t.source, local: t.localPath, resume });
        break;
      } catch (e) {
        // A real connection loss is not an ordinary error: ask the user whether to continue.
        if (t.cancelRequested || (await invoke<boolean>("check_connection"))) throw e;
        if (!(await offerReconnect(t))) {
          t.partialKept = true;
          throw e;
        }
        resume = true;
        t.samples = [];
        t.rateText = "";
      }
    }
    setStatus(`${t.kind === "upload" ? "Upload" : "Download"} „${t.name}“ abgeschlossen`);
    const secs = (Date.now() - (t.startedAt ?? Date.now())) / 1000;
    log(`${t.kind === "upload" ? "Upload" : "Download"} „${t.name}“ abgeschlossen (${formatSize(t.transferred)} in ${formatDuration(secs)}${secs > 0 ? `, Ø ${formatSize(t.transferred / secs)}/s` : ""})`);
  } catch (e) {
    const verb = t.kind === "upload" ? "Upload" : "Download";
    if (t.cancelRequested) {
      setStatus(`${verb} „${t.name}“ abgebrochen – unvollständige Datei wurde entfernt`);
      log(`${verb} „${t.name}“ vom Benutzer abgebrochen, unvollständige Datei entfernt`);
    } else {
      const text = errorText(e);
      setStatus(`${verb} „${t.name}“ fehlgeschlagen: ${text}`, true);
      const where = t.kind === "upload" ? `auf dem Server (${t.remotePath})` : `lokal (${t.localPath})`;
      const note = t.partialKept ? `

Die unvollständige Datei bleibt ${where} liegen.` : "";
      showError(`${verb} „${t.name}“ fehlgeschlagen`, text + note);
      // Don't run the rest of the same batch into the same error.
      for (const other of transfers.filter((x) => x.batch === t.batch && x.state === "queued")) removeTransfer(other);
    }
  } finally {
    removeTransfer(t);
    renderFiles();
    renderFooter();
    if (t.kind === "upload" && t.targetDir === currentPath) await refresh();
  }
}

/** The connection dropped mid-transfer: ask, then reconnect (retrying on request). */
async function offerReconnect(t: Transfer): Promise<boolean> {
  const pct = t.total > 0 ? ` bei ${Math.floor((t.transferred / t.total) * 100)} %` : "";
  let question = `Die Verbindung zum Server wurde unterbrochen (${describe(t)}${pct}).

Neu verbinden und die Übertragung dort fortsetzen, wo sie stehen geblieben ist?`;
  for (;;) {
    const again = await ask(question, {
      title: "Verbindung unterbrochen",
      kind: "warning",
      okLabel: "Neu verbinden und fortsetzen",
      cancelLabel: "Nicht fortsetzen",
    });
    if (!again) {
      log(`Verbindung unterbrochen – Benutzer verzichtet auf das Fortsetzen von „${t.name}“`);
      return false;
    }
    setStatus("Verbinde neu …");
    log("Verbindung unterbrochen – Benutzer wählt „Neu verbinden und fortsetzen“");
    try {
      await invoke("reconnect");
      log(`Neu verbunden, „${t.name}“ wird fortgesetzt`);
      return true;
    } catch (e) {
      const text = errorText(e);
      setStatus(`Neu verbinden fehlgeschlagen: ${text}`, true);
      log(`Neu verbinden fehlgeschlagen: ${text}`, "error");
      question = `Neu verbinden ist fehlgeschlagen:
${text}

Erneut versuchen?`;
    }
  }
}

function removeTransfer(t: Transfer) {
  const i = transfers.indexOf(t);
  if (i >= 0) transfers.splice(i, 1);
}

async function cancelTransfer(t: Transfer) {
  t.cancelRequested = true;
  if (t.state === "queued") {
    removeTransfer(t);
    renderFiles();
    renderFooter();
  } else {
    await invoke("cancel_transfer", { id: t.id });
    setStatus(`Breche „${t.name}“ ab …`);
    log(`Benutzer bricht „${t.name}“ ab`);
  }
}

/** Cancels everything and waits until the backend has removed partial files. */
async function cancelAllTransfers() {
  for (const t of [...transfers]) await cancelTransfer(t);
  if (pumping) await Promise.race([pumping, new Promise((r) => setTimeout(r, 8000))]);
}

/** Asks before something is done that would abort running transfers. */
async function confirmAbortTransfers(action: string): Promise<boolean> {
  if (transfers.length === 0) return true;
  const running = transfers.find((t) => t.state === "running");
  const queued = transfers.filter((t) => t.state === "queued").length;
  let msg = "Es läuft noch eine Übertragung";
  if (running) {
    const pct = running.total > 0 ? ` (${Math.floor((running.transferred / running.total) * 100)} %)` : "";
    msg += `:\n${describe(running)}${pct}`;
  }
  if (queued > 0 && !running) msg += `: ${queued} Übertragung(en) in der Warteschlange`;
  msg += `\n\nWenn du ${action}, wird sie abgebrochen. Unvollständige Dateien werden gelöscht und müssen erneut übertragen werden.`;
  const abort = await ask(msg, {
    title: "Übertragung läuft",
    kind: "warning",
    okLabel: "Abbrechen und fortfahren",
    cancelLabel: "Weiter übertragen",
  });
  if (!abort) return false;
  await cancelAllTransfers();
  return true;
}

$("cancel-btn").addEventListener("click", () => {
  const running = transfers.find((t) => t.state === "running");
  if (running) cancelTransfer(running);
});

// ---------- Actions ----------
async function downloadFile(entry: Entry) {
  const local = await save({ title: "Speichern unter", defaultPath: entry.name });
  if (!local) return;
  const t = newTransfer({
    batch: ++batchCounter, kind: "download", name: entry.name,
    targetDir: localDirname(local), remotePath: "", source: entry.path,
  });
  t.localPath = local;
  t.total = entry.size;
  enqueue([t]);
}

const joinLocal = (dir: string, name: string) => (dir.endsWith("\\") || dir.endsWith("/") ? dir + name : `${dir}${dir.includes("\\") ? "\\" : "/"}${name}`);

/** Downloads several files/folders into one chosen local folder. */
async function downloadEntries(entries: Entry[]) {
  if (entries.length === 0) return;
  const title = entries.length === 1 ? `Zielordner für „${entries[0].name}“ wählen` : `Zielordner für ${entries.length} Elemente wählen`;
  const dir = await open({ title, directory: true, multiple: false });
  if (typeof dir !== "string") return;
  const batch = ++batchCounter;
  const items: Transfer[] = [];
  for (const entry of entries) {
    const local = joinLocal(dir, entry.name);
    if (await invoke<boolean>("local_exists", { path: local })) {
      const what = entry.isDir
        ? `Der Ordner „${entry.name}“ existiert im Zielordner bereits. Die Inhalte werden zusammengeführt, gleichnamige Dateien überschrieben.`
        : `„${entry.name}“ existiert im Zielordner bereits und wird überschrieben.`;
      if (!(await ask(`${what}\n\nFortfahren?`, { title: "Bereits vorhanden", kind: "warning", okLabel: "Fortfahren", cancelLabel: "Überspringen" }))) {
        log(`„${entry.name}“ übersprungen (existiert lokal bereits)`);
        continue;
      }
    }
    const t = newTransfer({ batch, kind: "download", name: entry.name, targetDir: dir, remotePath: "", source: entry.path });
    t.localPath = local;
    t.total = entry.isDir ? 0 : entry.size;
    items.push(t);
  }
  if (items.length > 0) enqueue(items);
}

async function uploadPaths(localPaths: string[]) {
  if (localPaths.length === 0) return;
  const batch = ++batchCounter;
  const items: Transfer[] = [];
  for (const local of localPaths) {
    const name = localBasename(local);
    const remotePath = joinRemote(currentPath, name);
    const clash = lastEntries.find((e) => e.name === name) ?? null;
    const queuedClash = transfers.some((t) => t.remotePath === remotePath && t.kind === "upload");
    if (clash || queuedClash) {
      const what = clash?.isDir
        ? `Der Ordner „${name}“ existiert bereits. Die Inhalte werden zusammengeführt, gleichnamige Dateien überschrieben.`
        : `„${name}“ existiert bereits und wird überschrieben.`;
      if (!(await ask(`${what}\n\nFortfahren?`, { title: "Bereits vorhanden", kind: "warning", okLabel: "Fortfahren", cancelLabel: "Überspringen" }))) {
        log(`„${name}“ übersprungen (existiert bereits)`);
        continue;
      }
    }
    items.push(newTransfer({ batch, kind: "upload", name, targetDir: currentPath, remotePath, source: local }));
  }
  if (items.length > 0) enqueue(items);
}

async function renameEntry(entry: Entry) {
  const newName = await promptDialog(`"${entry.name}" umbenennen in:`, entry.name);
  if (!newName || newName === entry.name) return;
  const to = newName.startsWith("/") ? newName : joinRemote(currentPath, newName);
  await run("Umbenennen", () => invoke("rename", { from: entry.path, to }), `Umbenannt: ${entry.path} → ${to}`);
  await refresh();
}

async function deleteEntry(entry: Entry) {
  const msg = entry.isDir
    ? `Ordner "${entry.name}" inklusive aller Inhalte löschen?`
    : `Datei "${entry.name}" löschen?`;
  if (!(await ask(msg, { title: "Löschen bestätigen", kind: "warning" }))) return;
  await run(`Lösche ${entry.name}`, () => invoke("remove", { path: entry.path, isDir: entry.isDir }), `Gelöscht: ${entry.path}`);
  await refresh();
}

async function makeDir() {
  const name = await promptDialog("Name des neuen Ordners:");
  if (!name) return;
  const dir = joinRemote(currentPath, name);
  await run("Ordner anlegen", () => invoke("mkdir", { path: dir }), `Ordner angelegt: ${dir}`);
  await refresh();
}

async function pickFilesToUpload() {
  const files = await open({ title: "Dateien hochladen", multiple: true, directory: false });
  if (files) await uploadPaths(Array.isArray(files) ? files : [files]);
}

async function pickFolderToUpload() {
  const dirs = await open({ title: "Ordner hochladen", multiple: true, directory: true });
  if (dirs) await uploadPaths(Array.isArray(dirs) ? dirs : [dirs]);
}

async function disconnect() {
  if (!(await confirmAbortTransfers("die Verbindung trennst"))) return;
  await invoke("disconnect").catch(() => {});
  log("Verbindung getrennt");
  renderHistory();
  resetFolderSizes(true);
  browserView.hidden = true;
  loginView.hidden = false;
  $("files").replaceChildren();
  lastEntries = [];
  setStatus("");
}

$("up-btn").addEventListener("click", () => navigate(joinRemote(currentPath, "..")));
$("refresh-btn").addEventListener("click", refresh);
$("path-form").addEventListener("submit", (e) => { e.preventDefault(); navigate(input("path").value); });
$("mkdir-btn").addEventListener("click", makeDir);
$("upload-btn").addEventListener("click", pickFilesToUpload);
$("upload-dir-btn").addEventListener("click", pickFolderToUpload);
$("disconnect-btn").addEventListener("click", disconnect);
$("log-btn").addEventListener("click", () => toggleLog());
$("log-close").addEventListener("click", () => toggleLog(false));
$("log-clear").addEventListener("click", () => logListEl.replaceChildren());

// ---------- Context menu ----------
// The WebView's built-in menu has "Reload", which dropped the user back to the login screen.
type MenuItem = { label: string; action: () => void; danger?: boolean } | "sep";
const ctxMenu = $("ctx-menu");

function hideContextMenu() {
  ctxMenu.hidden = true;
}

function showContextMenu(x: number, y: number, items: MenuItem[]) {
  ctxMenu.replaceChildren();
  for (const item of items) {
    if (item === "sep") { ctxMenu.append(document.createElement("hr")); continue; }
    const b = document.createElement("button");
    b.textContent = item.label;
    if (item.danger) b.className = "danger";
    b.addEventListener("click", () => { hideContextMenu(); item.action(); });
    ctxMenu.append(b);
  }
  ctxMenu.hidden = false;
  const { width, height } = ctxMenu.getBoundingClientRect();
  ctxMenu.style.left = `${Math.max(0, Math.min(x, window.innerWidth - width - 4))}px`;
  ctxMenu.style.top = `${Math.max(0, Math.min(y, window.innerHeight - height - 4))}px`;
}

document.addEventListener("contextmenu", (e) => {
  const target = e.target as HTMLElement;
  if (target instanceof HTMLInputElement) return; // keep copy/paste in text fields
  e.preventDefault();
  hideContextMenu();
  if (browserView.hidden || !target.closest("#drop-zone")) return;

  const general: MenuItem[] = [
    { label: "Aktualisieren", action: () => void refresh() },
    { label: "Neuer Ordner", action: () => void makeDir() },
    { label: "Dateien hochladen …", action: () => void pickFilesToUpload() },
    { label: "Ordner hochladen …", action: () => void pickFolderToUpload() },
  ];
  const tr = target.closest("tr");
  const ghost = tr?.dataset.ghost ? transfers.find((t) => t.id === tr.dataset.ghost) : undefined;
  const entry = tr?.dataset.path ? lastEntries.find((en) => en.path === tr.dataset.path) : undefined;
  let items = general;
  if (ghost) {
    items = [{ label: "Upload abbrechen", danger: true, action: () => void cancelTransfer(ghost) }, "sep", ...general];
  } else if (entry && selected.has(entry.path) && selected.size > 1) {
    // Several items: only actions that make sense for all of them.
    const chosen = selectedEntries();
    items = [
      { label: `Download (${chosen.length} Elemente) …`, action: () => void downloadEntries(chosen) },
      "sep",
      ...general,
    ];
  } else if (entry) {
    // Right-click on an unselected row selects just that row (like the file explorer).
    if (!selected.has(entry.path)) selectRow(entry.path, { ctrlKey: false, metaKey: false, shiftKey: false });
    items = [
      entry.isDir
        ? { label: "Öffnen", action: () => void navigate(entry.path) }
        : { label: "Download …", action: () => void downloadFile(entry) },
      ...(entry.isDir ? [{ label: "Ordner herunterladen …", action: () => void downloadEntries([entry]) }] : []),
      ...(entry.isDir
        ? [
            sizeJobs.get(entry.path)?.state === "running"
              ? { label: "Berechnung abbrechen", action: () => cancelFolderSize(entry.path) }
              : { label: "Größe berechnen", action: () => void calculateFolderSize(entry) },
          ]
        : []),
      { label: "Umbenennen …", action: () => void renameEntry(entry) },
      { label: "Löschen …", danger: true, action: () => void deleteEntry(entry) },
      "sep",
      ...general,
    ];
  }
  showContextMenu(e.clientX, e.clientY, items);
});

document.addEventListener("mousedown", (e) => { if (!ctxMenu.contains(e.target as Node)) hideContextMenu(); });
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") hideContextMenu();
  const typing = e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement;
  if (!browserView.hidden && !typing && (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a") {
    e.preventDefault();
    for (const en of lastEntries) selected.add(en.path);
    paintSelection();
  }
  // Reload shortcuts would also throw the user back to the login screen (and lose running transfers).
  if (e.key === "F5" || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "r")) {
    e.preventDefault();
    if (!browserView.hidden) void refresh();
  }
});
window.addEventListener("blur", hideContextMenu);
$("drop-zone").addEventListener("scroll", hideContextMenu);
$("drop-zone").addEventListener("click", (e) => {
  if (!(e.target as HTMLElement).closest("tr[data-path]")) clearSelection();
});

// ---------- Drag & drop from the file explorer (files and folders) ----------
const overlay = $("drop-overlay");
getCurrentWebview().onDragDropEvent(({ payload }) => {
  if (browserView.hidden) return;
  if (payload.type === "enter" || payload.type === "over") overlay.hidden = false;
  else if (payload.type === "leave") overlay.hidden = true;
  else if (payload.type === "drop") {
    overlay.hidden = true;
    void uploadPaths(payload.paths);
  }
});

// ---------- Closing the window ----------
let closing = false;
getCurrentWindow().onCloseRequested(async (event) => {
  if (closing || transfers.length === 0) return; // nothing to protect
  event.preventDefault();
  closing = true;
  try {
    if (await confirmAbortTransfers("das Programm beendest")) await getCurrentWindow().destroy();
  } finally {
    closing = false;
  }
});

loadProfile();
