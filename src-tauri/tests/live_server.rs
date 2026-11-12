//! Integrationstests gegen einen echten SFTP-Server.
//! Zugangsdaten stehen in `.env.test` im Projektverzeichnis. Ausführen mit:
//!   cargo test --test live_server -- --ignored --nocapture

use std::sync::Arc;

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use russh_sftp::client::SftpSession;

struct AcceptAll;
impl client::Handler for AcceptAll {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true) // nur für Tests im lokalen Netz
    }
}

fn env() -> std::collections::HashMap<String, String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../.env.test");
    std::fs::read_to_string(path)
        .expect(".env.test fehlt")
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

async fn connect() -> (SftpSession, String) {
    let env = env();
    let host = env["SFTP_TEST_HOST"].clone();
    let port: u16 = env["SFTP_TEST_PORT"].parse().unwrap();
    assert!(!host.is_empty() && !env["SFTP_TEST_PASSWORD"].is_empty(), "Host/Passwort in .env.test eintragen");
    let mut handle = client::connect(Arc::new(client::Config::default()), (host.as_str(), port), AcceptAll)
        .await
        .expect("connect");
    let auth = handle
        .authenticate_password(env["SFTP_TEST_USER"].clone(), env["SFTP_TEST_PASSWORD"].clone())
        .await
        .expect("auth");
    assert!(auth.success(), "Anmeldung fehlgeschlagen");
    let channel = handle.channel_open_session().await.unwrap();
    channel.request_subsystem(true, "sftp").await.unwrap();
    let sftp = SftpSession::new(channel.into_stream()).await.unwrap();
    (sftp, env["SFTP_TEST_DIR"].clone())
}

/// Ein auf dem Server (hier: ebenfalls über SFTP) neu angelegter Ordner muss beim
/// nächsten `read_dir` auf derselben Session sofort sichtbar sein.
#[test]
#[ignore = "braucht echten Server (.env.test)"]
fn new_folder_is_visible_after_relisting() {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
        let (sftp, dir) = connect().await;
        let name = format!("{dir}/refresh-test-{}", std::process::id());
        let before: Vec<String> = sftp.read_dir(dir.clone()).await.unwrap().map(|e| e.file_name()).collect();
        sftp.create_dir(name.clone()).await.expect("mkdir");
        let after: Vec<String> = sftp.read_dir(dir.clone()).await.unwrap().map(|e| e.file_name()).collect();
        sftp.remove_dir(name.clone()).await.unwrap();
        let short = name.rsplit('/').next().unwrap();
        println!("vorher: {before:?}\nnachher: {after:?}");
        assert!(!before.iter().any(|n| n == short));
        assert!(after.iter().any(|n| n == short), "neuer Ordner fehlt im Listing");
    });
}

/// Ein Unterordner-Upload-Szenario: Ordner anlegen, Datei schreiben, wieder löschen.
#[test]
#[ignore = "braucht echten Server (.env.test)"]
fn write_permissions_in_test_dir() {
    use tokio::io::AsyncWriteExt;
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
        let (sftp, dir) = connect().await;
        let file = format!("{dir}/write-test-{}.txt", std::process::id());
        let mut f = sftp.create(file.clone()).await.expect("create");
        f.write_all(b"hallo").await.unwrap();
        f.shutdown().await.unwrap();
        sftp.remove_file(file).await.unwrap();
        // Außerhalb des freigegebenen Ordners sollte "Permission denied" kommen:
        let denied = sftp.create("/write-denied-test.txt".to_string()).await;
        println!("Schreiben außerhalb: {:?}", denied.as_ref().err().map(|e| e.to_string()));
    });
}

/// Fortsetzen: Datei teilweise schreiben, ohne Abschneiden neu öffnen, ab Offset weiterschreiben.
#[test]
#[ignore = "braucht echten Server (.env.test)"]
fn resume_write_at_offset() {
    use russh_sftp::protocol::OpenFlags;
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async {
        let (sftp, dir) = connect().await;
        let path = format!("{dir}/resume-test-{}.bin", std::process::id());
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let cut = 2_000_000usize;

        let mut f = sftp.create(path.clone()).await.unwrap();
        f.write_all(&data[..cut]).await.unwrap();
        f.shutdown().await.unwrap();
        assert_eq!(sftp.metadata(path.clone()).await.unwrap().len(), cut as u64);

        let start = cut - 1_048_576; // Überlappung wie in der App
        let mut f = sftp.open_with_flags(path.clone(), OpenFlags::WRITE).await.unwrap();
        f.seek(std::io::SeekFrom::Start(start as u64)).await.unwrap();
        f.write_all(&data[start..]).await.unwrap();
        f.shutdown().await.unwrap();

        let mut back = Vec::new();
        sftp.open(path.clone()).await.unwrap().read_to_end(&mut back).await.unwrap();
        sftp.remove_file(path).await.unwrap();
        assert_eq!(back.len(), data.len());
        assert!(back == data, "Inhalt nach dem Fortsetzen stimmt nicht");
    });
}
