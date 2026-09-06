use chrono::{DateTime, Local};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    TempC,
    Volts,
    Amps,
    Watts,
    Rpm,
    Pct,
    Joules,
    Mhz,
    RateMBs,
    Dbm,
}

impl Unit {
    pub fn suffix(self) -> &'static str {
        match self {
            Unit::TempC => "°C",
            Unit::Volts => "V",
            Unit::Amps => "A",
            Unit::Watts => "W",
            Unit::Rpm => "RPM",
            Unit::Pct => "%",
            Unit::Joules => "J",
            Unit::Mhz => "MHz",
            Unit::RateMBs => "MiB/s",
            Unit::Dbm => "dBm",
        }
    }

    pub fn csv_suffix(self) -> &'static str {
        match self {
            Unit::TempC => "C",
            Unit::RateMBs => "MiBps",
            other => other.suffix(),
        }
    }

    pub fn fmt_value(self, v: f64) -> String {
        match self {
            Unit::TempC => format!("{v:.1}"),
            Unit::Volts => format!("{v:.2}"),
            Unit::Amps => format!("{v:.3}"),
            Unit::Watts => format!("{v:.2}"),
            Unit::Rpm => format!("{v:.0}"),
            Unit::Pct => format!("{v:.0}"),
            Unit::Joules => format!("{v:.1}"),
            Unit::Mhz => format!("{v:.0}"),
            Unit::RateMBs => format!("{v:.1}"),
            Unit::Dbm => format!("{v:.0}"),
        }
    }

    pub fn format(self, v: f64) -> String {
        format!("{}{}", self.fmt_value(v), self.suffix())
    }
}

#[derive(Clone, Debug)]
pub struct Reading {
    pub label: String,
    pub value: f64,
    pub unit: Unit,
}

#[derive(Clone, Debug)]
pub struct Chip {
    pub name: String,
    pub readings: Vec<Reading>,
}

#[derive(Clone, Debug, Default)]
pub struct CpuSnapshot {
    pub model: String,
    pub total_pct: f32,
    pub per_pct: Vec<f32>,
    pub per_mhz: Vec<f32>,
    pub pkg_w: Option<f64>,
    pub governor: String,
    pub epp: String,
}

#[derive(Clone, Debug, Default)]
pub struct MemSnapshot {
    pub total_kib: u64,
    pub avail_kib: u64,
    pub swap_total_kib: u64,
    pub swap_free_kib: u64,
}

#[derive(Clone, Debug, Default)]
pub struct BatterySnapshot {
    pub status: String,
    pub capacity_pct: Option<f64>,
    pub volts: Option<f64>,
    pub amps: Option<f64>,
    pub watts: Option<f64>,
    pub cycles: Option<u32>,
    pub energy_now_wh: Option<f64>,
    pub energy_full_wh: Option<f64>,
    pub health_pct: Option<f64>,
    pub runtime_min: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct VramSnapshot {
    pub used_mib: f64,
    pub total_mib: f64,
    pub gtt_used_mib: Option<f64>,
}

pub struct Snapshot {
    pub time: DateTime<Local>,
    pub hostname: String,
    pub kernel: String,
    pub uptime_s: f64,
    pub load: [f32; 3],
    pub cpu: CpuSnapshot,
    pub mem: MemSnapshot,
    pub battery: Option<BatterySnapshot>,
    pub ac_online: Option<bool>,
    pub vram: Option<VramSnapshot>,
    pub chips: Vec<Chip>,
}
