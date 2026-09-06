use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::process::Command;

use anyhow::{Context, Result};
use chrono::Local;

use crate::model::Snapshot;

/// root 运行时把新建的 CSV 归还给发起 sudo 的用户, 避免 root 属主文件散落
fn chown_to_sudo_user(path: &str) {
    let Some(user) = std::env::var("SUDO_USER").ok() else {
        return;
    };
    let uid = Command::new("id")
        .args(["-u", &user])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok());
    let Some(uid) = uid else { return };
    let gid = Command::new("id")
        .args(["-g", &user])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().ok());
    let _ = std::os::unix::fs::chown(path, Some(uid), gid);
}

pub struct Logger {
    file: File,
    header: Vec<String>,
    path: String,
}

impl Logger {
    pub fn create(snap: &Snapshot, path: Option<&str>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_string(),
            None => format!("rustinfo_{}.csv", Local::now().format("%Y%m%d_%H%M%S")),
        };
        let header: Vec<String> = flatten(snap).into_iter().map(|(k, _)| k).collect();
        let mut file = File::create(&path).with_context(|| format!("无法创建 {path}"))?;
        writeln!(file, "{}", header.join(","))?;
        chown_to_sudo_user(&path);
        Ok(Self { file, header, path })
    }

    pub fn write(&mut self, snap: &Snapshot) -> Result<()> {
        let map: HashMap<String, String> = flatten(snap).into_iter().collect();
        let row: Vec<String> = self
            .header
            .iter()
            .map(|k| map.get(k).cloned().unwrap_or_default())
            .collect();
        writeln!(self.file, "{}", row.join(","))?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn finish(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub fn flatten(s: &Snapshot) -> Vec<(String, String)> {
    let mut out = Vec::new();
    out.push((
        "ts".into(),
        s.time.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
    ));
    out.push(("cpu_total_pct".into(), format!("{:.2}", s.cpu.total_pct)));
    if let Some(w) = s.cpu.pkg_w {
        out.push(("cpu_pkg_w".into(), format!("{:.3}", w)));
    }
    for (i, p) in s.cpu.per_pct.iter().enumerate() {
        out.push((format!("t{i:02}_pct"), format!("{:.2}", p)));
    }
    for (i, m) in s.cpu.per_mhz.iter().enumerate() {
        out.push((format!("t{i:02}_mhz"), format!("{:.0}", m)));
    }
    let used = s.mem.total_kib.saturating_sub(s.mem.avail_kib);
    let mem_pct = if s.mem.total_kib > 0 {
        used as f64 / s.mem.total_kib as f64 * 100.0
    } else {
        0.0
    };
    out.push(("mem_used_pct".into(), format!("{:.2}", mem_pct)));
    out.push(("mem_used_mib".into(), format!("{:.1}", used as f64 / 1024.0)));
    out.push((
        "swap_used_mib".into(),
        format!(
            "{:.1}",
            s.mem.swap_total_kib.saturating_sub(s.mem.swap_free_kib) as f64 / 1024.0
        ),
    ));
    out.push(("load1".into(), format!("{:.3}", s.load[0])));
    if let Some(b) = &s.battery {
        if let Some(v) = b.capacity_pct {
            out.push(("batt_pct".into(), format!("{:.0}", v)));
        }
        if let Some(v) = b.volts {
            out.push(("batt_v".into(), format!("{:.3}", v)));
        }
        if let Some(v) = b.amps {
            out.push(("batt_a".into(), format!("{:.3}", v)));
        }
        if let Some(v) = b.watts {
            out.push(("batt_w".into(), format!("{v:.3}")));
        }
        if let Some(v) = b.health_pct {
            out.push(("batt_health_pct".into(), format!("{v:.1}")));
        }
        if let Some(m) = b.runtime_min {
            out.push(("batt_runtime_min".into(), format!("{m}")));
        }
    }
    if let Some(v) = &s.vram {
        out.push(("vram_used_mib".into(), format!("{:.0}", v.used_mib)));
        out.push(("vram_total_mib".into(), format!("{:.0}", v.total_mib)));
    }
    for chip in &s.chips {
        for r in &chip.readings {
            out.push((
                format!(
                    "{}.{}_{}",
                    sanitize(&chip.name),
                    sanitize(&r.label),
                    r.unit.csv_suffix()
                ),
                format!("{:.3}", r.value),
            ));
        }
    }
    out
}
