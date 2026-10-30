//! "Export logs…": one zip per server for a support case or an audit, with a hardware
//! summary, the component health, the SEL and the Lifecycle Controller log. Credentials
//! are never written into it.

use std::io::{Cursor, Write};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use zip::write::SimpleFileOptions;

use crate::redfish::{self, CHASSIS_PATH, HealthReport, Redfish, SYSTEM_PATH, str_field};

/// How many Lifecycle Controller entries to include (the newest). iDRAC 8 returns 50 per
/// ~5 s request, so the full log (thousands) would take minutes.
const LC_LOG_LIMIT: usize = 500;

/// Everything read from the iDRAC, before it is turned into files.
#[derive(Debug, Default)]
pub struct Collected {
    pub system: Value,
    pub manager: Value,
    pub processors: Vec<Value>,
    pub memory: Vec<Value>,
    pub storage: Vec<(Value, Vec<Value>)>,
    pub network: Vec<Value>,
    pub power: Value,
    pub health: Option<HealthReport>,
    pub sel: Vec<redfish::SelEntry>,
    pub lc_log: Vec<Value>,
    /// Parts that could not be read; the export still goes ahead without them.
    pub errors: Vec<String>,
}

/// Read everything, reporting each step to `progress`; `cancelled` is checked between steps.
pub fn collect(
    rf: &Redfish,
    progress: &dyn Fn(&str),
    cancelled: &dyn Fn() -> bool,
) -> Result<Collected> {
    let mut c = Collected::default();
    macro_rules! step {
        ($label:expr, $target:expr, $read:expr) => {{
            if cancelled() {
                bail!("cancelled");
            }
            progress($label);
            match $read {
                Ok(v) => $target = v,
                Err(e) => c.errors.push(format!("{}: {e:#}", $label)),
            }
        }};
    }
    // The system is required: without it there is nothing to export.
    progress("system");
    c.system = rf.json(SYSTEM_PATH)?;
    step!(
        "iDRAC",
        c.manager,
        rf.json("/redfish/v1/Managers/iDRAC.Embedded.1")
    );
    step!(
        "processors",
        c.processors,
        rf.collection(&format!("{SYSTEM_PATH}/Processors"))
    );
    step!(
        "memory",
        c.memory,
        rf.collection(&format!("{SYSTEM_PATH}/Memory"))
    );
    step!("storage", c.storage, read_storage(rf));
    step!(
        "network",
        c.network,
        rf.collection(&format!("{SYSTEM_PATH}/EthernetInterfaces"))
    );
    step!(
        "power supplies",
        c.power,
        rf.json(&format!("{CHASSIS_PATH}/Power"))
    );
    step!("component health", c.health, rf.full_health().map(Some));
    step!("System Event Log", c.sel, rf.sel());
    step!(
        "Lifecycle Controller log",
        c.lc_log,
        rf.lc_log(LC_LOG_LIMIT)
    );
    Ok(c)
}

fn read_storage(rf: &Redfish) -> Result<Vec<(Value, Vec<Value>)>> {
    let mut out = Vec::new();
    for controller in rf.collection(&format!("{SYSTEM_PATH}/Storage"))? {
        let mut drives = Vec::new();
        for link in controller["Drives"].as_array().into_iter().flatten() {
            if let Some(path) = link["@odata.id"].as_str() {
                drives.push(rf.json(path)?);
            }
        }
        out.push((controller, drives));
    }
    Ok(out)
}

/// DIMMs in slot order ("DIMM A2" before "DIMM A10"), not the iDRAC's order.
fn sorted_by_slot(dimms: &[Value]) -> Vec<&Value> {
    fn key(v: &Value) -> (String, u32) {
        let slot = str_field(v, "DeviceLocator");
        let digits: String = slot
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        let number = digits
            .chars()
            .rev()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        (
            slot.trim_end_matches(|c: char| c.is_ascii_digit())
                .to_string(),
            number,
        )
    }
    let mut sorted: Vec<&Value> = dimms.iter().collect();
    sorted.sort_by_key(|v| key(v));
    sorted
}

fn health_of(v: &Value) -> String {
    let status = &v["Status"];
    status["HealthRollup"]
        .as_str()
        .or_else(|| status["Health"].as_str())
        .unwrap_or("")
        .to_string()
}

fn enabled(v: &Value) -> bool {
    !matches!(
        v["Status"]["State"].as_str(),
        Some("Absent") | Some("Disabled") | Some("UnavailableOffline")
    )
}

/// The summary as structured data (also the source of `summary.txt`).
pub fn summary_json(c: &Collected, name: &str, host: &str, exported_at: &str) -> Value {
    let s = |v: &Value, k: &str| str_field(v, k);
    // iDRAC 8 reports an 8 GB DIMM as 7812 MiB; show sizes within 5% of a whole GiB as that.
    let gib = |mib: Option<f64>| {
        mib.map(|m| {
            let g = m / 1024.0;
            if (g - g.round()).abs() / g.max(1.0) < 0.05 {
                g.round()
            } else {
                (g * 10.0).round() / 10.0
            }
        })
    };
    let drives = |drives: &[Value]| -> Vec<Value> {
        drives
            .iter()
            .map(|d| {
                json!({
                    "name": s(d, "Name"),
                    "model": s(d, "Model"),
                    "serial": s(d, "SerialNumber"),
                    "capacity_gb": d["CapacityBytes"].as_f64().map(|b| (b / 1e9).round()),
                    "media": s(d, "MediaType"),
                    "protocol": s(d, "Protocol"),
                    "health": health_of(d),
                })
            })
            .collect()
    };
    json!({
        "exported_at": exported_at,
        "name": name,
        "address": host,
        "system": {
            "manufacturer": s(&c.system, "Manufacturer"),
            "model": s(&c.system, "Model"),
            "service_tag": s(&c.system, "SKU"),
            "serial_number": s(&c.system, "SerialNumber"),
            "bios_version": s(&c.system, "BiosVersion"),
            "host_name": s(&c.system, "HostName"),
            "power_state": s(&c.system, "PowerState"),
            "health": health_of(&c.system),
        },
        "idrac": {
            "model": s(&c.manager, "Model"),
            "firmware": s(&c.manager, "FirmwareVersion"),
        },
        "processors": c.processors.iter().filter(|p| enabled(p)).map(|p| json!({
            "socket": s(p, "Socket"),
            "model": s(p, "Model"),
            "cores": p["TotalCores"],
            "threads": p["TotalThreads"],
            "max_mhz": p["MaxSpeedMHz"],
            "health": health_of(p),
        })).collect::<Vec<_>>(),
        "memory": sorted_by_slot(&c.memory).into_iter().filter(|m| enabled(m) && m["CapacityMiB"].as_f64().unwrap_or(0.0) > 0.0).map(|m| json!({
            "slot": if s(m, "DeviceLocator").is_empty() { s(m, "Id") } else { s(m, "DeviceLocator") },
            "capacity_gib": gib(m["CapacityMiB"].as_f64()),
            "speed_mhz": m["OperatingSpeedMhz"],
            "type": s(m, "MemoryDeviceType"),
            "manufacturer": s(m, "Manufacturer"),
            "part_number": s(m, "PartNumber"),
            "health": health_of(m),
        })).collect::<Vec<_>>(),
        "storage": c.storage.iter().map(|(ctl, ds)| json!({
            "controller": s(ctl, "Name"),
            "health": health_of(ctl),
            "drives": drives(ds),
        })).collect::<Vec<_>>(),
        "network": c.network.iter().map(|n| json!({
            "id": s(n, "Id"),
            "mac": s(n, "MACAddress"),
            "link": s(n, "LinkStatus"),
            "speed_mbps": n["SpeedMbps"],
            "health": health_of(n),
        })).collect::<Vec<_>>(),
        "power_supplies": c.power["PowerSupplies"].as_array().into_iter().flatten().filter(|p| enabled(p)).map(|p| json!({
            "name": s(p, "Name"),
            "model": s(p, "Model"),
            "capacity_w": p["PowerCapacityWatts"],
            "firmware": s(p, "FirmwareVersion"),
            "health": health_of(p),
        })).collect::<Vec<_>>(),
        "health": c.health.as_ref().map(|h| json!({
            "overall": h.overall.label(),
            "alarms": h.alarms().iter().map(|a| format!("{} {}: {}", a.health.label(), a.category, a.name)).collect::<Vec<_>>(),
        })),
        "sel": {
            "entries": c.sel.len(),
            "warning_or_critical": c.sel.iter().filter(|e| e.severity.is_alarm()).count(),
        },
        "lc_log_entries": c.lc_log.len(),
        "errors": c.errors,
    })
}

/// A plain-text rendering of [`summary_json`] for people.
pub fn summary_text(j: &Value) -> String {
    let t = |v: &Value| match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    let sys = &j["system"];
    line(format!(
        "Rust Drac export — {} ({})",
        t(&j["name"]),
        t(&j["address"])
    ));
    line(format!("Exported:      {}", t(&j["exported_at"])));
    line(String::new());
    line(format!(
        "Model:         {} {}",
        t(&sys["manufacturer"]),
        t(&sys["model"])
    ));
    line(format!("Service tag:   {}", t(&sys["service_tag"])));
    line(format!("Serial:        {}", t(&sys["serial_number"])));
    line(format!("BIOS:          {}", t(&sys["bios_version"])));
    line(format!(
        "iDRAC:         {} firmware {}",
        t(&j["idrac"]["model"]),
        t(&j["idrac"]["firmware"])
    ));
    line(format!("Host name:     {}", t(&sys["host_name"])));
    line(format!("Power:         {}", t(&sys["power_state"])));
    line(format!(
        "Health:        {}",
        t(&j["health"]["overall"]).if_empty(&t(&sys["health"]))
    ));
    for alarm in j["health"]["alarms"].as_array().into_iter().flatten() {
        line(format!("  ! {}", t(alarm)));
    }
    let section = |out: &mut String, title: &str, rows: &Value, fmt: &dyn Fn(&Value) -> String| {
        let rows = rows.as_array().cloned().unwrap_or_default();
        out.push_str(&format!("\n{title} ({})\n", rows.len()));
        for r in &rows {
            out.push_str(&format!("  {}\n", fmt(r)));
        }
    };
    section(&mut out, "Processors", &j["processors"], &|p| {
        format!(
            "{}: {} — {} cores / {} threads, max {} MHz [{}]",
            t(&p["socket"]),
            t(&p["model"]),
            t(&p["cores"]),
            t(&p["threads"]),
            t(&p["max_mhz"]),
            t(&p["health"])
        )
    });
    section(&mut out, "Memory", &j["memory"], &|m| {
        format!(
            "{}: {} GiB {} @ {} MHz, {} {} [{}]",
            t(&m["slot"]),
            t(&m["capacity_gib"]),
            t(&m["type"]),
            t(&m["speed_mhz"]),
            t(&m["manufacturer"]),
            t(&m["part_number"]),
            t(&m["health"])
        )
    });
    let storage = j["storage"].as_array().cloned().unwrap_or_default();
    out.push_str(&format!("\nStorage controllers ({})\n", storage.len()));
    for ctl in &storage {
        out.push_str(&format!(
            "  {} [{}]\n",
            t(&ctl["controller"]),
            t(&ctl["health"])
        ));
        for d in ctl["drives"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "    {}: {} {} GB {} {}, serial {} [{}]\n",
                t(&d["name"]),
                t(&d["model"]),
                t(&d["capacity_gb"]),
                t(&d["media"]),
                t(&d["protocol"]),
                t(&d["serial"]),
                t(&d["health"])
            ));
        }
    }
    section(&mut out, "Network interfaces", &j["network"], &|n| {
        format!(
            "{}: {} link {} {} Mb/s [{}]",
            t(&n["id"]),
            t(&n["mac"]),
            t(&n["link"]),
            t(&n["speed_mbps"]),
            t(&n["health"])
        )
    });
    section(&mut out, "Power supplies", &j["power_supplies"], &|p| {
        format!(
            "{}: {} {} W, firmware {} [{}]",
            t(&p["name"]),
            t(&p["model"]),
            t(&p["capacity_w"]),
            t(&p["firmware"]),
            t(&p["health"])
        )
    });
    out.push_str(&format!(
        "\nSystem Event Log: {} entries, {} warning/critical (sel.csv)\n",
        t(&j["sel"]["entries"]),
        t(&j["sel"]["warning_or_critical"])
    ));
    out.push_str(&format!(
        "Lifecycle Controller log: newest {} entries (lclog.csv)\n",
        t(&j["lc_log_entries"])
    ));
    if let Some(errors) = j["errors"].as_array().filter(|e| !e.is_empty()) {
        out.push_str("\nCould not read:\n");
        for e in errors {
            out.push_str(&format!("  {}\n", t(e)));
        }
    }
    out
}

trait IfEmpty {
    fn if_empty(self, other: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, other: &str) -> String {
        if self.is_empty() {
            other.to_string()
        } else {
            self
        }
    }
}

fn csv_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn health_csv(report: &HealthReport) -> String {
    let mut out = String::from("Status,Component,Name,Reading\n");
    for s in &report.sensors {
        out += &format!(
            "{},{},{},{}\n",
            s.health.label(),
            csv_quote(s.category),
            csv_quote(&s.name),
            csv_quote(&s.reading)
        );
    }
    out
}

fn lc_log_csv(entries: &[Value]) -> String {
    let mut out = String::from("Id,Created,Severity,MessageId,Message\n");
    for e in entries {
        out += &format!(
            "{},{},{},{},{}\n",
            csv_quote(&str_field(e, "Id")),
            csv_quote(&str_field(e, "Created")),
            csv_quote(&str_field(e, "Severity")),
            csv_quote(&str_field(e, "MessageId")),
            csv_quote(&str_field(e, "Message"))
        );
    }
    out
}

/// The zip archive: `summary.txt`, `summary.json`, `health.csv`, `sel.csv`, `lclog.csv`,
/// all inside a folder named after the archive.
pub fn build_zip(c: &Collected, folder: &str, summary: &Value) -> Result<Vec<u8>> {
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut add = |name: &str, body: &[u8]| -> Result<()> {
        zip.start_file(format!("{folder}/{name}"), opts)?;
        zip.write_all(body)?;
        Ok(())
    };
    add("summary.txt", summary_text(summary).as_bytes())?;
    add(
        "summary.json",
        serde_json::to_string_pretty(summary)?.as_bytes(),
    )?;
    if let Some(h) = &c.health {
        add("health.csv", health_csv(h).as_bytes())?;
    }
    add("sel.csv", redfish::sel_csv(&c.sel).as_bytes())?;
    add("lclog.csv", lc_log_csv(&c.lc_log).as_bytes())?;
    Ok(zip.finish()?.into_inner())
}

/// `<name>-<service tag>-YYYYMMDD-HHMMSS`, safe as a file name.
pub fn base_name(name: &str, service_tag: &str, stamp: &str) -> String {
    let clean = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let mut parts = vec![clean(name)];
    if !service_tag.is_empty() && !name.contains(service_tag) {
        parts.push(clean(service_tag));
    }
    parts.push(stamp.to_string());
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample() -> Collected {
        Collected {
            system: json!({"Manufacturer": "Dell Inc.", "Model": "PowerEdge R630", "SKU": "ABC1234",
                "SerialNumber": "CN123", "BiosVersion": "2.19.0", "PowerState": "On",
                "Status": {"Health": "OK", "HealthRollup": "OK", "State": "Enabled"}}),
            manager: json!({"Model": "13G Monolithic", "FirmwareVersion": "2.75.75.75"}),
            processors: vec![
                json!({"Socket": "CPU.Socket.1", "Model": "Xeon E5-2660 v3", "TotalCores": 10,
                "TotalThreads": 20, "MaxSpeedMHz": 4000, "Status": {"Health": "OK", "State": "Enabled"}}),
            ],
            memory: vec![
                json!({"DeviceLocator": "DIMM A1", "CapacityMiB": 16384, "OperatingSpeedMhz": 2133,
                    "MemoryDeviceType": "DDR4", "Status": {"Health": "OK", "State": "Enabled"}}),
                json!({"DeviceLocator": "DIMM A2", "CapacityMiB": 0, "Status": {"State": "Absent"}}),
            ],
            power: json!({"PowerSupplies": [
                {"Name": "PS1 Status", "PowerCapacityWatts": 750, "Status": {"Health": "OK", "State": "Enabled"}},
                {"Name": "PS2 Status", "Status": {"State": "Absent"}}]}),
            ..Default::default()
        }
    }

    #[test]
    fn summary_lists_only_populated_parts() {
        let j = summary_json(&sample(), "lab-r630", "192.0.2.10", "2026-10-30 17:00:00");
        assert_eq!(j["system"]["service_tag"], "ABC1234");
        assert_eq!(j["idrac"]["firmware"], "2.75.75.75");
        assert_eq!(
            j["memory"].as_array().unwrap().len(),
            1,
            "empty DIMM slot skipped"
        );
        assert_eq!(j["memory"][0]["capacity_gib"], 16.0);
        assert_eq!(
            j["power_supplies"].as_array().unwrap().len(),
            1,
            "absent PSU skipped"
        );
        let text = summary_text(&j);
        assert!(text.contains("Service tag:   ABC1234"), "{text}");
        assert!(text.contains("DIMM A1: 16.0 GiB DDR4 @ 2133 MHz"), "{text}");
        assert!(
            text.contains("iDRAC:         13G Monolithic firmware 2.75.75.75"),
            "{text}"
        );
    }

    #[test]
    fn zip_holds_the_expected_files_and_no_secrets() {
        let c = sample();
        let j = summary_json(&c, "lab-r630", "192.0.2.10", "now");
        let bytes = build_zip(&c, "lab-r630-ABC1234-20261030-170000", &j).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "lab-r630-ABC1234-20261030-170000/lclog.csv",
                "lab-r630-ABC1234-20261030-170000/sel.csv",
                "lab-r630-ABC1234-20261030-170000/summary.json",
                "lab-r630-ABC1234-20261030-170000/summary.txt",
            ]
        );
        let mut all = String::new();
        for i in 0..zip.len() {
            zip.by_index(i).unwrap().read_to_string(&mut all).unwrap();
        }
        assert!(!all.to_lowercase().contains("password"));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(
            base_name("lab r630/1", "ABC1234", "20261030-170000"),
            "lab_r630_1-ABC1234-20261030-170000"
        );
        assert_eq!(
            base_name("idrac-ABC1234", "ABC1234", "x"),
            "idrac-ABC1234-x"
        );
    }

    /// Against a real iDRAC:
    /// `RUSTDRAC_TEST_HOST=… RUSTDRAC_TEST_USER=… RUSTDRAC_TEST_PASSWORD=… cargo test -- --ignored export`
    /// (writes the zip to the system temp dir and prints its summary).
    #[test]
    #[ignore]
    fn export_from_a_real_idrac() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let host = var("RUSTDRAC_TEST_HOST");
        let client = crate::idrac::http_client().unwrap();
        let rf = Redfish::new(
            &client,
            &host,
            &var("RUSTDRAC_TEST_USER"),
            &var("RUSTDRAC_TEST_PASSWORD"),
        );
        let started = std::time::Instant::now();
        let c = collect(
            &rf,
            &|step| eprintln!("[{:>5.1}s] {step}", started.elapsed().as_secs_f32()),
            &|| false,
        )
        .unwrap();
        let j = summary_json(&c, "test", &host, "now");
        let zip = build_zip(&c, "rustdrac-export-test", &j).unwrap();
        let path = std::env::temp_dir().join("rustdrac-export-test.zip");
        std::fs::write(&path, &zip).unwrap();
        eprintln!(
            "{}\n{} bytes -> {}",
            summary_text(&j),
            zip.len(),
            path.display()
        );
        assert!(
            j["errors"].as_array().unwrap().is_empty(),
            "errors: {}",
            j["errors"]
        );
    }
}
