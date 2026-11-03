# Changelog

All notable changes to Rust Drac are recorded here, newest first, in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style. Each release tag
(`vX.Y.Z`) corresponds to the dated section it was cut from.

## 2026-11-03 (v0.5.2)

### Added
- MegaRAC firmware 13.04 (Gigabyte R163-Z32) and 12.60 (R272-Z30): console, Health & SEL and log export verified. The 13.04 viewer takes its CSRF token from a cookie and its KVM flags from the window that opened it, so the console page now seeds both.

### Fixed
- MegaRAC: SEL and audit-log pages without a next-link (firmware 12.60 and 13.04 page by 50 and report the page size as the count) are followed by `$skip`, so the Health tab and the export read the whole log instead of the first 50 entries. A BMC that ignores `$skip` and repeats the page stops the walk.
- Power supplies named "nil" (MegaRAC 13.04) are listed as "PSU <id>"; a PSU capacity given as a string such as "450W" is exported as a number instead of "450W W".
- MegaRAC: the serial number shown on the console strip and saved to the address book is now the FRU product serial (what the BMC's own FRU page shows). It used to come from the host-interface inventory, which reads "NA" until the host has reported its SMBIOS data, so Gigabyte R181 servers showed "NA" while the BMC knew the serial.

## 2026-11-02 (v0.5.1)

### Added
- ASRock Rack BMCs (AMI MegaRAC with a blank page title) are detected as MegaRAC too, from AMI's logo markup. Tested on a 1U4LW-X570/2L2T (BMC firmware 3.09): console, power, Health & SEL and log export.
- Power supplies whose health reads "Normal" and whose wattage is a string such as "450W" (ASRock Rack) show as OK with their capacity instead of Unknown.

## 2026-11-02 (v0.5.0)

### Added
- **AMI MegaRAC (Gigabyte) support**: the HTML5 viewer in a console tab, power control, Health & SEL, the alarm checker, log export and address-book autofill, detected from the BMC's "MegaRAC SP-X" page like the other vendors. Redfish there is used through one session per BMC and user (a MegaRAC refuses a PAM user's fifth basic-auth request within a minute), ended when the app exits. Tested on a Gigabyte R181-NA0 (MegaRAC SP-X firmware 12.61). The address book accepts `vendor: megarac`.
- SEL entries whose message is a raw IPMI record (AMI firmware) show the event and the sensor name instead of the key-value dump.
- Reset types are read from the action's `ActionInfo` document when the system leaves them out (AMI firmware), so the power menu only offers what the BMC accepts.
- `docs/megarac-redfish-login.md`: why a MegaRAC's Redfish rejects users the web UI accepts (PAM is off in its account service) and the one call that fixes it; the Health tab points there when a MegaRAC rejects a login.
- Console tabs have a power button showing the server's power state, with **Power On**, **Graceful Shutdown**, **Force Off**, **Reset**, **Graceful Restart** and **Power Cycle** (Redfish `ComputerSystem.Reset`). Only actions the BMC offers are enabled, everything but Power On asks first, and the state is read back afterwards.
- A rejected login ("invalid username or password") on a console or Health tab shows a **Change Password…** button: enter another user name and password and connect again. For an address-book entry the new user name is saved and the password can be saved to the keyring.

### Changed
- Saving a password where no keyring is reachable (WSL, a bare X session, SSH: no Secret Service on the session bus) now explains that and how to get one, and offers to go on without saving: connect, add or edit the server with the password kept for the session only.
- An iDRAC that refuses its name with "Access Error: 400 -- Bad Request" (the Host header check, on by default since iDRAC 8 firmware 2.81 and iDRAC 9 firmware 5.10 as part of the CVE-2021-21510 fix) no longer produces a raw HTML dump. The tab explains why, lists the racadm fixes (`DNSRacName`/`DNSDomainName`, `ManualDNSEntry`, and when disabling `HostHeaderCheck` is justified) and, once the name resolves, offers a **Connect by IP Address** button. The Health & SEL tab shows the same page (instead of the explanation squeezed into its status lines) with the same button. The Health tab and log export report the same explanation.

## 2026-11-02 (v0.4.1)

### Added
- Console tabs show the server's model, service tag / serial and BMC address above the viewer. Click a value to copy it; the copy button next to them copies all three.
- Health & SEL: SEL entries can be copied. Select one or more (Shift/Ctrl-click) and press Ctrl+C, or right-click for **Copy Line**, **Copy Message** and **Copy All Shown** (the filtered list with a header). Lines are tab-separated.

## 2026-11-01 (v0.4.0)

### Added
- **Windows build.** A per-user installer (`rustdrac-<version>-x86_64-setup.exe`, also on winget as `karubits.RustDrac`) and a portable zip. The GTK 3 interface is the same; the console is shown in Microsoft Edge WebView2 (the runtime ships with Windows 11 and current Windows 10; the installer fetches it otherwise), passwords are stored in the Credential Manager and health alarms arrive as Windows notifications. The address book lives in `%APPDATA%\rustdrac\connections.yaml`.

### Changed
- The toast at the bottom of the window ("Screenshot saved", "Export finished") no longer floats over the page; the page makes room for it while it is shown.

## 2026-11-01 (v0.3.0)

### Added
- **Supermicro X11 support.** The console tab, address-book autofill, Health & SEL tab, alarm checker and log export now also work with Supermicro X10/X11 BMCs (HTML5 iKVM). The vendor is detected on first contact, saved as `vendor:` in `connections.yaml`, and can be set by hand in the Add/Edit dialog ("BMC type").
- The address-book column and field "Service Tag" is now "Service Tag / Serial": Supermicro servers show their serial number there.

### Changed
- Export: the BMC log file is `lclog.csv` on Dell (as before) and `maintenance.csv` on Supermicro. In `summary.json` the `idrac` key is now `bmc` (with `kind`, `model`, `firmware`), `lc_log_entries` is `bmc_log` (`kind`, `entries`, `file`), and there is a `vendor` field.
- The live export test is `export_from_a_real_bmc`; `RUSTDRAC_TEST_VENDOR=dell|supermicro` skips the vendor probe.

## 2026-10-31

Now published on GitHub: https://github.com/karubits/rustdrac

### Added
- **Settings.** A ☰ menu in the header bar opens Settings, where you choose the screenshot folder (type it, browse for it, or reset to `~/Pictures/Rust Drac`). It is saved as `defaults.screenshot_dir`.
- **About dialog.** Shows the version, a link to the source code on GitHub, the licence and how to contribute.
- `CONTRIBUTING.md`, and badges and a feature overview in the README.

### Changed
- CI and releases run on GitHub Actions (`.github/workflows/`), building in a Debian 12 container as before.

## 2026-10-30

iDRAC 9 support (after v0.1.0, the same day).

### Added
- **iDRAC 9.** The virtual console, Health & SEL and the alarm checker work on iDRAC 9. Tested on PowerEdge R6515 with iDRAC 9 firmware 7.10.30.
  - The iDRAC generation is detected automatically, so address-book entries need no new setting.
  - Login uses the iDRAC 9 session API. The noVNC viewer gets its KVM session ids the way Dell's web UI hands them over, so they never appear in the URL.
  - Closing a tab unloads the viewer and deletes the session; reconnecting right away doesn't hit the "share this session" prompt.

- **Screenshot.** A header-bar button saves the console picture as a PNG at its native resolution, without Dell's toolbar, to `~/Pictures/Rust Drac` (`defaults.screenshot_dir`). A notice offers to open the folder.
- **Export Logs.** Saves a zip per server, from the header bar or the address book's right-click menu:
  - `summary.txt` / `summary.json`: model, service tag, serial, BIOS and iDRAC firmware, health, CPUs, DIMMs, storage, NICs and PSUs;
  - `health.csv`, `sel.csv`, and `lclog.csv` (the newest 500 Lifecycle Controller entries).

  It has progress and Cancel, and contains no credentials.

### Fixed
- **No GPU.** The console now loads on machines without a GPU render node (`/dev/dri`), such as VMs without a virtual GPU and containers. The app switches Mesa to software rendering (`LIBGL_ALWAYS_SOFTWARE=1`) when there is no render node.
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
- The console needs a GPU render node (`/dev/dri`). Desktops and most VMs with a virtual GPU have one; without it, WebKit's web process stalls and the console page never loads. The address book and Health & SEL still work. (Fixed in the section above.)
- The AppImage needs glibc 2.36 or newer (Debian 12, Ubuntu 23.04, Fedora 37 and later).
- iDRAC 8 Redfish is slow (about 5 s per request plus a TLS handshake), so a full Health & SEL read of an R630 takes about a minute.
