# SFTP Client (Tauri 2)

Kleiner SFTP-Client mit Login per **SSH-Agent** (z. B. Bitwarden), **SSH-Key** oder **Passwort**.

- Backend: Rust, [`russh`](https://github.com/Eugeny/russh) + `russh-sftp` (reines Rust, kein OpenSSL)
- Frontend: Vanilla TypeScript + Vite

## Funktionen

- Login über einen **SSH-Agenten** (Bitwarden, OpenSSH-Agent, `ssh-agent`, Pageant), ohne dass der private Schlüssel in die App geladen wird
- Login per Passwort (auch über *keyboard-interactive*) oder Private Key (ed25519, RSA, ECDSA; OpenSSH- und PEM-Format, optional mit Passphrase)
- Host-Key-Prüfung nach dem Prinzip *Trust on first use*: Beim ersten Verbinden wird der Fingerprint gespeichert,
  bei späteren Verbindungen wird ein geänderter Key abgelehnt.
  Gespeichert in `%APPDATA%\dev.local.sftp-client\known_hosts`
- Verzeichnisse durchsuchen (Doppelklick auf einen Ordner, ⬆-Button, Pfad eintippen)
- Download, Upload (Button oder Drag & Drop aus dem Explorer) mit Fortschrittsanzeige
- Ordner anlegen, Umbenennen/Verschieben, Löschen (Ordner rekursiv)
- Speichert die letzten Verbindungsdaten (**ohne** Passwort/Passphrase)

## SSH-Agent

Im Login-Dialog „SSH-Agent“ wählen. Der private Schlüssel bleibt im Agenten, die App erhält nur die öffentlichen
Schlüssel und lässt Signaturen vom Agenten erzeugen. Die Liste „Schlüssel“ zeigt die Schlüssel des Agenten
(„Automatisch“ probiert bis zu 5 Schlüssel der Reihe nach). Gespeichert wird nur der Fingerprint, kein Geheimnis.

Der Bitwarden-Agent zeigt pro Anmeldung ein Freigabe-Fenster (Wartezeit bis 60 s). Das gilt auch beim
automatischen Wiederverbinden.

**Windows**
- Bitwarden: Einstellungen › SSH-Agent aktivieren, Tresor entsperren. Bitwarden und der Windows-OpenSSH-Agent
  nutzen dieselbe Pipe `\\.\pipe\openssh-ssh-agent`, es kann nur einer laufen. Belegt der Dienst „OpenSSH Authentication Agent“ die
  Pipe, diesen beenden (`Stop-Service ssh-agent`) und deaktivieren.
- Erkennung: 1. Pipe `\\.\pipe\openssh-ssh-agent`, 2. Pageant.

**Linux / macOS**
- Erkennung: 1. `SSH_AUTH_SOCK`, 2. Bitwarden-Socket `~/.bitwarden-ssh-agent.sock` (Flatpak:
  `~/.var/app/com.bitwarden.desktop/data/.bitwarden-ssh-agent.sock`, Snap:
  `~/snap/bitwarden/current/.bitwarden-ssh-agent.sock`, macOS App Store:
  `~/Library/Containers/com.bitwarden.desktop/Data/.bitwarden-ssh-agent.sock`).
- Für Sonderfälle: im Dialog unter „Erweitert“ den Socket-/Pipe-Pfad eintragen (unter Windows auch `pageant`).
- Programme, die nicht aus der Shell gestartet werden (Startmenü), erben `SSH_AUTH_SOCK` oft nicht. Dann den Pfad unter „Erweitert“ eintragen.

## Voraussetzungen

Gemeinsam für alle Systeme: **Node.js 24** (npm ≥ 11) und **Rust** (stable). Node-Version per [nvm](https://github.com/nvm-sh/nvm):

```bash
nvm install    # liest .nvmrc (24)
nvm use
```

Rust: https://rustup.rs

### Linux (auch WSL2, Debian/Ubuntu)

Für den nativen Linux-Build (`npm run build:linux`):

```bash
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

Andere Distributionen: https://v2.tauri.app/start/prerequisites/#linux

**Zusätzlich für den Windows-Cross-Build** (`npm run build:windows`, `.exe` + NSIS-Installer):

```bash
sudo apt install clang lld llvm nsis
rustup target add x86_64-pc-windows-msvc
cargo install --locked cargo-xwin
```

- `cargo-xwin` lädt beim ersten Build die Microsoft-CRT- und Windows-SDK-Header/-Libs (ca. 1–2 GB, einmalig).
  Mit `XWIN_CACHE_DIR=~/.xwin-cache` wird der Cache zwischen Projekten geteilt.
- Der Cross-Build ist experimentell: nur NSIS-Installer (kein MSI), keine Signierung. Die Linker-Warnungen
  `LNK4099` (fehlende `.pdb`-Dateien der Microsoft-Runtime) sind harmlos.
- In der WSL läuft die Linux-App über WSLg. Für die Windows-`.exe` ist in der WSL nichts weiter nötig, sie wird unter Windows gestartet.

### Windows

Für den nativen Windows-Build (`npm run tauri build`, erzeugt MSI und NSIS):

1. **Microsoft C++ Build Tools**: https://visualstudio.microsoft.com/visual-cpp-build-tools/
   → Workload „Desktopentwicklung mit C++“
2. **Rust**: https://rustup.rs (`rustup-init.exe`, Toolchain `stable-x86_64-pc-windows-msvc`)
3. **WebView2**: unter Windows 10/11 normalerweise schon vorhanden
4. **Node.js 24**: z. B. mit [nvm-windows](https://github.com/coreybutler/nvm-windows): `nvm install 24` und `nvm use 24`
   (nvm-windows liest `.nvmrc` nicht automatisch)

WiX (für MSI) und NSIS lädt Tauri beim ersten Build selbst herunter.

Ein Linux-Build von Windows aus wird nicht unterstützt. Dafür WSL2 nutzen (siehe oben).

Siehe auch https://v2.tauri.app/start/prerequisites/

## Starten und Bauen

```bash
npm install
npm run tauri dev        # Entwicklungsmodus
npm run build:linux      # Linux-Bundles unter src-tauri/target/release/bundle
npm run build:windows    # Windows-Cross-Build (aus Linux/WSL), Ausgabe unter
                         # src-tauri/target/x86_64-pc-windows-msvc/release/
npm run tauri build      # Native Build für das aktuelle System
```

Unter Linux/WSL die gebaute App starten: `./src-tauri/target/release/sftp-client`.
Die Mesa/EGL-Warnungen (`libEGL warning`, `ZINK`) unter WSLg sind harmlos; mit `LIBGL_ALWAYS_SOFTWARE=1` verschwinden sie meist.

## Projektstruktur

```
index.html, src/main.ts, src/style.css   Frontend (UI)
src-tauri/src/sftp.rs                    SSH/SFTP-Logik und Tauri-Commands
src-tauri/src/agent.rs                   SSH-Agent-Anbindung (Socket/Pipe/Pageant, Signatur-Login)
src-tauri/src/lib.rs                     App-Setup
src-tauri/tauri.conf.json                Fenster-, Build- und Bundle-Konfiguration
src-tauri/capabilities/default.json      Berechtigungen (Dialog-Plugin)
```

## Bekannte Einschränkungen

- Upload/Download nur für einzelne Dateien, keine ganzen Ordner
- Nur eine Verbindung gleichzeitig
