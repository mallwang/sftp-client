//! SFTP backend: SSH connection (password or key), host key check (TOFU),
//! and the SFTP operations exposed to the frontend as Tauri commands.

use std::collections::HashMap;
use std::future::Future;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use russh::client::{self, KeyboardInteractiveAuthResponse};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::Disconnect;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

type CmdResult<T> = Result<T, String>;

/// Converts technical (English) errors into understandable German messages.
fn err<E: std::fmt::Display>(e: E) -> String {
    let raw = e.to_string();
    let lower = raw.to_lowercase();
    let friendly = if lower.contains("permission denied") {
        "Zugriff verweigert – dem Benutzer fehlt die Berechtigung für diese Aktion"
    } else if lower.contains("no such file") || lower.contains("not found") {
        "Datei oder Ordner wurde nicht gefunden"
    } else if lower.contains("connection lost")
        || lower.contains("no connection")
        || lower.contains("unexpected eof")
        || lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("channel")
    {
        "Die Verbindung zum Server wurde unterbrochen"
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "Zeitüberschreitung – der Server antwortet nicht"
    } else if lower.starts_with("failure") {
        "Der Server hat die Aktion abgelehnt (z. B. Name existiert bereits, Speicher voll oder Kontingent erreicht)"
    } else if lower.contains("operation unsupported") {
        "Der Server unterstützt diese Aktion nicht"
    } else {
        return raw;
    };
    format!("{friendly} ({raw})")
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct AppState {
    conn: Mutex<Option<Connection>>,
    /// Credentials of the current session (memory only), used for reconnecting
    login: Mutex<Option<LoginParams>>,
    /// Cancel flags of running transfers by transfer id
    cancels: std::sync::Mutex<HashMap<String, Arc<AtomicBool>>>,
}

struct Connection {
    handle: client::Handle<HostKeyChecker>,
    sftp: Arc<SftpSession>,
}

/// Returns the active SFTP session without holding the lock for the whole
/// operation, so a long transfer doesn't block directory listings.
async fn session(state: &State<'_, AppState>) -> CmdResult<Arc<SftpSession>> {
    state
        .conn
        .lock()
        .await
        .as_ref()
        .map(|c| c.sftp.clone())
        .ok_or_else(|| "Nicht verbunden".to_string())
}

// ---------------------------------------------------------------------------
// Host key check (trust on first use, stored in the app data directory)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct HostKeyReport {
    fingerprint: String,
    new_host: bool,
    error: Option<String>,
}

struct HostKeyChecker {
    host_id: String,
    known_hosts: PathBuf,
    report: Arc<std::sync::Mutex<HostKeyReport>>,
}

impl client::Handler for HostKeyChecker {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = server_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        let mut report = self.report.lock().unwrap();
        report.fingerprint = fingerprint.clone();

        match check_known_host(&self.known_hosts, &self.host_id, &fingerprint) {
            Ok(KnownHost::Match) => Ok(true),
            Ok(KnownHost::New) => {
                report.new_host = true;
                Ok(true)
            }
            Ok(KnownHost::Mismatch(expected)) => {
                report.error = Some(format!(
                    "WARNUNG: Der Host-Key von {} hat sich geändert!\n\
                     Erwartet: {expected}\nErhalten: {fingerprint}\n\
                     Falls die Änderung legitim ist, entferne den Eintrag aus {}",
                    self.host_id,
                    self.known_hosts.display()
                ));
                Ok(false)
            }
            Err(e) => {
                report.error = Some(format!("known_hosts konnte nicht gelesen werden: {e}"));
                Ok(false)
            }
        }
    }
}

enum KnownHost {
    Match,
    New,
    Mismatch(String),
}

/// File format: one `host_id fingerprint` entry per line.
fn check_known_host(file: &Path, host_id: &str, fingerprint: &str) -> std::io::Result<KnownHost> {
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    for line in content.lines() {
        if let Some((host, fp)) = line.trim().split_once(' ') {
            if host == host_id {
                return Ok(if fp == fingerprint {
                    KnownHost::Match
                } else {
                    KnownHost::Mismatch(fp.to_string())
                });
            }
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut content = content;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!("{host_id} {fingerprint}\n"));
    std::fs::write(file, content)?;
    Ok(KnownHost::New)
}

// ---------------------------------------------------------------------------
// Connect / disconnect
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum Auth {
    Password {
        password: String,
    },
    Key {
        key_path: String,
        passphrase: Option<String>,
    },
}

#[derive(Clone)]
pub struct LoginParams {
    host: String,
    port: u16,
    username: String,
    auth: Auth,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectInfo {
    home: String,
    fingerprint: String,
    new_host_key: bool,
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[tauri::command]
pub async fn connect(
    app: AppHandle,
    state: State<'_, AppState>,
    host: String,
    port: u16,
    username: String,
    auth: Auth,
) -> CmdResult<ConnectInfo> {
    let params = LoginParams { host, port, username, auth };
    let info = establish(&app, &state, &params).await?;
    // Kept in memory only (never written to disk) so a dropped connection can be re-established.
    *state.login.lock().await = Some(params);
    Ok(info)
}

/// Re-establishes the connection with the credentials of the last successful login.
#[tauri::command]
pub async fn reconnect(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let params = state
        .login
        .lock()
        .await
        .clone()
        .ok_or_else(|| "Keine Zugangsdaten im Speicher – bitte neu anmelden".to_string())?;
    establish(&app, &state, &params).await.map(|_| ())
}

/// Checks whether the SSH/SFTP connection still answers.
#[tauri::command]
pub async fn check_connection(state: State<'_, AppState>) -> CmdResult<bool> {
    let Ok(sftp) = session(&state).await else { return Ok(false) };
    Ok(matches!(
        tokio::time::timeout(Duration::from_secs(5), sftp.canonicalize(".")).await,
        Ok(Ok(_))
    ))
}

async fn establish(
    app: &AppHandle,
    state: &State<'_, AppState>,
    params: &LoginParams,
) -> CmdResult<ConnectInfo> {
    let LoginParams { host, port, username, auth } = params.clone();
    close_connection(state).await;

    let host = host.trim().to_string();
    let known_hosts = app.path().app_data_dir().map_err(err)?.join("known_hosts");
    let report = Arc::new(std::sync::Mutex::new(HostKeyReport::default()));
    let handler = HostKeyChecker {
        host_id: format!("[{host}]:{port}"),
        known_hosts,
        report: report.clone(),
    };

    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        nodelay: true,
        ..Default::default()
    });

    let connecting = client::connect(config, (host.as_str(), port), handler);
    let mut handle = match tokio::time::timeout(Duration::from_secs(15), connecting).await {
        Err(_) => return Err(format!("Zeitüberschreitung beim Verbinden mit {host}:{port}")),
        Ok(Err(e)) => {
            let hostkey_error = report.lock().unwrap().error.clone();
            return Err(hostkey_error.unwrap_or_else(|| format!("Verbindung fehlgeschlagen: {e}")));
        }
        Ok(Ok(handle)) => handle,
    };

    let authenticated = match auth {
        Auth::Password { password } => {
            let ok = handle
                .authenticate_password(username.clone(), password.clone())
                .await
                .map_err(err)?
                .success();
            // Many servers only allow passwords via "keyboard-interactive".
            ok || keyboard_interactive(&mut handle, &username, &password).await?
        }
        Auth::Key { key_path, passphrase } => {
            let passphrase = passphrase.filter(|p| !p.is_empty());
            let key = keys::load_secret_key(expand_tilde(key_path.trim()), passphrase.as_deref())
                .map_err(|e| format!("Schlüssel konnte nicht geladen werden: {e}"))?;
            let rsa_hash = handle.best_supported_rsa_hash().await.map_err(err)?.flatten();
            handle
                .authenticate_publickey(
                    username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
                )
                .await
                .map_err(err)?
                .success()
        }
    };
    if !authenticated {
        let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
        return Err("Anmeldung fehlgeschlagen (Benutzername, Passwort oder Schlüssel falsch)".into());
    }

    let channel = handle.channel_open_session().await.map_err(err)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("SFTP-Subsystem nicht verfügbar: {e}"))?;
    let sftp = SftpSession::new(channel.into_stream()).await.map_err(err)?;
    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into());

    let (fingerprint, new_host_key) = {
        let r = report.lock().unwrap();
        (r.fingerprint.clone(), r.new_host)
    };
    *state.conn.lock().await = Some(Connection {
        handle,
        sftp: Arc::new(sftp),
    });
    Ok(ConnectInfo {
        home,
        fingerprint,
        new_host_key,
    })
}

async fn keyboard_interactive(
    handle: &mut client::Handle<HostKeyChecker>,
    username: &str,
    password: &str,
) -> CmdResult<bool> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(username, None::<String>)
        .await
        .map_err(err)?;
    // Answer every prompt with the password, max. a few rounds.
    for _ in 0..5 {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_string()).collect();
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(err)?;
            }
        }
    }
    Ok(false)
}

async fn close_connection(state: &State<'_, AppState>) {
    let conn = state.conn.lock().await.take();
    if let Some(conn) = conn {
        let _ = conn.sftp.close().await;
        let _ = conn
            .handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
    }
}

#[tauri::command]
pub async fn disconnect(state: State<'_, AppState>) -> CmdResult<()> {
    *state.login.lock().await = None;
    close_connection(&state).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Directory operations
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    name: String,
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    /// Unix timestamp in seconds
    modified: Option<u64>,
    permissions: Option<String>,
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn mode_string(mode: u32, is_dir: bool, is_symlink: bool) -> String {
    let kind = if is_symlink { 'l' } else if is_dir { 'd' } else { '-' };
    let bits = ['r', 'w', 'x'];
    let mut s = String::from(kind);
    for i in (0..9).rev() {
        s.push(if mode & (1 << i) != 0 { bits[(8 - i) % 3] } else { '-' });
    }
    s
}

/// Normalizes a path on the server (e.g. `..`, `~`-less relative paths).
#[tauri::command]
pub async fn resolve_path(state: State<'_, AppState>, path: String) -> CmdResult<String> {
    let sftp = session(&state).await?;
    sftp.canonicalize(path).await.map_err(err)
}

#[tauri::command]
pub async fn list_dir(state: State<'_, AppState>, path: String) -> CmdResult<Vec<Entry>> {
    let sftp = session(&state).await?;
    let mut entries = Vec::new();
    for entry in sftp.read_dir(path.clone()).await.map_err(err)? {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let full = join_remote(&path, &name);
        let meta = entry.metadata();
        let is_symlink = meta.is_symlink();
        // For symlinks, follow the link to find out whether it points to a directory.
        let is_dir = if is_symlink {
            sftp.metadata(full.clone()).await.map(|m| m.is_dir()).unwrap_or(false)
        } else {
            meta.is_dir()
        };
        entries.push(Entry {
            permissions: meta.permissions.map(|p| mode_string(p, is_dir, is_symlink)),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            size: meta.size.unwrap_or(0),
            name,
            path: full,
            is_dir,
            is_symlink,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

#[tauri::command]
pub async fn mkdir(state: State<'_, AppState>, path: String) -> CmdResult<()> {
    session(&state).await?.create_dir(path).await.map_err(err)
}

#[tauri::command]
pub async fn rename(state: State<'_, AppState>, from: String, to: String) -> CmdResult<()> {
    session(&state).await?.rename(from, to).await.map_err(err)
}

#[tauri::command]
pub async fn remove(state: State<'_, AppState>, path: String, is_dir: bool) -> CmdResult<()> {
    let sftp = session(&state).await?;
    if is_dir {
        remove_dir_recursive(&sftp, path).await
    } else {
        sftp.remove_file(path).await.map_err(err)
    }
}

fn remove_dir_recursive(
    sftp: &SftpSession,
    path: String,
) -> Pin<Box<dyn Future<Output = CmdResult<()>> + Send + '_>> {
    Box::pin(async move {
        for entry in sftp.read_dir(path.clone()).await.map_err(err)? {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let full = join_remote(&path, &name);
            // Don't follow symlinks: only delete the link itself.
            if entry.metadata().is_dir() {
                remove_dir_recursive(sftp, full).await?;
            } else {
                sftp.remove_file(full).await.map_err(err)?;
            }
        }
        sftp.remove_dir(path).await.map_err(err)
    })
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

const CANCELLED: &str = "Abgebrochen";

/// When resuming, re-send the last MiB: the server may not have stored everything it acknowledged.
const RESUME_OVERLAP: u64 = 1024 * 1024;

/// True for errors that come from a dead connection (partial files are then kept for resuming).
fn is_connection_lost(e: &str) -> bool {
    e.contains("Verbindung zum Server wurde unterbrochen") || e.contains("Zeitüberschreitung")
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    id: String,
    transferred: u64,
    total: u64,
    /// File currently being transferred (relevant for folder uploads)
    current: String,
    file_index: u32,
    file_count: u32,
}

/// Progress/cancel context of one transfer (a single file or a whole folder).
struct Ctx {
    app: AppHandle,
    id: String,
    cancel: Arc<AtomicBool>,
    total: u64,
    done: u64,
    current: String,
    file_index: u32,
    file_count: u32,
    last_emit: Instant,
}

impl Ctx {
    fn new(app: AppHandle, id: &str, cancel: Arc<AtomicBool>, current: String) -> Self {
        Ctx {
            app,
            id: id.to_string(),
            cancel,
            total: 0,
            done: 0,
            current,
            file_index: 1,
            file_count: 1,
            last_emit: Instant::now(),
        }
    }

    fn emit(&mut self) {
        self.last_emit = Instant::now();
        let _ = self.app.emit(
            "transfer-progress",
            Progress {
                id: self.id.clone(),
                transferred: self.done,
                total: self.total.max(self.done),
                current: self.current.clone(),
                file_index: self.file_index,
                file_count: self.file_count,
            },
        );
    }

    fn check_cancel(&self) -> CmdResult<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(CANCELLED.to_string())
        } else {
            Ok(())
        }
    }

    /// Counts bytes that don't need to be sent again (resume).
    fn skip(&mut self, n: u64) {
        self.done += n;
    }

    fn start_file(&mut self, name: String, index: u32) {
        self.current = name;
        self.file_index = index;
        self.emit();
    }

    fn advance(&mut self, n: u64) {
        self.done += n;
        if self.last_emit.elapsed() >= Duration::from_millis(100) {
            self.emit();
        }
    }
}

async fn copy_file<R, W>(ctx: &mut Ctx, src: &mut R, dst: &mut W) -> CmdResult<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        ctx.check_cancel()?;
        let n = src.read(&mut buf).await.map_err(err)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await.map_err(err)?;
        ctx.advance(n as u64);
    }
    ctx.check_cancel()?;
    dst.flush().await.map_err(err)?;
    dst.shutdown().await.map_err(err)?;
    ctx.emit();
    Ok(())
}

fn register_transfer(state: &State<'_, AppState>, id: &str) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    state.cancels.lock().unwrap().insert(id.to_string(), flag.clone());
    flag
}

fn unregister_transfer(state: &State<'_, AppState>, id: &str) {
    state.cancels.lock().unwrap().remove(id);
}

/// Aborts a running transfer; partial files are removed by the transfer itself.
#[tauri::command]
pub fn cancel_transfer(state: State<'_, AppState>, id: String) {
    if let Some(flag) = state.cancels.lock().unwrap().get(&id) {
        flag.store(true, Ordering::Relaxed);
    }
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

struct DownloadPlan {
    /// Local directories to create (parents before children)
    dirs: Vec<PathBuf>,
    /// (remote file, local file, size)
    files: Vec<(String, PathBuf, u64)>,
}

/// Names that must never be turned into a local path component.
fn is_safe_name(name: &str) -> bool {
    !(name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\'))
}

/// Walks a remote folder. Symbolic links are skipped.
async fn scan_remote(
    sftp: &SftpSession,
    remote_root: &str,
    local_root: &Path,
    cancel: &AtomicBool,
) -> CmdResult<DownloadPlan> {
    let mut plan = DownloadPlan { dirs: vec![local_root.to_path_buf()], files: Vec::new() };
    let mut stack = vec![(remote_root.to_string(), local_root.to_path_buf())];
    while let Some((remote_dir, local_dir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        for entry in sftp.read_dir(remote_dir.clone()).await.map_err(err)? {
            let name = entry.file_name();
            if !is_safe_name(&name) {
                continue;
            }
            let meta = entry.metadata();
            if meta.is_symlink() {
                continue;
            }
            let remote = join_remote(&remote_dir, &name);
            let local = local_dir.join(&name);
            if meta.is_dir() {
                plan.dirs.push(local.clone());
                stack.push((remote, local));
            } else {
                plan.files.push((remote, local, meta.size.unwrap_or(0)));
            }
        }
    }
    Ok(plan)
}

async fn download_file(
    sftp: &SftpSession,
    ctx: &mut Ctx,
    remote: &str,
    local: &Path,
    size: u64,
    resume: bool,
) -> CmdResult<()> {
    // Resume: continue after what is already on disk.
    let mut start = 0;
    if resume {
        if let Ok(m) = tokio::fs::metadata(local).await {
            if m.len() == size {
                ctx.skip(size); // complete already
                ctx.emit();
                return Ok(());
            }
            if m.len() < size {
                start = m.len().saturating_sub(RESUME_OVERLAP);
            }
        }
    }
    let mut src = sftp.open(remote.to_string()).await.map_err(err)?;
    let mut dst = if start > 0 {
        let mut f = tokio::fs::OpenOptions::new().write(true).open(local).await.map_err(err)?;
        f.seek(SeekFrom::Start(start)).await.map_err(err)?;
        src.seek(SeekFrom::Start(start)).await.map_err(err)?;
        ctx.skip(start);
        f
    } else {
        tokio::fs::File::create(local).await.map_err(err)?
    };
    let res = copy_file(ctx, &mut src, &mut dst).await;
    if matches!(&res, Err(e) if !is_connection_lost(e)) {
        drop(dst);
        let _ = tokio::fs::remove_file(local).await; // don't leave half files behind
    }
    res
}

/// Downloads a file or (recursively) a folder to the full local path `local`.
#[tauri::command]
pub async fn download(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    remote: String,
    local: String,
    resume: Option<bool>,
) -> CmdResult<()> {
    let resume = resume.unwrap_or(false);
    let sftp = session(&state).await?;
    let cancel = register_transfer(&state, &id);
    let result = async {
        let meta = sftp.metadata(remote.clone()).await.map_err(err)?;
        let local = PathBuf::from(&local);
        let name = remote.trim_end_matches('/').rsplit('/').next().unwrap_or(&remote).to_string();
        let mut ctx = Ctx::new(app, &id, cancel.clone(), name);
        if meta.is_dir() {
            let plan = scan_remote(&sftp, &remote, &local, &cancel).await?;
            ctx.total = plan.files.iter().map(|f| f.2).sum();
            ctx.file_count = plan.files.len() as u32;
            ctx.file_index = 0;
            ctx.emit();
            for dir in &plan.dirs {
                ctx.check_cancel()?;
                tokio::fs::create_dir_all(dir).await.map_err(err)?;
            }
            for (i, (remote_file, local_file, size)) in plan.files.iter().enumerate() {
                ctx.check_cancel()?;
                ctx.start_file(file_name_of(local_file), i as u32 + 1);
                download_file(&sftp, &mut ctx, remote_file, local_file, *size, resume).await?;
            }
            ctx.emit();
            Ok(())
        } else {
            ctx.total = meta.len();
            ctx.emit();
            download_file(&sftp, &mut ctx, &remote, &local, meta.len(), resume).await
        }
    }
    .await;
    unregister_transfer(&state, &id);
    result
}

/// Whether a local file/folder already exists (to ask before overwriting).
#[tauri::command]
pub async fn local_exists(path: String) -> bool {
    tokio::fs::try_exists(&path).await.unwrap_or(false)
}

struct UploadPlan {
    /// Remote directories to create (parents before children)
    dirs: Vec<String>,
    /// (local file, remote file, size)
    files: Vec<(PathBuf, String, u64)>,
}

/// Walks a local folder. Symbolic links are skipped.
fn scan_local(root: &Path, remote_root: &str) -> std::io::Result<UploadPlan> {
    let mut plan = UploadPlan { dirs: vec![remote_root.to_string()], files: Vec::new() };
    let mut stack = vec![(root.to_path_buf(), remote_root.to_string())];
    while let Some((dir, remote_dir)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let remote = join_remote(&remote_dir, &entry.file_name().to_string_lossy());
            if kind.is_dir() {
                plan.dirs.push(remote.clone());
                stack.push((entry.path(), remote));
            } else if kind.is_file() {
                plan.files.push((entry.path(), remote, entry.metadata()?.len()));
            }
        }
    }
    Ok(plan)
}

async fn ensure_remote_dir(sftp: &SftpSession, path: &str) -> CmdResult<()> {
    match sftp.create_dir(path.to_string()).await {
        Ok(()) => Ok(()),
        // Already existing folders are fine (merge).
        Err(e) => match sftp.metadata(path.to_string()).await {
            Ok(m) if m.is_dir() => Ok(()),
            _ => Err(err(e)),
        },
    }
}

async fn upload_file(
    sftp: &SftpSession,
    ctx: &mut Ctx,
    local: &Path,
    remote: &str,
    resume: bool,
) -> CmdResult<()> {
    let mut src = tokio::fs::File::open(local).await.map_err(err)?;
    let local_len = src.metadata().await.map_err(err)?.len();
    // Resume: continue behind what the server already has.
    let mut start = 0;
    if resume {
        if let Ok(m) = sftp.metadata(remote.to_string()).await {
            let remote_len = m.len();
            if remote_len == local_len {
                ctx.skip(local_len); // complete already
                ctx.emit();
                return Ok(());
            }
            if remote_len < local_len {
                start = remote_len.saturating_sub(RESUME_OVERLAP);
            }
        }
    }
    let mut dst = if start > 0 {
        let mut f = sftp
            .open_with_flags(remote.to_string(), OpenFlags::WRITE)
            .await
            .map_err(err)?;
        f.seek(SeekFrom::Start(start)).await.map_err(err)?;
        src.seek(SeekFrom::Start(start)).await.map_err(err)?;
        ctx.skip(start);
        f
    } else {
        sftp.create(remote.to_string()).await.map_err(err)?
    };
    let res = copy_file(ctx, &mut src, &mut dst).await;
    // With a dead connection keep the partial file so the user can resume.
    if matches!(&res, Err(e) if !is_connection_lost(e)) {
        // Remove the incomplete file on the server (with a timeout in case the connection is dead).
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = dst.shutdown().await;
            let _ = sftp.remove_file(remote.to_string()).await;
        })
        .await;
    }
    res
}

/// Uploads a file or (recursively) a folder to the full remote path `remote`.
#[tauri::command]
pub async fn upload(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    local: String,
    remote: String,
    resume: Option<bool>,
) -> CmdResult<()> {
    let resume = resume.unwrap_or(false);
    let sftp = session(&state).await?;
    let cancel = register_transfer(&state, &id);
    let result = async {
        let local = PathBuf::from(&local);
        let meta = tokio::fs::metadata(&local).await.map_err(err)?;
        let mut ctx = Ctx::new(app, &id, cancel, file_name_of(&local));
        if meta.is_dir() {
            let (root, remote_root) = (local.clone(), remote.clone());
            let plan = tokio::task::spawn_blocking(move || scan_local(&root, &remote_root))
                .await
                .map_err(err)?
                .map_err(|e| format!("Ordner konnte nicht gelesen werden: {e}"))?;
            ctx.total = plan.files.iter().map(|f| f.2).sum();
            ctx.file_count = plan.files.len() as u32;
            ctx.file_index = 0;
            ctx.emit();
            for dir in &plan.dirs {
                ctx.check_cancel()?;
                ensure_remote_dir(&sftp, dir).await?;
            }
            for (i, (path, remote_file, _)) in plan.files.iter().enumerate() {
                ctx.check_cancel()?;
                ctx.start_file(file_name_of(path), i as u32 + 1);
                upload_file(&sftp, &mut ctx, path, remote_file, resume).await?;
            }
            ctx.emit();
            Ok(())
        } else {
            ctx.total = meta.len();
            ctx.emit();
            upload_file(&sftp, &mut ctx, &local, &remote, resume).await
        }
    }
    .await;
    unregister_transfer(&state, &id);
    result
}

// ---------------------------------------------------------------------------
// Folder size (SFTP has no such call: walk the tree and add up the files)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSize {
    size: u64,
    files: u64,
    folders: u64,
    /// Sub-folders that could not be read (e.g. missing permission) and are not counted
    skipped: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SizeProgress {
    id: String,
    size: u64,
    files: u64,
}

/// Adds up the size of a folder recursively. Symlinks are not followed.
/// Can be aborted with `cancel_transfer` using the same id.
#[tauri::command]
pub async fn folder_size(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    path: String,
) -> CmdResult<FolderSize> {
    let sftp = session(&state).await?;
    let cancel = register_transfer(&state, &id);
    let result = async {
        let mut total = FolderSize { size: 0, files: 0, folders: 0, skipped: 0 };
        let mut stack = vec![path.clone()];
        let mut last_emit = Instant::now();
        while let Some(dir) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            let entries = match sftp.read_dir(dir.clone()).await {
                Ok(entries) => entries,
                Err(e) if dir == path => return Err(err(e)),
                Err(_) => {
                    total.skipped += 1;
                    continue;
                }
            };
            for entry in entries {
                let name = entry.file_name();
                if name == "." || name == ".." {
                    continue;
                }
                let meta = entry.metadata();
                if meta.is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    total.folders += 1;
                    stack.push(join_remote(&dir, &name));
                } else {
                    total.files += 1;
                    total.size += meta.size.unwrap_or(0);
                }
            }
            if last_emit.elapsed() >= Duration::from_millis(150) {
                last_emit = Instant::now();
                let _ = app.emit(
                    "size-progress",
                    SizeProgress { id: id.clone(), size: total.size, files: total.files },
                );
            }
        }
        Ok(total)
    }
    .await;
    unregister_transfer(&state, &id);
    result
}
