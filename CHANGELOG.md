# Changelog

All notable changes to Rust Drac are recorded here, newest first, in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style. Each release tag
(`vX.Y.Z`) corresponds to the dated section it was cut from.

## 2026-10-30

iDRAC 9 support (after v0.1.0, the same day).

### Added
- **iDRAC 9.** The virtual console, Health & SEL and the alarm checker work on iDRAC 9. Tested on PowerEdge R6515 with iDRAC 9 firmware 7.10.30.
  - The iDRAC generation is detected automatically, so address-book entries need no new setting.
  - Login uses the iDRAC 9 session API. The noVNC viewer gets its KVM session ids the way Dell's web UI hands them over, so they never appear in the URL.
  - Closing a tab unloads the viewer and deletes the session; reconnecting right away doesn't hit the "share this session" prompt.

### Fixed
- The viewer's UI language is set to English when the system locale is `C`. The iDRAC 9 viewer used to request a missing `c.json` locale file.

### Known Issues
- iDRAC 9 two-factor (TFA/RSA) logins and the "password must be changed" state are reported as errors, not handled.

## 2026-10-30

First release (v0.1.0).

### Added
- **Virtual console.** Opens the iDRAC 8 HTML5 virtual console in its own window without a browser.
  - Logs in behind the scenes and ignores self-signed certificates.
  - Provides the opener window and the certificate popup that Dell's viewer expects.
- **Tabs.** Consoles to several servers can be open at once, each in its own tab.
  - Closing a tab disconnects the KVM the way *Disconnect Viewer* does, then logs out, so the next connect doesn't hit "A session to this device already exists".
  - The console page is retried if the iDRAC drops the connection while it loads.
- **Connection manager.** An address book of servers.
  - Each server has a name, address, user, group, tags, service tag, model and notes.
  - Search covers every field.
  - Quick Connect handles one-off servers.
  - Service tag and model are filled in from Redfish on first connect.
- **Config file.** The address book is plain YAML (`~/.config/rustdrac/connections.yaml`), so it can be copied between machines.
- **Passwords.** Optionally saved in the desktop keyring (Secret Service); never written to the config file.
- **Health & SEL tab.**
  - Component health: system, CPUs, memory, PSUs, voltages, fans, temperatures, storage controllers and drives.
  - The full System Event Log, with a severity filter, search and CSV export.
  - Clear SEL, after a confirmation.
  - When the health rollup is in alarm while every component reads OK, the SEL events that explain it are listed with the components.
- **Alarm checker.**
  - A Health column in the address book.
  - **Check All**, plus a periodic background check (`defaults.health_check_minutes`, default 15).
  - A desktop notification when a server becomes Warning or Critical.
- **Command line.** `rustdrac <host>…` opens consoles directly; `--health` opens Health & SEL instead.
- **Look.** Dark theme by default with an orange accent; `--theme light|system` switches it.
- **Packaging.**
  - An app icon and desktop entry.
  - Release builds as an AppImage and a tarball, with SHA256SUMS.
- **CI.**
  - rustfmt, clippy (warnings are errors) and tests.
  - cargo-deny: advisories, licences, bans and sources.
  - gitleaks and betterleaks secret scanning.

### Known Issues
- The console needs a GPU render node (`/dev/dri`). Desktops and most VMs with a virtual GPU have one; without it, WebKit's web process stalls and the console page never loads. The address book and Health & SEL still work.
- The AppImage needs glibc 2.36 or newer (Debian 12, Ubuntu 23.04, Fedora 37 and later).
- iDRAC 8 Redfish is slow (about 5 s per request plus a TLS handshake), so a full Health & SEL read of an R630 takes about a minute.
