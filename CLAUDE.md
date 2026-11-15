# MieryCommander

Zweispaltiger Dateimanager (Total-Commander-Stil) in Rust mit eframe/egui 0.36, für Linux, macOS und Windows.
Repo: https://github.com/Evil8810/MieryCommander (öffentlich, MIT). Webseite: GitHub Pages aus `docs/` auf `main`.
Der Nutzer schreibt Deutsch; Antworten, Commit-Texte und Release-Notes auf Deutsch (Release-Notes zweisprachig DE/EN).

## Bauen, testen, starten

Rust kommt auf dem Entwicklungsrechner (Bazzite/Fedora Atomic) aus Homebrew, nicht von rustup – es gibt dort nur das Host-Target.

```bash
cargo run --release
cargo test --release              # ~80 Tests, inkl. UI-Tests mit egui_kittest (wgpu-Render)
cargo test --release <name>       # einzelner Test
./package-linux.sh                # AppImage + tar.gz nach target/linux
./install-desktop.sh              # Startmenü-Eintrag + Icon unter ~/.local (Linux)
./install-macos.sh --universal --dmg --no-install   # nur auf macOS
```

Netzwerktests sind per Umgebungsvariable abgeschaltet und laufen nur mit echtem Server:
`MIERY_FTP_TEST/ROOT`, `MIERY_SFTP_TEST/ROOT/KEYS`, `MIERY_SMB_TEST/SHARE`. Website-Screenshots: `MIERY_SCREENSHOTS`.
Ignorierte Tests (`-- --ignored`): `real_apps` (Öffnen-mit-Liste dieses Rechners), `real_update` (lädt das echte GitHub-Release).

Windows und macOS lassen sich lokal nicht bauen – dafür die CI nutzen (siehe unten).

## Aufbau (src/)

| Datei | Inhalt |
|---|---|
| `main.rs` | Einstieg, Viewport (app_id `miery-commander`, Icon), Modul-Liste |
| `app.rs` | `MieryApp`: Zustand, `Cmd`-Enum + `run()`, Tastenbelegung (`handle_keys`, `raw_input_hook`), Menü/Toolbar/F-Leiste, Job-Fenster + Warteschlange, Zoom, Updater-Anbindung, Auto-Verbinden |
| `panel.rs` | `Panel`/`Tab`/`Location` (Dir, Archive, Ftp), asynchrones Einlesen mit Generationszähler, Tabelle, Kontextmenüs, Drag & Drop, Umbenennen in der Liste |
| `dialogs.rs` | Alle Modal-Dialoge (`Dialog`-Enum), Verbindungsdialog, Einstellungen, Tastenkürzel-Tabelle |
| `ops.rs` | Hintergrund-Jobs (`JobKind`, `Worker`): Kopieren/Verschieben/Löschen, Archive, Up-/Download, Server↔Server, Sync; Überschreiben-Fragen über Kanal |
| `archive.rs` | ZIP/TAR(+gz/bz2/xz/zst)/7z/RAR lesen, Index-Cache, Entpacken (`Sink`), Packen, `rewrite()` zum Ändern von ZIP/TAR |
| `remote.rs` | `Remote`-Trait, Verbindungsregister, `Site`, Schlüsselbund, Erreichbarkeit + Auto-Verbinden |
| `ftp.rs` / `sftp.rs` | FTP/FTPS (suppaftp+rustls) / SFTP (russh, eigene tokio-Runtime, known_hosts) |
| `smb.rs` | SMB über das System: kio-fuse (D-Bus) / gio (Linux), Finder `mount volume` (macOS), WNet (Windows); Netzwerksuche, Freigabenliste |
| `fsutil.rs` | `read_dir`, Laufwerke/Mounts (`locations()` im Hintergrund gecacht), Formatierung, Terminal/Öffnen |
| `winsys.rs` | Windows-API (Laufwerke, Speicherplatz, Freigaben) – nur `cfg(windows)` |
| `openwith.rs` | „Öffnen mit“: .desktop + MIME-DB (Linux), Launch Services (macOS), System-Dialog (Windows) |
| `update.rs` | Update-Prüfung gegen GitHub-Releases und Installation (AppImage, tar.gz, .app, .exe) |
| `i18n.rs` | `l!(de, en)` / `lf!(de, en, args)` – jeder UI-Text steht in beiden Sprachen |
| `oplog.rs` | Protokoll der Vorgänge: im Speicher + Textdatei im Datenordner (in Tests nur Speicher) |
| `thumbs.rs` | Vorschaubilder: Cache pro App, Dekodieren in zwei Hintergrund-Threads |
| `clip.rs`, `compare.rs`, `sync.rs`, `search.rs`, `rename.rs`, `viewer.rs`, `fonts.rs` | Zwischenablage, Dateivergleich, Synchronisieren, Suche, Mehrfach-Umbenennen, Lister, Ersatzschriften |
| `tests.rs` | UI-/Integrationstests (Harness-Helfer, `DirRemote` als nachgestellter Server) |

## Konventionen

- **Nie den UI-Thread blockieren:** Dateisystem, Netzwerk, Archive laufen in Threads mit `mpsc`; danach `fsutil::wake_ui()`. Veraltete Ergebnisse über Generationszähler verwerfen.
- **Texte immer zweisprachig** mit `l!`/`lf!`; Tastennamen deutsch schreiben („Strg“, „Entf“), `i18n::keys()` übersetzt sie.
- **Passwörter nie auf die Kommandozeile** (stdin, Umgebung oder System-API) und nie in die Konfiguration – nur in den System-Schlüsselbund.
- **Daten des Nutzers schützen:** Archive und überschriebene Dateien (`ops::part_path`) werden in eine Nachbardatei geschrieben und erst am Ende ersetzt; beim Verschieben nie eine Quelle löschen, in der etwas übersprungen wurde (`Worker::skipped`); bei Verschieben die Quelle nur nach Erfolg löschen; endgültiges Löschen immer mit Rückfrage.
- Plattformcode mit `#[cfg(...)]`; Windows-Pfade und `\` in Tests bedenken (`std::path::MAIN_SEPARATOR_STR`).
- Kommentare auf Englisch, knapp, im Stil des umgebenden Codes.

## Test-Fallen

- egui_kittest: ein Frame = 1/4 s, `steps()`/`h.step()` kann zwei Frames laufen → Doppelklicks nur mit eigenem Harness `with_step_dt(1.0/60.0)`.
- Modals sind im ersten (Mess-)Frame nicht klickbar → nach dem Öffnen `steps(&mut h, 2)`.
- Dialoge bekommen pro Öffnen eine neue Id (`dialog_serial`) und ein `layout_key()`; sonst übernimmt egui die alte Höhe und Knöpfe werden abgeschnitten. Prüfen mit `assert_buttons_visible` (Pixel).
- Tests dürfen nie die echte Zwischenablage, den echten Papierkorb (`__no_trash__` im Pfad simuliert „kein Papierkorb“) oder das Netzwerk benutzen; Update-Prüfung ist in Tests aus.
- Auf Loopback kann ein Connect auf einen gerade geschlossenen hohen Port bei sich selbst landen → für „nicht erreichbar“ Port 1 nehmen.

## Datenschutz

- `beispiel/` (fremde TC-Screenshots) ist in `.gitignore` und darf nie veröffentlicht werden.
- Keine echten NAS-/Mount-Namen oder die private E-Mail in Code, Tests, Screenshots oder Commits. Screenshots nutzen `PLACES_OVERRIDE` mit Demo-Laufwerken.
- Git-Identität: `Evil8810 <196352677+Evil8810@users.noreply.github.com>`.

## CI und Release

- `.github/workflows/build.yml`: Jobs `macos` (universal .dmg), `linux` (Ubuntu 22.04: AppImage + tar.gz), `windows` (.zip), jeweils mit `cargo test`; `release` nur bei Tags `v*`.
- Branch-Builds ohne PR: `gh workflow run build.yml --ref <branch>`.
- Release-Ablauf: CI auf `main` grün → Version in `Cargo.toml` erhöhen → Commit → `git tag -a vX.Y.Z` → `main` und Tag pushen → nach dem Release-Build die zweisprachigen Notes mit `gh release edit vX.Y.Z --notes-file …` setzen.
- Releases, Tags und Force-Pushes nur nach Freigabe durch den Nutzer.
- Auf macOS-Runnern scheitert `hdiutil` gelegentlich mit „Resource busy“ – das Skript wiederholt es; sonst Job neu starten.
- Der eingebaute Updater erkennt Downloads am Namensmuster: `-<arch>.AppImage`, `-linux-<arch>.tar.gz`, `.dmg`, `-windows-<arch>.zip` – Paketnamen nicht ändern.

## Offene Punkte

- Windows-Version nur per CI getestet, noch nicht von Hand auf einem PC.
- Server↔Server nur mit nachgestellten Servern getestet, nicht gegen echte FTP/SFTP-Server.
- macOS-Rückmeldungen (Nutzer-Familie mit NAS) zu Löschen ohne Papierkorb, Auto-Verbinden und Umlaut-Dateinamen stehen aus.
- Neu und nur per Test/Render geprüft, noch nicht von Hand: Trenner ziehen, einspaltige Ansicht, Vorschaubilder, Pause/Hintergrund, Platzprüfung, Prüfen nach dem Kopieren, Protokoll.
- Sicheres Überschreiben gilt bisher nur lokal (auch eingebundene Netzlaufwerke), nicht für FTP/SFTP-Uploads.
- Ideen: Video-Vorschau, eigene Spalten, Baumansicht, Explorer/Finder-Zwischenablage, Windows-Installer.
