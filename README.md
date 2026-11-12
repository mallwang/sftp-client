# SFTP Client (Tauri 2)

Kleiner SFTP-Client mit Login per **Passwort** oder **SSH-Key**.

- Backend: Rust, [`russh`](https://github.com/Eugeny/russh) + `russh-sftp` (reines Rust, kein OpenSSL)
- Frontend: Vanilla TypeScript + Vite

## Funktionen

- Login per Passwort (auch über *keyboard-interactive*) oder Private Key (ed25519, RSA, ECDSA; OpenSSH- und PEM-Format, optional mit Passphrase)
- Host-Key-Prüfung nach dem Prinzip *Trust on first use*: Beim ersten Verbinden wird der Fingerprint gespeichert,
  bei späteren Verbindungen wird ein geänderter Key abgelehnt.
  Gespeichert in `%APPDATA%\dev.local.sftp-client\known_hosts`
- Verzeichnisse durchsuchen (Doppelklick auf einen Ordner, ⬆-Button, Pfad eintippen)
- Download, Upload (Button oder Drag & Drop aus dem Explorer) mit Fortschrittsanzeige
- Ordner anlegen, Umbenennen/Verschieben, Löschen (Ordner rekursiv)
- Speichert die letzten Verbindungsdaten (**ohne** Passwort/Passphrase)

## Voraussetzungen (Windows)

1. **Rust**: https://rustup.rs (`rustup-init.exe`, Standard-Toolchain `stable-x86_64-pc-windows-msvc`)
2. **Microsoft C++ Build Tools**: https://visualstudio.microsoft.com/visual-cpp-build-tools/
   → Workload „Desktopentwicklung mit C++“
3. **WebView2**: ist unter Windows 10/11 normalerweise schon vorhanden
4. **Node.js** ≥ 24 (npm ≥ 11; `nvm use` liest `.nvmrc`)

Siehe auch https://v2.tauri.app/start/prerequisites/

## Starten

```bash
npm install
npm run tauri dev      # Entwicklungsmodus
npm run tauri build    # Installer/Exe unter src-tauri/target/release/bundle
```

## Projektstruktur

```
index.html, src/main.ts, src/style.css   Frontend (UI)
src-tauri/src/sftp.rs                    SSH/SFTP-Logik und Tauri-Commands
src-tauri/src/lib.rs                     App-Setup
src-tauri/tauri.conf.json                Fenster-, Build- und Bundle-Konfiguration
src-tauri/capabilities/default.json      Berechtigungen (Dialog-Plugin)
```

## Bekannte Einschränkungen

- Upload/Download nur für einzelne Dateien, keine ganzen Ordner
- Kein SSH-Agent/Pageant
- Nur eine Verbindung gleichzeitig
