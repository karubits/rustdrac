//! Health, sensors and the System Event Log (SEL) over Redfish.
//!
//! Redfish uses HTTP basic auth per request, so none of this takes one of the BMC's few
//! web-session slots (or disturbs an open console). The exception is a MegaRAC, where every
//! basic-auth request by a PAM (IPMI) user counts as a login and the fifth within a minute is
//! refused: there one Redfish session per host and user serves the whole run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::Value;

use crate::bmc::{self, Vendor};

/// Where a vendor keeps things, and which query options its firmware accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paths {
    pub system: &'static str,
    pub chassis: &'static str,
    pub manager: &'static str,
    pub sel_service: &'static str,
    /// The entries collection, when known (saves a slow request on an iDRAC 8).
    pub sel_entries: Option<&'static str>,
    /// The BMC's own log (Dell's Lifecycle log, Supermicro's maintenance log).
    pub aux_log_service: &'static str,
    pub aux_log_entries: Option<&'static str>,
    /// `?$expand=*($levels=1)` works. Supermicro answers any query string with a redirect
    /// to a mangled URL and then HTTP 501.
    pub expand: bool,
    /// Page by `$skip` when a page has no `nextLink` (iDRAC 8). Supermicro returns the whole
    /// collection at once and ignores `$skip`, which would loop.
    pub skip_paging: bool,
}

const DELL: Paths = Paths {
    system: "/redfish/v1/Systems/System.Embedded.1",
    chassis: "/redfish/v1/Chassis/System.Embedded.1",
    manager: "/redfish/v1/Managers/iDRAC.Embedded.1",
    sel_service: "/redfish/v1/Managers/iDRAC.Embedded.1/LogServices/Sel",
    sel_entries: Some("/redfish/v1/Managers/iDRAC.Embedded.1/Logs/Sel"),
    aux_log_service: "/redfish/v1/Managers/iDRAC.Embedded.1/LogServices/Lclog",
    aux_log_entries: Some("/redfish/v1/Managers/iDRAC.Embedded.1/Logs/Lclog"),
    expand: true,
    skip_paging: true,
};

const SUPERMICRO: Paths = Paths {
    system: "/redfish/v1/Systems/1",
    chassis: "/redfish/v1/Chassis/1",
    manager: "/redfish/v1/Managers/1",
    sel_service: "/redfish/v1/Systems/1/LogServices/Log1",
    sel_entries: Some("/redfish/v1/Systems/1/LogServices/Log1/Entries"),
    aux_log_service: "/redfish/v1/Managers/1/LogServices/Log1",
    aux_log_entries: Some("/redfish/v1/Managers/1/LogServices/Log1/Entries"),
    expand: false,
    skip_paging: false,
};

/// AMI MegaRAC SP-X (Gigabyte, tested on firmware 12.61): the `Self` singletons; `$expand`
/// works and SEL pages carry a `nextLink`. The audit log stands in for a lifecycle log.
const MEGARAC: Paths = Paths {
    system: "/redfish/v1/Systems/Self",
    chassis: "/redfish/v1/Chassis/Self",
    manager: "/redfish/v1/Managers/Self",
    sel_service: "/redfish/v1/Managers/Self/LogServices/SEL",
    sel_entries: Some("/redfish/v1/Managers/Self/LogServices/SEL/Entries"),
    aux_log_service: "/redfish/v1/Managers/Self/LogServices/AuditLog",
    aux_log_entries: Some("/redfish/v1/Managers/Self/LogServices/AuditLog/Entries"),
    expand: true,
    skip_paging: false,
};

pub fn paths(vendor: Vendor) -> &'static Paths {
    match vendor {
        Vendor::Dell => &DELL,
        Vendor::Supermicro => &SUPERMICRO,
        Vendor::MegaRac => &MEGARAC,
    }
}

/// Redfish health, ordered from best to worst so `max()` gives the overall state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Health {
    Ok,
    Unknown,
    Warning,
    Critical,
}

impl Health {
    pub fn parse(s: Option<&str>) -> Health {
        match s.map(str::to_ascii_lowercase).as_deref() {
            Some("ok") => Health::Ok,
            Some("warning") => Health::Warning,
            Some("critical") => Health::Critical,
            _ => Health::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Health::Ok => "OK",
            Health::Unknown => "Unknown",
            Health::Warning => "Warning",
            Health::Critical => "Critical",
        }
    }

    pub fn is_alarm(self) -> bool {
        matches!(self, Health::Warning | Health::Critical)
    }
}

/// One component or sensor with a Redfish `Status`.
#[derive(Debug, Clone)]
pub struct Sensor {
    pub category: &'static str,
    pub name: String,
    pub health: Health,
    pub reading: String,
}

#[derive(Debug, Clone)]
pub struct HealthReport {
    pub overall: Health,
    pub power_state: String,
    pub sensors: Vec<Sensor>,
}

impl HealthReport {
    /// Merge one step's sensors (or its error) and update the overall state.
    pub fn add(&mut self, step: Step, sensors: Result<Vec<Sensor>>) {
        match sensors {
            Ok(sensors) => self.add_sensors(sensors),
            Err(e) => self.add_sensors(vec![Sensor {
                category: "Redfish",
                name: format!("could not read {}: {e:#}", step.label()),
                health: Health::Unknown,
                reading: String::new(),
            }]),
        }
    }

    pub fn add_sensors(&mut self, sensors: Vec<Sensor>) {
        self.sensors.extend(sensors);
        // An unreadable sensor never outranks a real reading.
        self.overall = self
            .sensors
            .iter()
            .map(|s| s.health)
            .chain([self.overall])
            .filter(|h| *h != Health::Unknown)
            .max()
            .unwrap_or(Health::Unknown);
    }

    /// Components that are in Warning or Critical state, worst first.
    pub fn alarms(&self) -> Vec<&Sensor> {
        let mut alarms: Vec<&Sensor> = self
            .sensors
            .iter()
            .filter(|s| s.health.is_alarm())
            .collect();
        alarms.sort_by_key(|a| std::cmp::Reverse(a.health));
        alarms
    }

    /// "Critical: PS2; Warning: Fan3A" style summary.
    pub fn summary(&self) -> String {
        let alarms = self.alarms();
        if alarms.is_empty() {
            return format!("{} · power {}", self.overall.label(), self.power_state);
        }
        alarms
            .iter()
            .map(|s| format!("{} {}: {}", s.health.label(), s.category, s.name))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelEntry {
    pub id: u32,
    pub created: String,
    pub severity: Health,
    pub message: String,
    pub sensor: String,
    /// "Assert" (the condition started) or "Deassert" (it cleared).
    pub event: String,
}

enum GetError {
    /// Worth retrying (connection trouble, 5xx, a garbled reply).
    Transient(anyhow::Error),
    Fatal(anyhow::Error),
}

impl GetError {
    fn into_error(self) -> anyhow::Error {
        match self {
            GetError::Transient(e) | GetError::Fatal(e) => e,
        }
    }
}

/// Redfish session tokens by (host, user), for BMCs that need session auth (see the module
/// doc). Ended by [`end_sessions`] at exit; the BMC expires them anyway.
struct Token {
    value: String,
    /// Where to `DELETE` it.
    location: String,
    password: String,
}

fn tokens() -> &'static Mutex<HashMap<(String, String), Token>> {
    static TOKENS: OnceLock<Mutex<HashMap<(String, String), Token>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// Delete every Redfish session this process opened (best effort, at exit).
pub fn end_sessions(client: &Client) {
    let tokens: Vec<Token> = tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, t)| t)
        .collect();
    for t in tokens {
        let _ = client
            .delete(&t.location)
            .header("X-Auth-Token", &t.value)
            .timeout(Duration::from_secs(5))
            .send();
    }
}

/// One lock per BMC host, shared by every Redfish client in the process.
fn host_lock(host: &str) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(host.to_lowercase())
        .or_default()
        .clone()
}

/// A Redfish client for one BMC. Cheap to clone; clones share the HTTP connection pool,
/// which matters on an iDRAC 8: a TLS handshake there takes seconds.
#[derive(Clone)]
pub struct Redfish {
    client: Client,
    host: String,
    user: String,
    password: String,
    /// Known up front (from the address book), or detected on first use.
    vendor: Option<Vendor>,
    /// Per-request timeout.
    timeout: Duration,
}

/// One step of reading the full health, so the UI can show results as they arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Power,
    Thermal,
    Storage,
}

impl Step {
    pub const ALL: [Step; 3] = [Step::Power, Step::Thermal, Step::Storage];

    pub fn label(self) -> &'static str {
        match self {
            Step::Power => "power",
            Step::Thermal => "fans and temperatures",
            Step::Storage => "storage",
        }
    }
}

impl Redfish {
    pub fn new(client: &Client, host: &str, user: &str, password: &str) -> Self {
        Redfish {
            client: client.clone(),
            host: host.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            vendor: None,
            timeout: Duration::from_secs(60),
        }
    }

    /// Give up on a request after `timeout` (default 60 s; an iDRAC 8 can take seconds).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Use the saved vendor instead of probing the host (`None` keeps probing).
    pub fn with_vendor(mut self, vendor: Option<Vendor>) -> Self {
        self.vendor = vendor;
        self
    }

    /// The vendor given to [`Redfish::with_vendor`], else probed (once per host per run).
    pub fn vendor(&self) -> Result<Vendor> {
        match self.vendor {
            Some(v) => Ok(v),
            None => bmc::vendor(&self.client, &self.host),
        }
    }

    pub fn paths(&self) -> Result<&'static Paths> {
        Ok(paths(self.vendor()?))
    }

    /// GET a Redfish resource. Requests to one BMC are serialised: an iDRAC 8 handles
    /// one at a time anyway and answers concurrent ones with errors or garbage. Transient
    /// failures are retried.
    fn get(&self, path: &str) -> Result<Value> {
        let lock = host_lock(&self.host);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.get_once(path) {
                Ok(v) => return Ok(v),
                Err(GetError::Fatal(e)) => return Err(e),
                Err(GetError::Transient(e)) if attempt >= 3 => return Err(e),
                Err(GetError::Transient(e)) => {
                    eprintln!("rustdrac: {}: {e:#}; retrying", self.host);
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    /// Whether requests carry a session token instead of basic auth (MegaRAC, see the
    /// module doc). Unknown vendors keep basic auth.
    fn uses_sessions(&self) -> bool {
        self.vendor().ok() == Some(Vendor::MegaRac)
    }

    fn token_key(&self) -> (String, String) {
        (self.host.to_lowercase(), self.user.clone())
    }

    /// The session token for this host and user: the cached one, or a new login. The flag
    /// says whether it was just created (so a 401 with it is a rejected login, not an
    /// expired session).
    fn token(&self) -> Result<(String, bool), GetError> {
        let key = self.token_key();
        if let Some(t) = tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .filter(|t| t.password == self.password)
        {
            return Ok((t.value.clone(), false));
        }
        let resp = self
            .client
            .post(format!(
                "https://{}/redfish/v1/SessionService/Sessions",
                self.host
            ))
            .json(&serde_json::json!({ "UserName": self.user, "Password": self.password }))
            .timeout(self.timeout)
            .send()
            .with_context(|| format!("connecting to https://{}", self.host))
            .map_err(GetError::Transient)?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Err(GetError::Fatal(
                bmc::AuthFailed {
                    host: self.host.clone(),
                }
                .into(),
            ));
        }
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let (value, location) = (header("X-Auth-Token"), header("Location"));
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(GetError::Transient(anyhow::anyhow!(
                "{}: Redfish login refused: {}",
                self.host,
                redfish_error(status, &body)
            )));
        }
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            return Err(GetError::Fatal(anyhow::anyhow!(
                "{}: Redfish login returned no session token",
                self.host
            )));
        };
        let location = match location {
            Some(l) if l.starts_with('/') => format!("https://{}{l}", self.host),
            Some(l) => l,
            None => String::new(),
        };
        tokens().lock().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            Token {
                value: value.clone(),
                location,
                password: self.password.clone(),
            },
        );
        Ok((value, true))
    }

    fn forget_token(&self) {
        tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token_key());
    }

    /// Send `build()`'s request with the right credentials. A 401 is a rejected login with
    /// basic auth or a fresh token; with a cached token it means the session expired, so the
    /// token is dropped and the request repeated once with a new one.
    fn send(
        &self,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response, GetError> {
        loop {
            let (req, fresh) = if self.uses_sessions() {
                let (token, fresh) = self.token()?;
                (build().header("X-Auth-Token", token), fresh)
            } else {
                (build().basic_auth(&self.user, Some(&self.password)), true)
            };
            let resp = req
                .timeout(self.timeout)
                .send()
                .with_context(|| format!("connecting to https://{}", self.host))
                .map_err(GetError::Transient)?;
            if resp.status().as_u16() != 401 {
                return Ok(resp);
            }
            if !fresh {
                self.forget_token();
                continue;
            }
            return Err(GetError::Fatal(
                bmc::AuthFailed {
                    host: self.host.clone(),
                }
                .into(),
            ));
        }
    }

    fn get_once(&self, path: &str) -> Result<Value, GetError> {
        let resp = self.send(|| self.client.get(format!("https://{}{path}", self.host)))?;
        let status = resp.status();
        match status.as_u16() {
            403 => {
                return Err(GetError::Fatal(anyhow::anyhow!(
                    "{}: user {} may not use Redfish",
                    self.host,
                    self.user
                )));
            }
            404 => return Err(GetError::Fatal(anyhow::anyhow!("GET {path}: not found"))),
            _ => {}
        }
        let body = resp
            .text()
            .with_context(|| format!("reading {path}"))
            .map_err(GetError::Transient)?;
        if crate::idrac::host_header_rejected(status, &body) {
            return Err(GetError::Fatal(
                crate::idrac::HostHeaderRejected {
                    host: self.host.clone(),
                }
                .into(),
            ));
        }
        if !status.is_success() {
            return Err(GetError::Transient(anyhow::anyhow!(
                "GET {path}: HTTP {status}"
            )));
        }
        serde_json::from_str(&body).map_err(|e| {
            let snippet: String = body.chars().take(80).collect();
            GetError::Transient(anyhow::anyhow!("GET {path}: not JSON ({e}): {snippet:?}"))
        })
    }

    /// Cheap alarm check: one request for the system's health rollup, plus component
    /// detail only when something is not OK (to say what).
    pub fn check(&self) -> Result<HealthReport> {
        let mut report = self.system()?;
        if report.overall != Health::Ok {
            for step in Step::ALL {
                let sensors = self.step(step).map(|(sensors, _)| sensors);
                report.add(step, sensors);
            }
            // The rollup can be Critical while every component reads OK (e.g. a CPU machine
            // check); the SEL says why.
            if let Ok(sel) = self.sel() {
                report.add_sensors(sel_alarms(&sel));
            }
        }
        Ok(report)
    }

    /// Every component and sensor, read in one go (the Health tab reads the same steps
    /// one by one to show progress).
    pub fn full_health(&self) -> Result<HealthReport> {
        let mut report = self.system()?;
        for step in Step::ALL {
            match self.step(step) {
                Ok((sensors, drives)) => {
                    report.add_sensors(sensors);
                    for drive in drives {
                        match self.drive(&drive) {
                            Ok(Some(sensor)) => report.add_sensors(vec![sensor]),
                            Ok(None) => {}
                            Err(e) => report.add(step, Err(e)),
                        }
                    }
                }
                Err(e) => report.add(step, Err(e)),
            }
        }
        Ok(report)
    }

    /// Overall system, processor and memory health (one request).
    pub fn system(&self) -> Result<HealthReport> {
        Ok(system_report(&self.get(self.paths()?.system)?))
    }

    /// Sensors of one step; for storage also the drives to read with [`Redfish::drive`].
    pub fn step(&self, step: Step) -> Result<(Vec<Sensor>, Vec<String>)> {
        let chassis = self.paths()?.chassis;
        match step {
            Step::Power => Ok((
                power_sensors(&self.get(&format!("{chassis}/Power"))?),
                vec![],
            )),
            Step::Thermal => Ok((
                thermal_sensors(&self.get(&format!("{chassis}/Thermal"))?),
                vec![],
            )),
            Step::Storage => self.storage(),
        }
    }

    fn storage(&self) -> Result<(Vec<Sensor>, Vec<String>)> {
        let storage = self.get(&format!("{}/Storage", self.paths()?.system))?;
        let mut sensors = Vec::new();
        let mut drives = Vec::new();
        for member in members(&storage) {
            let controller = self.get(member)?;
            let count = controller["Drives"].as_array().map(Vec::len).unwrap_or(0);
            if let Some(health) = status_health(&controller) {
                sensors.push(Sensor {
                    category: "Storage",
                    name: str_field(&controller, "Name"),
                    health,
                    reading: format!("{count} drive(s)"),
                });
            }
            drives.extend(
                members_of(&controller["Drives"])
                    .into_iter()
                    .map(str::to_string),
            );
        }
        Ok((sensors, drives))
    }

    pub fn drive(&self, path: &str) -> Result<Option<Sensor>> {
        let drive = self.get(path)?;
        let Some(health) = status_health(&drive) else {
            return Ok(None);
        };
        let size = drive["CapacityBytes"]
            .as_f64()
            .map(|b| format!("{:.0} GB", b / 1e9))
            .unwrap_or_default();
        let media = str_field(&drive, "MediaType");
        let reading = [media, size]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        Ok(Some(Sensor {
            category: "Drive",
            name: str_field(&drive, "Name"),
            health,
            reading,
        }))
    }

    /// The whole SEL, newest first.
    pub fn sel(&self) -> Result<Vec<SelEntry>> {
        let p = self.paths()?;
        let raw = self.log_members(p.sel_entries, p.sel_service, usize::MAX)?;
        let mut entries: Vec<SelEntry> = raw.iter().map(sel_entry).collect();
        entries.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        // Pages can overlap if entries are logged while paging.
        entries.dedup_by_key(|e| e.id);
        Ok(entries)
    }

    /// Up to `limit` entries of the BMC's own log (see [`Vendor::aux_log_name`]), newest
    /// first as the BMC returns them.
    pub fn aux_log(&self, limit: usize) -> Result<Vec<Value>> {
        let p = self.paths()?;
        self.log_members(p.aux_log_entries, p.aux_log_service, limit)
    }

    /// Raw members of a log's entry collection, following the BMC's paging, up to `limit`.
    fn log_members(
        &self,
        known_entries: Option<&str>,
        service: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let skip_paging = self.paths()?.skip_paging;
        // Known location first (saves a slow request); otherwise ask the log service.
        let known = known_entries.and_then(|path| self.get(path).ok().map(|page| (path, page)));
        let (base, mut page) = match known {
            Some((path, page)) => (path.to_string(), page),
            None => {
                let service_doc = self.get(service)?;
                let base = service_doc["Entries"]["@odata.id"]
                    .as_str()
                    .with_context(|| format!("{service} has no Entries link"))?
                    .to_string();
                let page = self.get(&base)?;
                (base, page)
            }
        };
        let mut entries: Vec<Value> = Vec::new();
        let mut current = base.clone();
        loop {
            let members = page["Members"].as_array().cloned().unwrap_or_default();
            let got = members.len();
            entries.extend(members);
            if entries.len() >= limit {
                entries.truncate(limit);
                break;
            }
            let total = page["Members@odata.count"].as_u64().unwrap_or(0) as usize;
            // An iDRAC 8 pages by $skip and leaves nextLink empty, so page by count as well.
            let next = match page["Members@odata.nextLink"].as_str().map(str::trim) {
                Some(link) if !link.is_empty() && link != current => link.to_string(),
                _ if skip_paging && got > 0 && entries.len() < total => {
                    format!("{base}?$skip={}", entries.len())
                }
                _ => break,
            };
            current = next;
            page = self.get(&current)?;
        }
        Ok(entries)
    }

    /// Any Redfish resource, for callers that read fields themselves.
    pub fn json(&self, path: &str) -> Result<Value> {
        self.get(path)
    }

    /// The members of a collection, expanded. Asks for `$expand` (one request) where the
    /// firmware supports it, and fetches each member where it returns links only.
    pub fn collection(&self, path: &str) -> Result<Vec<Value>> {
        let expanded = self
            .paths()?
            .expand
            .then(|| self.get(&format!("{path}?$expand=*($levels=1)")).ok())
            .flatten();
        let doc = match expanded {
            Some(doc) => doc,
            None => self.get(path)?,
        };
        let mut out = Vec::new();
        for member in doc["Members"].as_array().into_iter().flatten() {
            let is_link = member.as_object().is_some_and(|o| o.len() == 1);
            match member["@odata.id"].as_str() {
                Some(link) if is_link => out.push(self.get(link)?),
                _ => out.push(member.clone()),
            }
        }
        Ok(out)
    }

    /// The power state and reset types. A MegaRAC lists its reset types only in the
    /// action's `ActionInfo` document, so that is read when the system leaves them out.
    pub fn power(&self) -> Result<Power> {
        let system = self.get(self.paths()?.system)?;
        let mut power = power_of(&system);
        if power.reset_types.is_empty()
            && let Some(info) = reset_action_info_link(&system)
            && let Ok(doc) = self.get(info)
        {
            power.reset_types = reset_types_from_action_info(&doc);
        }
        Ok(power)
    }

    /// `ComputerSystem.Reset` with `reset_type` ("On", "ForceOff", "GracefulShutdown",
    /// "ForceRestart", "GracefulRestart", "PowerCycle", …).
    pub fn reset(&self, reset_type: &str) -> Result<()> {
        let lock = host_lock(&self.host);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let url = format!(
            "https://{}{}/Actions/ComputerSystem.Reset",
            self.host,
            self.paths()?.system
        );
        let resp = self
            .send(|| {
                self.client
                    .post(&url)
                    .json(&serde_json::json!({ "ResetType": reset_type }))
            })
            .map_err(GetError::into_error)
            .with_context(|| format!("{}: power action {reset_type}", self.host))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().unwrap_or_default();
        if crate::idrac::host_header_rejected(status, &body) {
            return Err(crate::idrac::HostHeaderRejected {
                host: self.host.clone(),
            }
            .into());
        }
        bail!(
            "{}: power action {reset_type} refused: {}",
            self.host,
            redfish_error(status, &body)
        );
    }

    pub fn clear_sel(&self) -> Result<()> {
        let url = format!(
            "https://{}{}/Actions/LogService.ClearLog",
            self.host,
            self.paths()?.sel_service
        );
        self.send(|| self.client.post(&url).json(&serde_json::json!({})))
            .map_err(GetError::into_error)
            .context("clearing the SEL")?
            .error_for_status()
            .context("clearing the SEL")?;
        Ok(())
    }
}

/// The system's power state and the reset types its BMC accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Power {
    /// "On", "Off", "PoweringOn", "PoweringOff" or "" when unknown.
    pub state: String,
    /// `ResetType@Redfish.AllowableValues`; empty when the BMC does not say.
    pub reset_types: Vec<String>,
}

impl Power {
    pub fn allows(&self, reset_type: &str) -> bool {
        self.reset_types.is_empty() || self.reset_types.iter().any(|t| t == reset_type)
    }
}

/// [`Power`] from a Redfish `ComputerSystem` document.
pub fn power_of(system: &Value) -> Power {
    Power {
        state: str_field(system, "PowerState"),
        reset_types:
            system["Actions"]["#ComputerSystem.Reset"]["ResetType@Redfish.AllowableValues"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect(),
    }
}

/// The `@Redfish.ActionInfo` link of `ComputerSystem.Reset`, when the system gives one.
pub fn reset_action_info_link(system: &Value) -> Option<&str> {
    system["Actions"]["#ComputerSystem.Reset"]["@Redfish.ActionInfo"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
}

/// The `ResetType` values an `ActionInfo` document allows.
pub fn reset_types_from_action_info(info: &Value) -> Vec<String> {
    info["Parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["Name"].as_str() == Some("ResetType"))
        .flat_map(|p| p["AllowableValues"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The message of a Redfish error reply, if it has one, else the HTTP status.
fn redfish_error(status: reqwest::StatusCode, body: &str) -> String {
    let doc: Value = serde_json::from_str(body).unwrap_or_default();
    let info = &doc["error"]["@Message.ExtendedInfo"][0];
    let msg = str_field(info, "Message");
    if msg.is_empty() {
        format!("HTTP {status}")
    } else {
        msg
    }
}

pub fn str_field(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().trim().to_string()
}

fn members(v: &Value) -> Vec<&str> {
    members_of(&v["Members"])
}

fn members_of(v: &Value) -> Vec<&str> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["@odata.id"].as_str())
        .filter(|id| !id.trim().is_empty())
        .collect()
}

/// Health of an enabled component; `None` for absent or disabled ones (empty PSU slots…)
/// and for ones that report no health at all (e.g. an unused chipset SATA controller).
fn status_health(v: &Value) -> Option<Health> {
    let status = &v["Status"];
    match status["State"].as_str() {
        Some("Absent") | Some("Disabled") | Some("UnavailableOffline") => None,
        _ => status["HealthRollup"]
            .as_str()
            .or_else(|| status["Health"].as_str())
            .map(|h| Health::parse(Some(h))),
    }
}

fn system_report(system: &Value) -> HealthReport {
    let mut sensors = Vec::new();
    for (category, value, reading) in [
        ("System", system, str_field(system, "Model")),
        (
            "Processors",
            &system["ProcessorSummary"],
            format!(
                "{} × {}",
                system["ProcessorSummary"]["Count"].as_u64().unwrap_or(0),
                str_field(&system["ProcessorSummary"], "Model")
            ),
        ),
        (
            "Memory",
            &system["MemorySummary"],
            system["MemorySummary"]["TotalSystemMemoryGiB"]
                .as_f64()
                .map(|g| format!("{g:.0} GiB"))
                .unwrap_or_default(),
        ),
    ] {
        if let Some(health) = status_health(value) {
            sensors.push(Sensor {
                category,
                name: category.to_string(),
                health,
                reading,
            });
        }
    }
    HealthReport {
        overall: sensors
            .iter()
            .map(|s| s.health)
            .max()
            .unwrap_or(Health::Unknown),
        power_state: str_field(system, "PowerState"),
        sensors,
    }
}

fn power_sensors(power: &Value) -> Vec<Sensor> {
    let mut sensors = Vec::new();
    for psu in power["PowerSupplies"].as_array().into_iter().flatten() {
        if let Some(health) = status_health(psu) {
            // Supermicro reports no capacity, but the current output.
            let watts = psu["PowerCapacityWatts"]
                .as_f64()
                .map(|w| format!("{w:.0} W capacity"))
                .or_else(|| {
                    psu["PowerOutputWatts"]
                        .as_f64()
                        .map(|w| format!("{w:.0} W output"))
                })
                .unwrap_or_default();
            // AMI lists PSUs without a name, only a member id.
            let mut name = str_field(psu, "Name");
            if name.is_empty() {
                name = format!("PSU {}", str_field(psu, "MemberId"));
            }
            sensors.push(Sensor {
                category: "Power Supply",
                name,
                health,
                reading: watts,
            });
        }
    }
    for v in power["Voltages"].as_array().into_iter().flatten() {
        if let Some(health) = status_health(v) {
            let reading = match v["ReadingVolts"].as_f64() {
                // Dell reports "PG" (power good) sensors as 1/0.
                Some(r) if str_field(v, "Name").ends_with(" PG") => {
                    if r >= 1.0 {
                        "good".into()
                    } else {
                        "fail".into()
                    }
                }
                Some(r) => format!("{r:.2} V"),
                None => String::new(),
            };
            sensors.push(Sensor {
                category: "Voltage",
                name: str_field(v, "Name"),
                health,
                reading,
            });
        }
    }
    for c in power["PowerControl"].as_array().into_iter().flatten() {
        if let Some(w) = c["PowerConsumedWatts"].as_f64() {
            sensors.push(Sensor {
                category: "Power",
                name: str_field(c, "Name"),
                health: status_health(c).unwrap_or(Health::Ok),
                reading: format!("{w:.0} W"),
            });
        }
    }
    sensors
}

fn thermal_sensors(thermal: &Value) -> Vec<Sensor> {
    let mut sensors = Vec::new();
    for fan in thermal["Fans"].as_array().into_iter().flatten() {
        if let Some(health) = status_health(fan) {
            let name = fan["FanName"]
                .as_str()
                .or_else(|| fan["Name"].as_str())
                .unwrap_or_default();
            let reading = fan["Reading"]
                .as_f64()
                .map(|r| format!("{r:.0} {}", fan["ReadingUnits"].as_str().unwrap_or("RPM")))
                .unwrap_or_default();
            sensors.push(Sensor {
                category: "Fan",
                name: name.trim().to_string(),
                health,
                reading,
            });
        }
    }
    for t in thermal["Temperatures"].as_array().into_iter().flatten() {
        if let Some(health) = status_health(t) {
            let mut reading = t["ReadingCelsius"]
                .as_f64()
                .map(|r| format!("{r:.0} °C"))
                .unwrap_or_default();
            if let Some(crit) = t["UpperThresholdCritical"].as_f64() {
                reading += &format!(" (critical at {crit:.0} °C)");
            }
            sensors.push(Sensor {
                category: "Temperature",
                name: str_field(t, "Name"),
                health,
                reading,
            });
        }
    }
    sensors
}

fn sel_entry(e: &Value) -> SelEntry {
    let first_member = |key: &str| {
        e[key]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|m| m["Member"].as_str())
            .or_else(|| e[key].as_str())
            .unwrap_or_default()
            .to_string()
    };
    // Dell names the sensor type; Supermicro has an OEM type plus the raw IPMI sensor name
    // (often just "OEM").
    let mut sensor = first_member("SensorType");
    if sensor.is_empty() {
        let raw = str_field(&e["Oem"]["Supermicro"]["RawEventData"], "SensorName");
        sensor = if raw.is_empty() || raw == "OEM" {
            str_field(e, "OemSensorType")
        } else {
            raw
        };
    }
    // AMI (MegaRAC) dumps the raw IPMI record into the message: pick out the event and the
    // sensor name instead.
    let raw = str_field(e, "Message");
    let mut message = raw.clone();
    if let Some(fields) = ipmi_dump_fields(&raw) {
        if let Some(event_type) = fields.get("Event_Type").filter(|s| !s.is_empty()) {
            message = event_type.to_string();
        }
        if let Some(name) = fields.get("Sensor_Name").filter(|s| !s.is_empty()) {
            sensor = name.to_string();
        }
    }
    SelEntry {
        id: e["Id"]
            .as_str()
            .and_then(|s| s.trim().parse().ok())
            .or_else(|| e["Id"].as_u64().map(|n| n as u32))
            .unwrap_or(0),
        created: str_field(e, "Created"),
        severity: Health::parse(e["Severity"].as_str()),
        message,
        sensor,
        event: first_member("EntryCode"),
    }
}

/// The `key : value` pairs of an AMI SEL message such as
/// `"Event_Data_1 : 160, Record_Type : system event record, …, Event_Type : Correctable ECC,
/// Sensor_Type : Memory, , Sensor_Name : DIMM_P0_B0"`; `None` for any other message.
fn ipmi_dump_fields(message: &str) -> Option<HashMap<&str, &str>> {
    if !message.contains("Event_Type : ") || !message.contains("Record_Type : ") {
        return None;
    }
    Some(
        message
            .split(',')
            .filter_map(|part| part.split_once(" : "))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect(),
    )
}

/// The newest Warning/Critical SEL events, as sensors for an alarm summary.
pub fn sel_alarms(entries: &[SelEntry]) -> Vec<Sensor> {
    entries
        .iter()
        .filter(|e| e.severity.is_alarm() && e.event != "Deassert")
        .take(3)
        .map(|e| Sensor {
            category: "SEL",
            name: e.message.clone(),
            health: e.severity,
            reading: e.created.clone(),
        })
        .collect()
}

/// CSV of the SEL, for export.
pub fn sel_csv(entries: &[SelEntry]) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("Id,Created,Severity,Event,Sensor,Message\n");
    for e in entries {
        out += &format!(
            "{},{},{},{},{},{}\n",
            e.id,
            quote(&e.created),
            e.severity.label(),
            quote(&e.event),
            quote(&e.sensor),
            quote(&e.message)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn megarac_paths_are_the_known_ones() {
        let p = super::paths(super::Vendor::MegaRac);
        assert_eq!(p.system, "/redfish/v1/Systems/Self");
        assert_eq!(
            p.sel_entries,
            Some("/redfish/v1/Managers/Self/LogServices/SEL/Entries")
        );
        assert!(p.expand);
        assert!(!p.skip_paging);
    }

    #[test]
    fn reset_types_come_from_action_info() {
        let system: serde_json::Value = serde_json::from_str(
            r##"{"Actions":{"#ComputerSystem.Reset":{"@Redfish.ActionInfo":"/redfish/v1/Systems/Self/ResetActionInfo","target":"/redfish/v1/Systems/Self/Actions/ComputerSystem.Reset"}},"PowerState":"On"}"##,
        )
        .unwrap();
        assert!(super::power_of(&system).reset_types.is_empty());
        assert_eq!(
            super::reset_action_info_link(&system),
            Some("/redfish/v1/Systems/Self/ResetActionInfo")
        );
        let info: serde_json::Value = serde_json::from_str(
            r#"{"Id":"ResetAction","Parameters":[{"AllowableValues":["GracefulShutdown","ForceRestart","On","ForceOff"],"DataType":"String","Name":"ResetType","Required":true}]}"#,
        )
        .unwrap();
        assert_eq!(
            super::reset_types_from_action_info(&info),
            ["GracefulShutdown", "ForceRestart", "On", "ForceOff"]
        );
        assert_eq!(
            super::reset_action_info_link(&serde_json::Value::Null),
            None
        );
    }

    #[test]
    fn ami_sel_messages_are_decoded() {
        let e: serde_json::Value = serde_json::from_str(
            r#"{"Created":"2024-07-08T02:41:19Z","EntryCode":"Assert","Id":"4","Message":"Event_Data_1 : 160, Record_Type : system event record, Sensor_Number : 8, Event_Dir : Assertion event, Event_Data_2 : 0, Timestamp : 2024-07-08T02:41:17+00:00, EvM_Rev : IPMI v2.0 Event Messages, Generator_ID : 0x2100, Event_Data_3 : 16, Event_Type : Correctable ECC, Record_ID : 4, Sensor_Type : Memory, , Sensor_Name : DIMM_P0_B0","SensorType":"Memory","Severity":"Warning"}"#,
        )
        .unwrap();
        let entry = super::sel_entry(&e);
        assert_eq!(entry.id, 4);
        assert_eq!(entry.message, "Correctable ECC");
        assert_eq!(entry.sensor, "DIMM_P0_B0");
        assert_eq!(entry.event, "Assert");
        assert_eq!(entry.severity, super::Health::Warning);
        // Without a sensor name the sensor type stays.
        let e2 = serde_json::json!({"Id": "1", "Message": "Record_Type : system event record, Event_Type : Log Area Reset/Cleared, Sensor_Type : Event Logging Disabled, ", "SensorType": "Event Logging Disabled", "Severity": "OK"});
        let entry = super::sel_entry(&e2);
        assert_eq!(entry.message, "Log Area Reset/Cleared");
        assert_eq!(entry.sensor, "Event Logging Disabled");
        // Other vendors' messages pass through.
        let dell = serde_json::json!({"Id": "7", "Message": "CPU 1 machine check error detected.", "SensorType": "Processor", "Severity": "Critical"});
        assert_eq!(
            super::sel_entry(&dell).message,
            "CPU 1 machine check error detected."
        );
        assert!(super::ipmi_dump_fields("a, b : c").is_none());
    }

    /// Against a real, unused server: graceful shutdown (force off if that takes too
    /// long), then power on, reading the state back the way the console tab does.
    /// `RUSTDRAC_TEST_HOST=… RUSTDRAC_TEST_USER=… RUSTDRAC_TEST_PASSWORD=… cargo test -- --ignored power_off --nocapture`
    #[test]
    #[ignore]
    fn power_off_and_on_a_real_server() {
        use super::*;
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let client = crate::bmc::http_client().unwrap();
        let rf = Redfish::new(
            &client,
            &var("RUSTDRAC_TEST_HOST"),
            &var("RUSTDRAC_TEST_USER"),
            &var("RUSTDRAC_TEST_PASSWORD"),
        );
        let started = std::time::Instant::now();
        let log = |msg: &str| eprintln!("[{:>5.1}s] {msg}", started.elapsed().as_secs_f32());
        let wait_for = |state: &str, secs: u64| -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(5));
                let p = rf.power().unwrap();
                log(&format!("state {}", p.state));
                if p.state == state {
                    return true;
                }
            }
            false
        };
        let p = rf.power().unwrap();
        log(&format!("state {}; allowed {:?}", p.state, p.reset_types));
        assert_eq!(p.state, "On", "the server must be on to start");
        rf.reset("GracefulShutdown").unwrap();
        log("GracefulShutdown accepted");
        if !wait_for("Off", 180) {
            rf.reset("ForceOff").unwrap();
            log("ForceOff accepted");
            assert!(wait_for("Off", 60));
        }
        rf.reset("On").unwrap();
        log("On accepted");
        assert!(wait_for("On", 90));
        // The console tab's redundant-action message.
        let e = rf.reset("On").unwrap_err();
        log(&format!("second On refused: {e:#}"));
    }

    #[test]
    fn power_info_and_errors_are_parsed() {
        use super::*;
        let sys = serde_json::json!({"PowerState": "On", "Actions": {"#ComputerSystem.Reset": {
            "ResetType@Redfish.AllowableValues": ["On", "ForceOff", "GracefulRestart"]}}});
        let p = power_of(&sys);
        assert_eq!(p.state, "On");
        assert!(p.allows("ForceOff"));
        assert!(!p.allows("PowerCycle"));
        assert!(Power::default().allows("PowerCycle"));
        let body =
            r#"{"error":{"@Message.ExtendedInfo":[{"Message":"Server is already powered ON."}]}}"#;
        assert_eq!(
            redfish_error(reqwest::StatusCode::CONFLICT, body),
            "Server is already powered ON."
        );
        assert_eq!(
            redfish_error(reqwest::StatusCode::CONFLICT, "nope"),
            "HTTP 409 Conflict"
        );
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sel_entries_like_idrac8() {
        let e = json!({
            "Created": "2026-10-30T02:03:59+09:00",
            "EntryCode": [{"Member": "Assert"}],
            "Id": "2",
            "Message": "The chassis is open while the power is off.",
            "SensorType": [{"Member": "Physical Chassis Security"}],
            "Severity": "Critical"
        });
        assert_eq!(
            sel_entry(&e),
            SelEntry {
                id: 2,
                created: "2026-10-30T02:03:59+09:00".into(),
                severity: Health::Critical,
                message: "The chassis is open while the power is off.".into(),
                sensor: "Physical Chassis Security".into(),
                event: "Assert".into(),
            }
        );
        let csv = sel_csv(&[sel_entry(&e)]);
        assert!(
            csv.lines()
                .nth(1)
                .unwrap()
                .starts_with("2,\"2026-10-30T02:03:59+09:00\",Critical,")
        );
    }

    #[test]
    fn parses_sel_entries_like_supermicro_x11() {
        let e = json!({
            "@odata.id": "/redfish/v1/Systems/1/LogServices/Log1/Entries/1",
            "Id": "1", "Name": "Health Event Log Entry 1", "EntryType": "SEL",
            "Severity": "Warning", "Created": "2018-11-27T23:01:00Z", "EntryCode": "Assert",
            "OemSensorType": "ACPowerOn", "SensorNumber": 255,
            "Message": "[ OEM ] First AC Power on", "MessageId": "0xA0FFFF",
            "Oem": {"Supermicro": {"RawEventData": {"EventDirAndType": "0x6F",
                "SensorType": "0xC8", "SensorName": "OEM", "EventData1": "0xA0"}}}
        });
        assert_eq!(
            sel_entry(&e),
            SelEntry {
                id: 1,
                created: "2018-11-27T23:01:00Z".into(),
                severity: Health::Warning,
                message: "[ OEM ] First AC Power on".into(),
                sensor: "ACPowerOn".into(),
                event: "Assert".into(),
            }
        );
        let named = json!({"Id": "7", "Severity": "Critical", "EntryCode": "Assert",
            "OemSensorType": "Temperature", "Message": "CPU1 Temp upper critical",
            "Oem": {"Supermicro": {"RawEventData": {"SensorName": "CPU1 Temp"}}}});
        assert_eq!(sel_entry(&named).sensor, "CPU1 Temp");
    }

    #[test]
    fn supermicro_power_and_thermal_use_the_standard_fields() {
        let power = json!({"PowerSupplies": [
            {"Name": "Power Supply Bay 1", "Status": {"State": "Enabled", "Health": "OK"},
             "PowerCapacityWatts": null, "PowerInputWatts": 430, "PowerOutputWatts": 397,
             "Model": "PWS-2K22A-1R"}],
            "Voltages": [{"Name": "12V", "Status": {"State": "Enabled", "Health": "OK"}, "ReadingVolts": 12.305}],
            "PowerControl": [{"Name": "System Power Control", "PowerConsumedWatts": 135,
                "Status": {"State": "Enabled", "Health": "OK"}}]});
        let sensors = power_sensors(&power);
        assert_eq!(sensors[0].name, "Power Supply Bay 1");
        assert_eq!(sensors[0].reading, "397 W output");
        assert_eq!(sensors[1].reading, "12.30 V");
        assert_eq!(sensors[2].reading, "135 W");

        let thermal = json!({
            "Fans": [{"Name": "FAN1", "Status": {"State": "Enabled", "Health": "OK"},
                "ReadingUnits": "RPM", "Reading": 8700}],
            "Temperatures": [{"Name": "CPU1 Temp", "Status": {"State": "Enabled", "Health": "OK"},
                "ReadingCelsius": 50, "UpperThresholdCritical": 98}]});
        let sensors = thermal_sensors(&thermal);
        assert_eq!(
            (sensors[0].name.as_str(), sensors[0].reading.as_str()),
            ("FAN1", "8700 RPM")
        );
        assert_eq!(sensors[1].reading, "50 °C (critical at 98 °C)");

        let system = json!({"Model": "SYS-2029BT-HNTR", "PowerState": "On",
            "Status": {"State": "Enabled", "Health": "OK"},
            "ProcessorSummary": {"Count": 2, "Model": "Intel(R) Xeon(R) processor",
                "Status": {"State": "Enabled", "Health": "OK", "HealthRollup": "OK"}},
            "MemorySummary": {"TotalSystemMemoryGiB": 256,
                "Status": {"State": "Enabled", "Health": "OK", "HealthRollup": "OK"}}});
        let report = system_report(&system);
        assert_eq!(report.overall, Health::Ok);
        assert_eq!(report.sensors[2].reading, "256 GiB");
    }

    #[test]
    fn dell_paths_are_the_known_ones() {
        let p = paths(Vendor::Dell);
        assert_eq!(p.system, "/redfish/v1/Systems/System.Embedded.1");
        assert_eq!(p.chassis, "/redfish/v1/Chassis/System.Embedded.1");
        assert_eq!(p.manager, "/redfish/v1/Managers/iDRAC.Embedded.1");
        assert_eq!(
            p.sel_service,
            "/redfish/v1/Managers/iDRAC.Embedded.1/LogServices/Sel"
        );
        assert_eq!(
            p.sel_entries,
            Some("/redfish/v1/Managers/iDRAC.Embedded.1/Logs/Sel")
        );
        assert_eq!(
            p.aux_log_service,
            "/redfish/v1/Managers/iDRAC.Embedded.1/LogServices/Lclog"
        );
        assert_eq!(
            p.aux_log_entries,
            Some("/redfish/v1/Managers/iDRAC.Embedded.1/Logs/Lclog")
        );
        assert!(p.expand && p.skip_paging);
        let s = paths(Vendor::Supermicro);
        assert_eq!(s.system, "/redfish/v1/Systems/1");
        assert!(!s.expand && !s.skip_paging);
    }

    #[test]
    fn rollup_takes_the_worst_component_and_skips_absent_ones() {
        let system = json!({
            "Model": "PowerEdge R630",
            "PowerState": "On",
            "Status": {"Health": "OK", "HealthRollup": "Warning", "State": "Enabled"},
            "ProcessorSummary": {"Count": 2, "Model": "E5-2660 v3", "Status": {"Health": "OK", "State": "Enabled"}},
            "MemorySummary": {"TotalSystemMemoryGiB": 208.6, "Status": {"Health": "OK", "State": "Enabled"}}
        });
        let mut report = system_report(&system);
        assert_eq!(report.overall, Health::Warning);
        assert_eq!(report.power_state, "On");

        let power = json!({"PowerSupplies": [
            {"Name": "PS1", "Status": {"Health": "OK", "State": "Enabled"}},
            {"Name": "PS2", "Status": {"Health": "Critical", "State": "Enabled"}},
            {"Name": "PS3", "Status": {"State": "Absent"}}
        ]});
        report.sensors.extend(power_sensors(&power));
        let alarms = report.alarms();
        assert_eq!(alarms.len(), 2);
        assert_eq!(alarms[0].name, "PS2");
        assert_eq!(alarms[0].health, Health::Critical);
        assert!(!report.sensors.iter().any(|s| s.name == "PS3"));
        assert!(report.summary().starts_with("Critical Power Supply: PS2"));
    }
}
