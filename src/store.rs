//! The address book: `~/.config/rustdrac/connections.yaml` (`%APPDATA%\rustdrac\connections.yaml`
//! on Windows).
//!
//! ```yaml
//! defaults:
//!   user: admin
//!   theme: dark          # dark (default), light or system
//!   health_check_minutes: 15   # background alarm check; 0 turns it off
//!   screenshot_dir: ~/Pictures/Rust Drac
//! connections:
//!   - id: 3f9c1a2b7d4e5f60
//!     name: lab-r630
//!     address: 192.0.2.10
//!     user: admin
//!     group: Lab
//!     tags: [r630, pxe]
//!     service_tag: ABC1234     # or the serial number on other vendors
//!     model: PowerEdge R630
//!     vendor: dell             # dell or supermicro; learned on first connect, or set by hand
//!     notes: Rack 3, U12
//!     save_password: true
//! ```
//!
//! The file is plain YAML so it can be copied between machines. Passwords are never written
//! to it: when `save_password` is set they live in the desktop keyring (Secret Service on
//! Linux, the Credential Manager on Windows),
//! keyed by the connection `id`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::bmc::Vendor;

const KEYRING_SERVICE: &str = "rustdrac";
/// The project used to be called idrac-console; its config and saved passwords are migrated.
const OLD_NAME: &str = "idrac-console";
const DEFAULT_HEALTH_CHECK_MINUTES: u64 = 15;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub connections: Vec<Connection>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// dark (default), light or system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// How often servers with a saved password are checked for alarms (default 15, 0 = off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_check_minutes: Option<u64>,
    /// Where console screenshots go (default: ~/Pictures/Rust Drac). `~/` is expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot_dir: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connection {
    #[serde(default = "new_id")]
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub address: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_tag: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Learned on first connect (or set by hand); `None` means detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<Vendor>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub save_password: bool,
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let mixed = nanos
        ^ (u64::from(std::process::id()) << 32)
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{mixed:016x}")
}

impl Connection {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.address
        } else {
            &self.name
        }
    }

    /// Every whitespace-separated term must appear (case-insensitively) in some field.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = [
            self.name.as_str(),
            &self.address,
            &self.user,
            &self.group,
            &self.service_tag,
            &self.model,
            &self.notes,
            &self.tags.join(" "),
        ]
        .join("\n")
        .to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|term| haystack.contains(term))
    }

    pub fn saved_password(&self) -> Option<String> {
        if !self.save_password {
            return None;
        }
        let read = |service| keyring::Entry::new(service, &self.id)?.get_password();
        let result = match read(KEYRING_SERVICE) {
            Err(keyring::Error::NoEntry) => read(OLD_NAME).inspect(|p| {
                if self.set_saved_password(p).is_ok() {
                    let _ =
                        keyring::Entry::new(OLD_NAME, &self.id).and_then(|e| e.delete_credential());
                }
            }),
            r => r,
        };
        match result {
            Ok(password) => Some(password),
            Err(e) => {
                eprintln!(
                    "rustdrac: reading saved password for {}: {e:#}",
                    self.display_name()
                );
                None
            }
        }
    }

    pub fn set_saved_password(&self, password: &str) -> Result<()> {
        keyring_entry(&self.id)?
            .set_password(password)
            .context("saving password to the system keyring")
    }

    pub fn forget_password(&self) {
        if let Ok(entry) = keyring_entry(&self.id) {
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(e) => eprintln!("rustdrac: removing saved password: {e}"),
            }
        }
    }
}

fn keyring_entry(id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, id).context("opening the system keyring")
}

pub fn path() -> PathBuf {
    if let Some(path) = std::env::var_os("RUSTDRAC_CONFIG") {
        return path.into();
    }
    config_path("rustdrac")
}

/// `/home/me/.config/…` as `~/.config/…` (`C:\Users\me\…` as `~\…`).
pub fn display_path(path: &Path) -> String {
    match dirs::home_dir().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rel) => format!("~{}{}", std::path::MAIN_SEPARATOR, rel.display()),
        None => path.display().to_string(),
    }
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `~/x` (or `~\x`) as `/home/me/x`; anything else unchanged.
pub fn expand_home(dir: &str) -> PathBuf {
    match dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")) {
        Some(rel) => home_dir().join(rel),
        None => PathBuf::from(dir),
    }
}

/// `~/Pictures/Rust Drac` (the desktop's Pictures folder).
pub fn default_screenshot_dir() -> PathBuf {
    dirs::picture_dir()
        .unwrap_or_else(|| home_dir().join("Pictures"))
        .join("Rust Drac")
}

fn config_path(app: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(app)
        .join("connections.yaml")
}

impl Store {
    pub fn load() -> Result<Store> {
        let path = path();
        let old = config_path(OLD_NAME);
        if std::env::var_os("RUSTDRAC_CONFIG").is_none() && !path.exists() && old.exists() {
            let store = Self::load_from(&old)?;
            store.save_to(&path)?;
            eprintln!("rustdrac: migrated {} to {}", old.display(), path.display());
            return Ok(store);
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Store> {
        let mut store: Store = match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Store::default(),
            Ok(text) => serde_yaml_ng::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        // Hand-written entries may share or lack ids; make them unique.
        let mut seen = std::collections::HashSet::new();
        for c in &mut store.connections {
            if !seen.insert(c.id.clone()) {
                c.id = new_id();
                seen.insert(c.id.clone());
            }
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_yaml_ng::to_string(self).context("serialising connections")?;
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }

    pub fn get(&self, id: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.id == id)
    }

    /// Look up a connection by name, address or service tag (for the command line).
    pub fn find(&self, key: &str) -> Option<&Connection> {
        let key = key.to_lowercase();
        self.connections.iter().find(|c| {
            [&c.name, &c.address, &c.service_tag]
                .iter()
                .any(|f| !f.is_empty() && f.to_lowercase() == key)
        })
    }

    /// Insert or replace (by id).
    pub fn upsert(&mut self, conn: Connection) {
        match self.get_mut(&conn.id) {
            Some(existing) => *existing = conn,
            None => self.connections.push(conn),
        }
    }

    pub fn remove(&mut self, id: &str) -> Option<Connection> {
        let idx = self.connections.iter().position(|c| c.id == id)?;
        Some(self.connections.remove(idx))
    }

    pub fn groups(&self) -> Vec<String> {
        let mut groups: Vec<String> = self
            .connections
            .iter()
            .filter(|c| !c.group.is_empty())
            .map(|c| c.group.clone())
            .collect();
        groups.sort_by_key(|g| g.to_lowercase());
        groups.dedup();
        groups
    }

    pub fn health_check_minutes(&self) -> u64 {
        self.defaults
            .health_check_minutes
            .unwrap_or(DEFAULT_HEALTH_CHECK_MINUTES)
    }

    pub fn screenshot_dir(&self) -> PathBuf {
        match self.defaults.screenshot_dir.as_deref().map(str::trim) {
            Some(dir) if !dir.is_empty() => expand_home(dir),
            _ => default_screenshot_dir(),
        }
    }

    /// Set the screenshot folder as typed in Settings. Empty, or the default folder, clears
    /// the setting; a folder under the home directory is written as `~/…`.
    pub fn set_screenshot_dir(&mut self, dir: &str) {
        let dir = expand_home(dir.trim());
        self.defaults.screenshot_dir = (!dir.as_os_str().is_empty()
            && dir != default_screenshot_dir())
        .then(|| display_path(&dir));
    }

    pub fn default_user(&self) -> String {
        self.defaults.user.clone().unwrap_or_else(|| "root".into())
    }

    /// The user to log in as: the connection's own, else `defaults.user`, else the vendor's
    /// factory account (`root` when the vendor is not known yet).
    pub fn user_for(&self, conn: &Connection) -> String {
        if !conn.user.is_empty() {
            conn.user.clone()
        } else if let Some(user) = &self.defaults.user {
            user.clone()
        } else {
            conn.vendor.unwrap_or_default().default_user().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Connection {
        Connection {
            id: "a".into(),
            name: "lab-r630".into(),
            address: "192.0.2.10".into(),
            user: "admin".into(),
            group: "Lab".into(),
            tags: vec!["pxe".into(), "Rack3".into()],
            service_tag: "ABC1234".into(),
            model: "PowerEdge R630".into(),
            vendor: Some(Vendor::Dell),
            notes: "Spare PSU missing".into(),
            save_password: true,
        }
    }

    #[test]
    fn search_covers_all_fields() {
        let c = sample();
        for q in [
            "lab",
            "192.0.2",
            "abc1234",
            "rack3",
            "psu",
            "r630 pxe",
            "LAB admin",
        ] {
            assert!(c.matches(q), "{q} should match");
        }
        assert!(c.matches(""));
        assert!(!c.matches("r640"));
        assert!(!c.matches("lab r640"));
    }

    #[test]
    fn yaml_round_trip_and_minimal_entries() {
        let dir = std::env::temp_dir().join(format!("rustdrac-test-{}", new_id()));
        let path = dir.join("connections.yaml");
        let store = Store {
            defaults: Defaults {
                user: Some("admin".into()),
                theme: None,
                health_check_minutes: Some(0),
                screenshot_dir: None,
            },
            connections: vec![sample()],
        };
        store.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text
                .lines()
                .any(|l| l.trim_start().starts_with("password:")),
            "no password field in:\n{text}"
        );
        let loaded = Store::load_from(&path).unwrap();
        assert_eq!(loaded.connections, store.connections);

        std::fs::write(
            &path,
            "connections:\n  - address: 10.0.0.1\n  - address: 10.0.0.2\n    vendor: supermicro\n",
        )
        .unwrap();
        let loaded = Store::load_from(&path).unwrap();
        assert_eq!(loaded.connections.len(), 2);
        assert_ne!(loaded.connections[0].id, loaded.connections[1].id);
        assert_eq!(loaded.connections[0].display_name(), "10.0.0.1");
        assert_eq!(loaded.connections[0].vendor, None);
        assert_eq!(loaded.connections[1].vendor, Some(Vendor::Supermicro));
        assert_eq!(loaded.find("10.0.0.2").unwrap().address, "10.0.0.2");
        assert_eq!(loaded.user_for(&loaded.connections[0]), "root");
        assert_eq!(loaded.user_for(&loaded.connections[1]), "ADMIN");
        assert_eq!(store.user_for(&loaded.connections[1]), "admin");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn screenshot_dir_setting() {
        let mut store = Store::default();
        assert_eq!(store.screenshot_dir(), default_screenshot_dir());
        store.set_screenshot_dir(&default_screenshot_dir().display().to_string());
        assert_eq!(store.defaults.screenshot_dir, None);
        store.set_screenshot_dir("  ");
        assert_eq!(store.defaults.screenshot_dir, None);

        let home = home_dir();
        store.set_screenshot_dir(&home.join("Shots").display().to_string());
        assert_eq!(
            store.defaults.screenshot_dir.as_deref(),
            Some(format!("~{}Shots", std::path::MAIN_SEPARATOR).as_str())
        );
        assert_eq!(store.screenshot_dir(), home.join("Shots"));
        store.set_screenshot_dir("~/Other ");
        assert_eq!(store.screenshot_dir(), home.join("Other"));
        store.set_screenshot_dir("/srv/shots");
        assert_eq!(store.defaults.screenshot_dir.as_deref(), Some("/srv/shots"));
    }

    /// Touches the real desktop keyring: `cargo test -- --ignored keyring`.
    #[test]
    #[ignore]
    fn keyring_round_trip() {
        let c = Connection {
            id: format!("test-{}", new_id()),
            save_password: true,
            ..sample()
        };
        c.set_saved_password("s3cret").unwrap();
        assert_eq!(c.saved_password().as_deref(), Some("s3cret"));
        c.forget_password();
        assert_eq!(c.saved_password(), None);
    }
}
