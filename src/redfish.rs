//! Health, sensors and the System Event Log (SEL) over Redfish.
//!
//! Redfish uses HTTP basic auth per request, so none of this takes one of the BMC's few
//! web-session slots (or disturbs an open console).

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

pub fn paths(vendor: Vendor) -> &'static Paths {
    match vendor {
        Vendor::Dell => &DELL,
        Vendor::Supermicro => &SUPERMICRO,
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
        }
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

    fn get_once(&self, path: &str) -> Result<Value, GetError> {
        let resp = self
            .client
            .get(format!("https://{}{path}", self.host))
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(60))
            .send()
            .with_context(|| format!("connecting to https://{}", self.host))
            .map_err(GetError::Transient)?;
        let status = resp.status();
        match status.as_u16() {
            401 => {
                return Err(GetError::Fatal(anyhow::anyhow!(
                    "{}: invalid username or password",
                    self.host
                )));
            }
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

    pub fn power(&self) -> Result<Power> {
        Ok(power_of(&self.get(self.paths()?.system)?))
    }

    /// `ComputerSystem.Reset` with `reset_type` ("On", "ForceOff", "GracefulShutdown",
    /// "ForceRestart", "GracefulRestart", "PowerCycle", …).
    pub fn reset(&self, reset_type: &str) -> Result<()> {
        let lock = host_lock(&self.host);
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let resp = self
            .client
            .post(format!(
                "https://{}{}/Actions/ComputerSystem.Reset",
                self.host,
                self.paths()?.system
            ))
            .basic_auth(&self.user, Some(&self.password))
            .json(&serde_json::json!({ "ResetType": reset_type }))
            .timeout(Duration::from_secs(60))
            .send()
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
        self.client
            .post(format!(
                "https://{}{}/Actions/LogService.ClearLog",
                self.host,
                self.paths()?.sel_service
            ))
            .basic_auth(&self.user, Some(&self.password))
            .json(&serde_json::json!({}))
            .timeout(Duration::from_secs(60))
            .send()
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
            sensors.push(Sensor {
                category: "Power Supply",
                name: str_field(psu, "Name"),
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
    SelEntry {
        id: e["Id"]
            .as_str()
            .and_then(|s| s.trim().parse().ok())
            .or_else(|| e["Id"].as_u64().map(|n| n as u32))
            .unwrap_or(0),
        created: str_field(e, "Created"),
        severity: Health::parse(e["Severity"].as_str()),
        message: str_field(e, "Message"),
        sensor,
        event: first_member("EntryCode"),
    }
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
