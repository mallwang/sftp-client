//! SSH-Agent-Anbindung (Bitwarden, OpenSSH-Agent, ssh-agent, Pageant).
//!
//! Der private Schlüssel verlässt den Agenten nie: wir bekommen nur die öffentlichen
//! Schlüssel und lassen die Signatur vom Agenten erzeugen. Dieses Modul hängt bewusst
//! nicht von Tauri ab.

use std::time::Duration;

use russh::client;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{Algorithm, HashAlg};
use serde::Serialize;

/// Verbindung zum Agenten, unabhängig vom Transport (Unix-Socket, Named Pipe, Pageant).
pub type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// Höchstens so viele Schlüssel pro Anmeldung probieren (sonst sperrt der Server
/// wegen zu vieler Fehlversuche).
const MAX_KEYS: usize = 5;
/// Verbindungsaufbau zum Agenten.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Der Bitwarden-Agent zeigt pro Anmeldung ein Freigabe-Fenster an, das dauert.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

pub const NOT_FOUND: &str = "SSH-Agent nicht gefunden. Läuft Bitwarden und ist der SSH-Agent \
     aktiviert (Einstellungen > SSH-Agent)?";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentKey {
    pub fingerprint: String,
    pub comment: String,
    pub algorithm: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub available: bool,
    /// Woher der Agent kommt (z. B. "SSH_AUTH_SOCK"), leer wenn nicht gefunden
    pub source: String,
}

enum Target {
    #[cfg(unix)]
    Socket(std::path::PathBuf),
    #[cfg(windows)]
    Pipe(String),
    #[cfg(windows)]
    Pageant,
}

#[cfg(windows)]
const WINDOWS_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

#[cfg(unix)]
fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// Kandidaten in der Reihenfolge, in der sie probiert werden.
fn candidates(explicit: Option<&str>) -> Result<Vec<(String, Target)>, String> {
    let explicit = explicit.map(str::trim).filter(|s| !s.is_empty());
    let mut list: Vec<(String, Target)> = Vec::new();

    #[cfg(unix)]
    {
        if let Some(path) = explicit {
            list.push((format!("Socket {path}"), Target::Socket(expand_home(path))));
            return Ok(list);
        }
        if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK").filter(|s| !s.is_empty()) {
            list.push(("SSH_AUTH_SOCK".into(), Target::Socket(sock.into())));
        }
        // Bitwarden Desktop: normale Installation, Flatpak, Snap, macOS (App Store).
        for rel in [
            ".bitwarden-ssh-agent.sock",
            ".var/app/com.bitwarden.desktop/data/.bitwarden-ssh-agent.sock",
            "snap/bitwarden/current/.bitwarden-ssh-agent.sock",
            "Library/Containers/com.bitwarden.desktop/Data/.bitwarden-ssh-agent.sock",
        ] {
            if let Some(home) = std::env::var_os("HOME") {
                let path = std::path::PathBuf::from(home).join(rel);
                if path.exists() {
                    list.push((format!("Bitwarden ({})", path.display()), Target::Socket(path)));
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(path) = explicit {
            if path.eq_ignore_ascii_case("pageant") {
                list.push(("Pageant".into(), Target::Pageant));
            } else if path.starts_with(r"\\.\pipe\") || path.starts_with("//./pipe/") {
                list.push((format!("Pipe {path}"), Target::Pipe(path.replace('/', "\\"))));
            } else {
                return Err(format!(
                    "Ungültiger Pfad „{path}“. Unter Windows wird eine Named Pipe \
                     (\\\\.\\pipe\\…) oder „pageant“ erwartet."
                ));
            }
            return Ok(list);
        }
        list.push(("OpenSSH/Bitwarden-Pipe".into(), Target::Pipe(WINDOWS_PIPE.into())));
        list.push(("Pageant".into(), Target::Pageant));
    }

    #[cfg(not(any(unix, windows)))]
    let _ = explicit;

    Ok(list)
}

async fn open(target: &Target) -> Result<Agent, String> {
    match target {
        #[cfg(unix)]
        Target::Socket(path) => AgentClient::connect_uds(path)
            .await
            .map(|a| a.dynamic())
            .map_err(|e| e.to_string()),
        #[cfg(windows)]
        Target::Pipe(name) => AgentClient::connect_named_pipe(name)
            .await
            .map(|a| a.dynamic())
            .map_err(|e| e.to_string()),
        #[cfg(windows)]
        Target::Pageant => AgentClient::connect_pageant()
            .await
            .map(|a| a.dynamic())
            .map_err(|e| e.to_string()),
    }
}

/// Verbindet mit dem ersten erreichbaren Agenten. Liefert zusätzlich die Quelle.
pub async fn connect(socket: Option<&str>) -> Result<(Agent, String), String> {
    for (label, target) in candidates(socket)? {
        if let Ok(Ok(agent)) = tokio::time::timeout(CONNECT_TIMEOUT, open(&target)).await {
            return Ok((agent, label));
        }
    }
    Err(NOT_FOUND.to_string())
}

pub async fn status(socket: Option<&str>) -> AgentStatus {
    match connect(socket).await {
        Ok((_, source)) => AgentStatus { available: true, source },
        Err(_) => AgentStatus { available: false, source: String::new() },
    }
}

fn fingerprint(id: &AgentIdentity) -> String {
    id.public_key().fingerprint(HashAlg::Sha256).to_string()
}

async fn identities(agent: &mut Agent) -> Result<Vec<AgentIdentity>, String> {
    match tokio::time::timeout(APPROVAL_TIMEOUT, agent.request_identities()).await {
        Err(_) => Err("Zeitüberschreitung: Der SSH-Agent antwortet nicht. Ist der Tresor \
                       entsperrt und die Freigabe erteilt?"
            .into()),
        Ok(Err(e)) => Err(format!("SSH-Agent: Schlüsselliste konnte nicht gelesen werden: {e}")),
        Ok(Ok(list)) => Ok(list),
    }
}

/// Schlüsselliste für die Auswahl im Login-Dialog.
pub async fn list_keys(socket: Option<&str>) -> Result<Vec<AgentKey>, String> {
    let (mut agent, _) = connect(socket).await?;
    Ok(identities(&mut agent)
        .await?
        .iter()
        .map(|id| AgentKey {
            fingerprint: fingerprint(id),
            comment: id.comment().to_string(),
            algorithm: id.public_key().algorithm().to_string(),
        })
        .collect())
}

/// Meldet den Benutzer über den Agenten an. `Ok(false)`: Agent erreichbar, aber kein
/// Schlüssel wurde akzeptiert.
pub async fn login<H: client::Handler>(
    handle: &mut client::Handle<H>,
    username: &str,
    fingerprint_filter: Option<&str>,
    socket: Option<&str>,
) -> Result<bool, String> {
    let (mut agent, _) = connect(socket).await?;
    let mut ids = identities(&mut agent).await?;

    if let Some(wanted) = fingerprint_filter.map(str::trim).filter(|f| !f.is_empty()) {
        ids.retain(|id| fingerprint(id) == wanted);
        if ids.is_empty() {
            return Err(format!(
                "Der gewählte Schlüssel ({wanted}) ist im SSH-Agenten nicht vorhanden. \
                 Ist der Tresor entsperrt?"
            ));
        }
    }
    if ids.is_empty() {
        return Err("Der SSH-Agent enthält keine Schlüssel. Ist der Tresor entsperrt \
                    und mindestens ein SSH-Schlüssel angelegt?"
            .into());
    }

    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| e.to_string())?
        .flatten();

    for id in ids.into_iter().take(MAX_KEYS) {
        // Der Hash-Parameter gilt nur für RSA; bei anderen Schlüsseln würde er das Flag falsch setzen.
        let hash = if matches!(id.public_key().algorithm(), Algorithm::Rsa { .. }) {
            rsa_hash
        } else {
            None
        };
        let attempt = async {
            match id {
                AgentIdentity::PublicKey { key, .. } => handle
                    .authenticate_publickey_with(username, key, hash, &mut agent)
                    .await
                    .map_err(|e| e.to_string()),
                AgentIdentity::Certificate { certificate, .. } => handle
                    .authenticate_certificate_with(username, certificate, hash, &mut agent)
                    .await
                    .map_err(|e| e.to_string()),
            }
        };
        match tokio::time::timeout(APPROVAL_TIMEOUT, attempt).await {
            Err(_) => {
                return Err("Zeitüberschreitung: Im SSH-Agenten (z. B. Bitwarden) wurde die \
                            Freigabe nicht rechtzeitig erteilt"
                    .into())
            }
            Ok(Err(e)) => {
                return Err(format!(
                    "Der SSH-Agent konnte nicht signieren (Freigabe abgelehnt?): {e}"
                ))
            }
            Ok(Ok(result)) if result.success() => return Ok(true),
            Ok(Ok(_)) => {} // Server hat den Schlüssel nicht akzeptiert: nächsten probieren
        }
    }
    Ok(false)
}
