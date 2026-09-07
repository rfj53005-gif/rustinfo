use std::collections::HashMap;
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use chrono::Local;

use crate::model::{
    BatterySnapshot, Chip, CpuSnapshot, MemSnapshot, Reading, Snapshot, Unit, VramSnapshot,
};

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// _IOWR('d', 0x40 + 0x05, struct drm_amdgpu_info)
const DRM_IOCTL_AMDGPU_INFO: u64 = 0xC020_6445;
const AMDGPU_INFO_SENSOR: u32 = 0x1D;

#[repr(C)]
struct DrmAmdgpuInfo {
    return_pointer: u64,
    return_size: u32,
    query: u32,
    sensor_type: u32,
    _pad: [u32; 3],
}

const MSR_RAPL_POWER_UNIT: u64 = 0xC001_0299;
const MSR_CORE_ENERGY: u64 = 0xC001_029A;

/// 每物理核能量计数器 (MSR C001_029A): 32 位, 单位 2^-Esu J (Esu 在 C001_0299[12:8]),
/// SMT 兄弟线程共享同一计数器, 核心进 CC6 时冻结。intel_rapl 驱动只在 lead CPU 上
/// 把它注册成 sysfs 的 "core" 子域, 所以那个值是单核数据而非全核合计 —— 这里按核直读。
struct MsrCore {
    label: String,
    file: File,
    prev: Option<u32>,
}

struct MsrState {
    cores: Vec<MsrCore>,
    j_per_lsb: f64,
}

fn msr_read(cpu: usize, addr: u64) -> Option<u64> {
    let f = File::open(format!("/dev/cpu/{cpu}/msr")).ok()?;
    let mut buf = [0u8; 8];
    f.read_exact_at(&mut buf, addr).ok()?;
    Some(u64::from_le_bytes(buf))
}

impl MsrState {
    fn init(n: usize) -> Option<MsrState> {
        let base = Path::new("/sys/devices/system/cpu");
        let mut groups: Vec<(String, usize)> = Vec::new();
        let mut seen: HashMap<String, ()> = HashMap::new();
        for i in 0..n {
            let core_id = read_trimmed(&base.join(format!("cpu{i}/topology/core_id")))?;
            let sibs = read_trimmed(&base.join(format!("cpu{i}/topology/thread_siblings_list")))?;
            if seen.insert(sibs, ()).is_some() {
                continue; // SMT 兄弟, 同一物理核
            }
            groups.push((format!("C{core_id:0>2}"), i));
        }
        let esu = ((msr_read(groups.first()?.1, MSR_RAPL_POWER_UNIT)? >> 8) & 0x1F) as i32;
        let j_per_lsb = 2f64.powi(-esu);
        let cores = groups
            .into_iter()
            .filter_map(|(label, cpu)| {
                File::open(format!("/dev/cpu/{cpu}/msr"))
                    .ok()
                    .map(|file| MsrCore {
                        label,
                        file,
                        prev: None,
                    })
            })
            .collect();
        Some(MsrState { cores, j_per_lsb })
    }

    fn power_watts(&mut self, dt: Option<f64>) -> Vec<Reading> {
        let mut out = Vec::new();
        for c in &mut self.cores {
            let mut buf = [0u8; 8];
            if c.file.read_exact_at(&mut buf, MSR_CORE_ENERGY).is_err() {
                continue;
            }
            let cur = (u64::from_le_bytes(buf) & 0xFFFF_FFFF) as u32;
            if let (Some(prev), Some(dt)) = (c.prev, dt) {
                let de = cur.wrapping_sub(prev) as f64;
                let w = de * self.j_per_lsb / dt;
                if (0.0..200.0).contains(&w) {
                    out.push(Reading {
                        label: c.label.clone(),
                        value: w,
                        unit: Unit::Watts,
                    });
                }
            }
            c.prev = Some(cur);
        }
        out
    }
}

fn read_trimmed(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn read_num<T: std::str::FromStr>(p: &Path) -> Option<T> {
    read_trimmed(p)?.parse().ok()
}

pub struct Prober {
    prev_idle: Option<u64>,
    prev_total: Option<u64>,
    prev_per: Vec<(u64, u64)>,
    prev_rapl: HashMap<String, (u64, Instant, u64)>,
    prev_disk: HashMap<String, (u64, u64, u64, Instant)>,
    prev_net: HashMap<String, (u64, u64, Instant)>,
    msr: Option<MsrState>,
    last_snap: Option<Instant>,
    // 静态/半静态缓存: 整个运行期只解析一次, 不再每个采样周期重读
    hostname: String,
    kernel: String,
    model: String,
    threads: usize,
    freq_paths: Vec<PathBuf>,
    amdgpu_dev: Option<PathBuf>,
    drm: Option<File>,
    chip_probes: Option<Vec<ChipProbe>>,
}

impl Prober {
    pub fn new() -> Self {
        let threads = thread_count();
        Self {
            prev_idle: None,
            prev_total: None,
            prev_per: Vec::new(),
            prev_rapl: HashMap::new(),
            prev_disk: HashMap::new(),
            prev_net: HashMap::new(),
            msr: None,
            last_snap: None,
            hostname: read_trimmed(Path::new("/proc/sys/kernel/hostname")).unwrap_or_default(),
            kernel: read_trimmed(Path::new("/proc/sys/kernel/osrelease")).unwrap_or_default(),
            model: cpu_model(),
            threads,
            freq_paths: (0..threads)
                .map(|i| {
                    PathBuf::from(format!(
                        "/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq"
                    ))
                })
                .collect(),
            amdgpu_dev: amdgpu_dev_path(),
            drm: amdgpu_card_node().and_then(|n| File::open(n).ok()),
            chip_probes: None,
        }
    }

    pub fn snapshot(&mut self) -> Result<Snapshot> {
        let now = Instant::now();
        let dt = self
            .last_snap
            .take()
            .map(|t| now.duration_since(t).as_secs_f64())
            .filter(|d| *d >= 0.05);
        self.last_snap = Some(now);

        let (uptime_s, load) = read_uptime_load();
        let (total_pct, per_pct) = self.cpu_usage();
        let per_mhz = read_freqs(&self.freq_paths);
        let mut rapl = rapl_zones(&mut self.prev_rapl);
        if self.msr.is_none() {
            self.msr = MsrState::init(self.threads);
        }
        rapl.extend(self.msr.as_mut().map(|m| m.power_watts(dt)).unwrap_or_default());
        let pkg_w = rapl
            .iter()
            .find(|r| r.label == "Package")
            .map(|r| r.value);
        let cpu = CpuSnapshot {
            model: self.model.clone(),
            total_pct,
            per_pct,
            per_mhz,
            pkg_w,
            governor: read_trimmed(Path::new(
                "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor",
            ))
            .unwrap_or_default(),
            epp: read_trimmed(Path::new(
                "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference",
            ))
            .unwrap_or_default(),
        };
        let mem = read_mem();
        let (battery, ac_online) = read_battery_and_ac();
        let vram = self.amdgpu_dev.as_deref().and_then(read_vram);
        let mut chips = self.read_chips();
        if !rapl.is_empty() {
            chips.push(Chip {
                name: "rapl".into(),
                readings: rapl,
            });
        }
        let drm_sensors = self
            .drm
            .as_ref()
            .map(|f| read_drm_sensors(f))
            .unwrap_or_default();
        if !drm_sensors.is_empty() {
            chips.push(Chip {
                name: "amdgpu-sensor".into(),
                readings: drm_sensors,
            });
        }
        let dpm = self
            .amdgpu_dev
            .as_deref()
            .map(read_amdgpu_dpm)
            .unwrap_or_default();
        if !dpm.is_empty() {
            chips.push(Chip {
                name: "amdgpu-dpm".into(),
                readings: dpm,
            });
        }
        chips.extend(read_disk_io(&mut self.prev_disk));
        chips.extend(read_net_io(&mut self.prev_net));
        let psi = read_psi();
        if !psi.is_empty() {
            chips.push(Chip {
                name: "psi".into(),
                readings: psi,
            });
        }
        let pm = read_smu_pmtable();
        if !pm.is_empty() {
            chips.push(Chip {
                name: "smu".into(),
                readings: pm,
            });
        }
        chips.extend(read_smu());
        Ok(Snapshot {
            time: Local::now(),
            hostname: self.hostname.clone(),
            kernel: self.kernel.clone(),
            uptime_s,
            load,
            cpu,
            mem,
            battery,
            ac_online,
            vram,
            chips,
        })
    }

    fn cpu_usage(&mut self) -> (f32, Vec<f32>) {
        let Some((idle, total, per)) = read_stat() else {
            return (0.0, Vec::new());
        };
        let total_pct = match (self.prev_idle, self.prev_total) {
            (Some(pi), Some(pt)) => {
                let dt = total.saturating_sub(pt) as f32;
                let di = idle.saturating_sub(pi) as f32;
                if dt > 0.0 {
                    ((1.0 - di / dt) * 100.0).clamp(0.0, 100.0)
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        let mut per_pct = Vec::with_capacity(per.len());
        for (i, (ci, ct)) in per.iter().enumerate() {
            let p = match self.prev_per.get(i) {
                Some((pi, pt)) => {
                    let dt = ct.saturating_sub(*pt) as f32;
                    let di = ci.saturating_sub(*pi) as f32;
                    if dt > 0.0 {
                        ((1.0 - di / dt) * 100.0).clamp(0.0, 100.0)
                    } else {
                        0.0
                    }
                }
                None => 0.0,
            };
            per_pct.push(p);
        }
        self.prev_idle = Some(idle);
        self.prev_total = Some(total);
        self.prev_per = per;
        (total_pct, per_pct)
    }

    /// hwmon 逐值读取: 目录枚举/label 在首次调用时缓存 (chip_probes),
    /// 之后每个采样周期只读数值文件本身
    fn read_chips(&mut self) -> Vec<Chip> {
        let probes = self.chip_probes.get_or_insert_with(enum_hwmon);
        probes
            .iter()
            .filter_map(|p| {
                let readings: Vec<Reading> = p
                    .items
                    .iter()
                    .filter_map(|i| {
                        Some(Reading {
                            label: i.label.clone(),
                            value: read_num::<f64>(&i.path)? / i.scale,
                            unit: i.unit,
                        })
                    })
                    .collect();
                if readings.is_empty() {
                    None
                } else {
                    Some(Chip {
                        name: p.name.clone(),
                        readings,
                    })
                }
            })
            .collect()
    }
}

impl Default for Prober {
    fn default() -> Self {
        Self::new()
    }
}

/// RAPL Package 功率 (增量法); energy_uj 仅 root 可读。
/// 只取 package 域: sysfs 的 "core" 子域实为 lead CPU 单核计数器
/// (MSR C001_029A), 已由下方按核直读的每核功率取代。
fn rapl_zones(prev: &mut HashMap<String, (u64, Instant, u64)>) -> Vec<Reading> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/powercap") else {
        return out;
    };
    let mut zones: Vec<(String, String)> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let name = read_trimmed(&dir.join("name"))?;
            if !name.contains("package") {
                return None;
            }
            Some((dir.to_string_lossy().into_owned(), name))
        })
        .collect();
    zones.sort();
    for (dir, name) in zones {
        let Some(cur) = read_num::<u64>(Path::new(&format!("{dir}/energy_uj"))) else {
            continue;
        };
        let range =
            read_num::<u64>(Path::new(&format!("{dir}/max_energy_range_uj"))).unwrap_or(u64::MAX);
        let label = if name.contains("package") {
            "Package".to_string()
        } else {
            name.clone()
        };
        let watts = match prev.get(&dir) {
            Some((pe, at, prange)) => {
                let dt = at.elapsed().as_secs_f64();
                if dt >= 0.05 {
                    let de = if cur >= *pe {
                        cur - pe
                    } else {
                        prange.saturating_sub(*pe).saturating_add(cur)
                    };
                    let w = de as f64 / dt / 1e6;
                    if (0.0..500.0).contains(&w) {
                        Some(w)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            None => None,
        };
        prev.insert(dir, (cur, Instant::now(), range));
        if let Some(w) = watts {
            out.push(Reading {
                label,
                value: w,
                unit: Unit::Watts,
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

fn read_stat() -> Option<(u64, u64, Vec<(u64, u64)>)> {
    let s = fs::read_to_string("/proc/stat").ok()?;
    let mut agg = None;
    let mut per = Vec::new();
    for line in s.lines() {
        // cpu 行在文件头部连续排列, 首个非 cpu 行 (intr) 之后的内容无需解析
        let Some(rest) = line.strip_prefix("cpu") else { break };
        let is_agg = rest.starts_with(' ');
        if !is_agg && !rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut fields: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        if !is_agg {
            if fields.is_empty() {
                continue;
            }
            fields.remove(0);
        }
        if fields.len() < 4 {
            continue;
        }
        let idle = fields[3] + fields.get(4).copied().unwrap_or(0);
        let total: u64 = fields.iter().sum();
        if is_agg {
            agg = Some((idle, total));
        } else {
            per.push((idle, total));
        }
    }
    agg.map(|(i, t)| (i, t, per))
}

fn read_freqs(paths: &[PathBuf]) -> Vec<f32> {
    paths
        .iter()
        .map(|p| {
            read_num::<f64>(p)
                .map(|khz| (khz / 1000.0) as f32)
                .unwrap_or(0.0)
        })
        .collect()
}

fn thread_count() -> usize {
    let n = fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0);
    if n > 0 {
        n
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
    }
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        })
        .unwrap_or_else(|| "未知 CPU".into())
}

fn amdgpu_card_node() -> Option<String> {
    for c in 0..8 {
        if Path::new(&format!("/sys/class/drm/card{c}/device/mem_info_vram_total")).exists() {
            return Some(format!("/dev/dri/card{c}"));
        }
    }
    None
}

/// amdgpu DRM 传感器: ioctl 直读 SMU 实时值 (GFX/MCLK 时钟、负载、功率)
/// 这是 pp_dpm_mclk 之外的细粒度实时频率来源 (nvtop 同款)
fn read_drm_sensors(f: &File) -> Vec<Reading> {
    use std::os::fd::AsRawFd;

    let fd = f.as_raw_fd();

    let mut info = DrmAmdgpuInfo {
        return_pointer: 0,
        return_size: 4,
        query: AMDGPU_INFO_SENSOR,
        sensor_type: 0,
        _pad: [0; 3],
    };

    let mut out: Vec<Reading> = Vec::new();
    macro_rules! sensor {
        ($st:expr, $label:expr, $unit:expr, $scale:expr) => {{
            info.sensor_type = $st;
            let mut val: u32 = 0;
            info.return_pointer = &mut val as *mut u32 as u64;
            let ret = unsafe { ioctl(fd, DRM_IOCTL_AMDGPU_INFO, &mut info as *mut DrmAmdgpuInfo as u64) };
            if ret == 0 {
                out.push(Reading { label: $label.into(), value: val as f64 * $scale, unit: $unit });
            }
        }};
    }
    sensor!(1, "GfxClk", Unit::Mhz, 1.0);
    sensor!(2, "MemClk", Unit::Mhz, 1.0);
    sensor!(4, "Load", Unit::Pct, 1.0);
    sensor!(5, "Power", Unit::Watts, 1.0);
    sensor!(0xc, "InputPower", Unit::Watts, 1.0);
    sensor!(3, "Temp", Unit::TempC, 0.001);
    out
}

fn amdgpu_dev_path() -> Option<PathBuf> {
    for c in 0..8 {
        let dev = PathBuf::from(format!("/sys/class/drm/card{c}/device"));
        if dev.join("mem_info_vram_total").exists() {
            return Some(dev);
        }
    }
    None
}

fn read_mem() -> MemSnapshot {
    let mut m = MemSnapshot::default();
    let Ok(s) = fs::read_to_string("/proc/meminfo") else {
        return m;
    };
    let val = |line: &str| -> Option<u64> {
        line.split_whitespace().nth(1)?.parse().ok()
    };
    for line in s.lines() {
        let Some(v) = val(line) else { continue };
        match line.split(':').next().unwrap_or("") {
            "MemTotal" => m.total_kib = v,
            "MemAvailable" => m.avail_kib = v,
            "SwapTotal" => m.swap_total_kib = v,
            "SwapFree" => m.swap_free_kib = v,
            _ => {}
        }
    }
    m
}

fn read_battery_and_ac() -> (Option<BatterySnapshot>, Option<bool>) {
    let mut batt = None;
    let mut ac = None;
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return (None, None);
    };
    for e in entries.flatten() {
        let dir = e.path();
        match read_trimmed(&dir.join("type")).unwrap_or_default().as_str() {
            "Battery" if batt.is_none() => {
                let status = read_trimmed(&dir.join("status")).unwrap_or_default();
                let capacity_pct = read_num::<f64>(&dir.join("capacity"));
                let volts = read_num::<f64>(&dir.join("voltage_now")).map(|v| v / 1e6);
                let amps = read_num::<f64>(&dir.join("current_now")).map(|v| v / 1e6);
                let watts = read_num::<f64>(&dir.join("power_now"))
                    .map(|v| v / 1e6)
                    .or_else(|| amps.zip(volts).map(|(a, v)| a * v));
                let cycles = read_num::<u32>(&dir.join("cycle_count")).filter(|c| *c > 0);
                let e_now = read_num::<f64>(&dir.join("energy_now"));
                let e_full = read_num::<f64>(&dir.join("energy_full"));
                let c_now = read_num::<f64>(&dir.join("charge_now"));
                let c_full = read_num::<f64>(&dir.join("charge_full"));
                let (energy_now_wh, energy_full_wh) = match (e_now, e_full) {
                    (Some(a), Some(b)) => (Some(a / 1e6), Some(b / 1e6)),
                    _ => match (c_now, c_full, volts) {
                        (Some(a), Some(b), Some(v)) => (Some(a * v / 1e6), Some(b * v / 1e6)),
                        _ => (None, None),
                    },
                };
                // 健康度 = 满充容量 / 设计容量
                let design = read_num::<f64>(&dir.join("charge_full_design"))
                    .or_else(|| read_num::<f64>(&dir.join("energy_full_design")));
                let health_pct = c_full
                    .or(e_full)
                    .zip(design)
                    .filter(|(f, _)| *f > 0.0)
                    .and_then(|(f, d)| {
                        let h = f / d * 100.0;
                        (1.0..150.0).contains(&h).then_some(h)
                    });
                // 续航估算: 剩余电荷 / 当前放电电流
                let runtime_min = if status == "Discharging" {
                    let now_ah = c_now
                        .map(|v| v / 1e6)
                        .or_else(|| e_now.zip(volts).map(|(e, v)| e / 1e6 / v));
                    match now_ah.zip(amps) {
                        Some((ah, a)) if a > 0.05 => {
                            Some(((ah / a) * 60.0).clamp(0.0, 6000.0) as u64)
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                batt = Some(BatterySnapshot {
                    status,
                    capacity_pct,
                    volts,
                    amps,
                    watts,
                    cycles,
                    energy_now_wh,
                    energy_full_wh,
                    health_pct,
                    runtime_min,
                });
            }
            "Mains" | "USB" | "ADP" | "UPS" if ac.is_none() => {
                ac = read_num::<u32>(&dir.join("online")).map(|v| v == 1);
            }
            _ => {}
        }
    }
    (batt, ac)
}

fn read_vram(dev: &Path) -> Option<VramSnapshot> {
    let total_mib =
        read_num::<f64>(&dev.join("mem_info_vram_total")).unwrap_or(0.0) / 1048576.0;
    let used_mib = read_num::<f64>(&dev.join("mem_info_vram_used")).unwrap_or(0.0) / 1048576.0;
    let gtt_used_mib = read_num::<f64>(&dev.join("mem_info_gtt_used")).map(|v| v / 1048576.0);
    Some(VramSnapshot {
        used_mib,
        total_mib,
        gtt_used_mib,
    })
}

fn read_uptime_load() -> (f64, [f32; 3]) {
    let uptime_s = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let mut load = [0.0f32; 3];
    if let Some(l) = fs::read_to_string("/proc/loadavg").ok() {
        for (i, v) in l.split_whitespace().take(3).enumerate() {
            load[i] = v.parse().unwrap_or(0.0);
        }
    }
    (uptime_s, load)
}

/// hwmon 一次性枚举结果: 路径 + 标签 + 单位/缩放, 采样周期只需重读数值
struct ChipProbe {
    name: String,
    items: Vec<ReadingProbe>,
}

struct ReadingProbe {
    path: PathBuf,
    label: String,
    unit: Unit,
    scale: f64,
}

fn enum_hwmon() -> Vec<ChipProbe> {
    let mut chips = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else {
        return chips;
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = read_trimmed(&dir.join("name")) else {
            continue;
        };
        let Ok(files) = fs::read_dir(&dir) else {
            continue;
        };
        let mut nums: Vec<(String, u32)> = Vec::new();
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            if let Some(rest) = fname.strip_prefix("pwm") {
                if let Ok(n) = rest.parse::<u32>() {
                    nums.push(("pwm".into(), n));
                }
                continue;
            }
            // 形如 temp1_input / in0_input / power1_average: 前缀 = 类别+序号
            let Some(base) = fname.strip_suffix("_input") else {
                continue;
            };
            let kind = base.trim_end_matches(|c: char| c.is_ascii_digit());
            let Ok(n) = base[kind.len()..].parse::<u32>() else {
                continue;
            };
            if !matches!(kind, "temp" | "in" | "curr" | "power" | "fan" | "energy") {
                continue;
            }
            nums.push((kind.into(), n));
        }
        nums.sort();
        nums.dedup();
        let mut items = Vec::new();
        for (kind, n) in nums {
            let (unit, scale) = match kind.as_str() {
                "temp" => (Unit::TempC, 1000.0),
                "in" => (Unit::Volts, 1000.0),
                "curr" => (Unit::Amps, 1000.0),
                "power" => (Unit::Watts, 1_000_000.0),
                "fan" => (Unit::Rpm, 1.0),
                "energy" => (Unit::Joules, 1_000_000.0),
                _ => (Unit::Pct, 255.0),
            };
            let (file, label) = if kind == "pwm" {
                (format!("pwm{n}"), format!("pwm{n}"))
            } else {
                (
                    format!("{kind}{n}_input"),
                    read_trimmed(&dir.join(format!("{kind}{n}_label")))
                        .unwrap_or_else(|| format!("{kind}{n}")),
                )
            };
            items.push(ReadingProbe {
                path: dir.join(file),
                label,
                unit,
                scale,
            });
        }
        if items.is_empty() {
            continue;
        }
        items.sort_by(|a, b| a.label.cmp(&b.label));
        chips.push(ChipProbe { name, items });
    }
    chips
}

/// amdgpu DPM 状态与占用率 (pp_dpm_* 中带 * 的是当前档位; 时钟门控时无标记, 报最高档 _max)
fn read_amdgpu_dpm(dev: &Path) -> Vec<Reading> {
    let mut out = Vec::new();
    for (file, label) in [("gpu_busy_percent", "busy"), ("vcn_busy_percent", "vcn_busy")] {
        if let Some(v) = read_num::<f64>(&dev.join(file)) {
            out.push(Reading {
                label: label.into(),
                value: v,
                unit: Unit::Pct,
            });
        }
    }
    for (file, label) in [
        ("pp_dpm_sclk", "sclk"),
        ("pp_dpm_mclk", "mclk"),
        ("pp_dpm_socclk", "socclk"),
        ("pp_dpm_fclk", "fclk"),
        ("pp_dpm_vclk", "vclk"),
        ("pp_dpm_dclk", "dclk"),
        ("pp_dpm_dcefclk", "dcefclk"),
    ] {
        let Some(text) = read_trimmed(&dev.join(file)) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        // 键名稳定: 当前档位恒为 {label} (门控时 0), 最高档恒为 {label}_max
        let cur = dpm_current_line(&text);
        let max = dpm_values(&text).into_iter().reduce(f64::max);
        out.push(Reading {
            label: label.into(),
            value: cur.unwrap_or(0.0),
            unit: Unit::Mhz,
        });
        if let Some(max) = max {
            out.push(Reading {
                label: format!("{label}_max"),
                value: max,
                unit: Unit::Mhz,
            });
        }
    }
    out
}

fn dpm_value_of(line: &str) -> Option<f64> {
    line.split_once(':')?
        .1
        .trim()
        .trim_end_matches('*')
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .trim()
        .parse()
        .ok()
}

fn dpm_current_line(text: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.contains('*'))
        .and_then(dpm_value_of)
}

fn dpm_values(text: &str) -> Vec<f64> {
    text.lines().filter_map(dpm_value_of).collect()
}

/// 块设备读写速率 (MiB/s) 与繁忙度 (%), delta 基于 /proc/diskstats
fn read_disk_io(prev: &mut HashMap<String, (u64, u64, u64, Instant)>) -> Vec<Chip> {
    let mut chips = Vec::new();
    let Ok(s) = fs::read_to_string("/proc/diskstats") else {
        return chips;
    };
    for line in s.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let dev = f[2];
        // 整盘 nvmeXnY, 排除分区 nvmeXnYpZ
        if !(dev.starts_with("nvme") && !dev.contains('p')) {
            continue;
        }
        let Ok(sec_r) = f[5].parse::<u64>() else { continue };
        let Ok(sec_w) = f[9].parse::<u64>() else { continue };
        let Ok(io_ticks) = f[12].parse::<u64>() else { continue };
        let now = Instant::now();
        let out = match prev.get(dev) {
            Some((pr, pw, pt, at)) => {
                let dt = at.elapsed().as_secs_f64();
                if dt < 0.05 {
                    None
                } else {
                    Some((
                        sec_r.saturating_sub(*pr) as f64 * 512.0 / dt / 1048576.0,
                        sec_w.saturating_sub(*pw) as f64 * 512.0 / dt / 1048576.0,
                        (io_ticks.saturating_sub(*pt) as f64 / (dt * 1000.0) * 100.0)
                            .clamp(0.0, 100.0),
                    ))
                }
            }
            None => None,
        };
        prev.insert(dev.to_string(), (sec_r, sec_w, io_ticks, now));
        if let Some((r, w, busy)) = out {
            chips.push(Chip {
                name: format!("{dev} IO"),
                readings: vec![
                    Reading {
                        label: "read".into(),
                        value: r,
                        unit: Unit::RateMBs,
                    },
                    Reading {
                        label: "write".into(),
                        value: w,
                        unit: Unit::RateMBs,
                    },
                    Reading {
                        label: "busy".into(),
                        value: busy,
                        unit: Unit::Pct,
                    },
                ],
            });
        }
    }
    chips
}

/// 网卡收发速率 (MiB/s) 与 Wi-Fi 信号 (dBm)
fn read_net_io(prev: &mut HashMap<String, (u64, u64, Instant)>) -> Vec<Chip> {
    let mut readings: Vec<Reading> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut ifaces: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    ifaces.sort();
    for dir in ifaces {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if name == "lo" {
            continue;
        }
        seen.push(name.clone());
        let rx = read_num::<u64>(&dir.join("statistics/rx_bytes")).unwrap_or(0);
        let tx = read_num::<u64>(&dir.join("statistics/tx_bytes")).unwrap_or(0);
        let now = Instant::now();
        let rates = match prev.get(&name) {
            Some((pr, pw, at)) => {
                let dt = at.elapsed().as_secs_f64();
                if dt < 0.05 {
                    None
                } else {
                    Some((
                        rx.saturating_sub(*pr) as f64 / dt / 1048576.0,
                        tx.saturating_sub(*pw) as f64 / dt / 1048576.0,
                    ))
                }
            }
            None => None,
        };
        prev.insert(name.clone(), (rx, tx, now));
        if let Some((r, w)) = rates {
            readings.push(Reading {
                label: format!("{name} down"),
                value: r,
                unit: Unit::RateMBs,
            });
            readings.push(Reading {
                label: format!("{name} up"),
                value: w,
                unit: Unit::RateMBs,
            });
        }
    }
    // /proc/net/wireless: "wlp98s0: 0000 59. -51. -256 ..."
    if let Ok(s) = fs::read_to_string("/proc/net/wireless") {
        for line in s.lines().skip(2) {
            let Some((name_raw, rest)) = line.split_once(':') else { continue };
            let name = name_raw.trim();
            if !seen.iter().any(|i| i == name) {
                continue;
            }
            let f: Vec<&str> = rest.split_whitespace().collect();
            if let Some(dbm) = f.get(2).and_then(|v| v.parse::<f64>().ok()) {
                readings.push(Reading {
                    label: format!("{name} rssi"),
                    value: dbm,
                    unit: Unit::Dbm,
                });
            }
        }
    }
    if readings.is_empty() {
        Vec::new()
    } else {
        vec![Chip {
            name: "net".into(),
            readings,
        }]
    }
}

/// PSI 压力信息: 资源阻塞时间占比 (avg10, %)
fn read_psi() -> Vec<Reading> {
    let mut out = Vec::new();
    for (file, name) in [("cpu", "cpu"), ("memory", "mem"), ("io", "io")] {
        let Ok(s) = fs::read_to_string(format!("/proc/pressure/{file}")) else {
            continue;
        };
        for line in s.lines() {
            let Some((kind, rest)) = line.split_once(' ') else { continue };
            let Some(avg10) = rest
                .split_whitespace()
                .find_map(|t| t.strip_prefix("avg10="))
            else {
                continue;
            };
            let Ok(v) = avg10.parse::<f64>() else { continue };
            let label = if kind == "some" {
                name.to_string()
            } else {
                format!("{name}_full")
            };
            out.push(Reading {
                label,
                value: v,
                unit: Unit::Pct,
            });
        }
    }
    out
}

/// 自研模块 rustinfo_smu 的 pm_table (debugfs, 每次 cat 触发全新传输)。
/// 偏移来自 ryzenadj 上游 api.c 的 FAM_STRIXPOINT 字段映射
/// (表版本 0x5D0009, 大小 0xD54), 并经本机 k10temp/RAPL/amdgpu 交叉验证。
/// 0x98 附近 (cclk/socket_power) 存在二义性, 0x540+ 的 L3/GFX 温度字段单位
/// 不明, 均不采信。
fn read_smu_pmtable() -> Vec<Reading> {
    const TABLE: &str = "/sys/kernel/debug/rustinfo_smu/table";
    let Ok(raw) = fs::read(TABLE) else {
        return Vec::new();
    };
    if raw.len() < 0x5B8 {
        return Vec::new();
    }
    let f32v = |off: usize| -> Option<f64> {
        let v = f32::from_le_bytes(raw[off..off + 4].try_into().ok()?);
        v.is_finite().then_some(v as f64)
    };
    let mut out = Vec::new();
    let mut push = |label: &str, off: usize, unit: Unit, scale: f64| {
        if let Some(v) = f32v(off) {
            if v != 0.0 {
                out.push(Reading {
                    label: label.into(),
                    value: v * scale,
                    unit,
                });
            }
        }
    };
    push("stapm_limit", 0x00, Unit::Watts, 1.0);
    push("stapm", 0x04, Unit::Watts, 1.0);
    push("fast_limit", 0x08, Unit::Watts, 1.0);
    push("fast", 0x0C, Unit::Watts, 1.0);
    push("slow_limit", 0x10, Unit::Watts, 1.0);
    push("slow", 0x14, Unit::Watts, 1.0);
    push("apu_slow_limit", 0x18, Unit::Watts, 1.0);
    push("tctl_limit", 0x58, Unit::TempC, 1.0);
    push("tctl", 0x5C, Unit::TempC, 1.0);
    push("psi0_limit", 0x40, Unit::Amps, 1.0);
    push("psi0soc_limit", 0x48, Unit::Amps, 1.0);
    push("gfx_clk", 0x5B4, Unit::Mhz, 1.0);
    push("gfx_volt", 0x5A8, Unit::Volts, 1e-3);

    // 每核数组 (逐核钉载差分破译): 槽位映射 (硬件核号, 表槽), 槽 5/8 为硅片禁用位
    const CORE_SLOTS: &[(u32, usize)] = &[
        (0, 0),
        (1, 1),
        (2, 2),
        (3, 3),
        (8, 4),
        (9, 6),
        (10, 7),
        (11, 9),
        (12, 10),
        (13, 11),
    ];
    if raw.len() >= 0x0CAC {
        for (core, slot) in CORE_SLOTS {
            let clk = f32v(0x0C1C + slot * 4);
            let watt = f32v(0x09DC + slot * 4);
            let vid = f32v(0x0C7C + slot * 4);
            if let Some(v) = clk {
                if v > 0.0 {
                    out.push(Reading {
                        label: format!("C{core:02}_clk"),
                        value: v * 0.25,
                        unit: Unit::Mhz,
                    });
                }
            }
            if let Some(v) = watt {
                if v > 0.0 {
                    out.push(Reading {
                        label: format!("C{core:02}_w"),
                        value: v,
                        unit: Unit::Watts,
                    });
                }
            }
            if let Some(v) = vid {
                if v > 0.0 {
                    out.push(Reading {
                        label: format!("C{core:02}_vid"),
                        value: v,
                        unit: Unit::Volts,
                    });
                }
            }
        }
    }
    out
}

/// ryzen_smu debugfs 遥测 (自动探测; 当前 Strix Point 尚无内核支持, 目录不存在则静默跳过)
fn read_smu() -> Vec<Chip> {
    let base = Path::new("/sys/kernel/debug/ryzen_smu");
    if !base.is_dir() {
        return Vec::new();
    }
    let mut scalars: Vec<Reading> = Vec::new();
    for f in ["mclk", "fclk"] {
        if let Some(v) = read_num::<f64>(&base.join(f)) {
            scalars.push(Reading {
                label: f.into(),
                value: v,
                unit: Unit::Mhz,
            });
        }
    }
    for (file, unit, scale) in [
        ("power", Unit::Watts, 1000.0),   // 假定 mW
        ("currents", Unit::Amps, 1000.0), // 假定 mA
    ] {
        if let Ok(s) = fs::read_to_string(base.join(file)) {
            for line in s.lines() {
                let Some((k, v)) = line.split_once(':') else { continue };
                let Ok(raw) = v
                    .trim()
                    .trim_end_matches(char::is_alphabetic)
                    .trim()
                    .parse::<f64>()
                else {
                    continue;
                };
                scalars.push(Reading {
                    label: k.trim().to_string(),
                    value: raw / scale,
                    unit,
                });
            }
        }
    }
    let mut chips = Vec::new();
    if !scalars.is_empty() {
        chips.push(Chip {
            name: "smu".into(),
            readings: scalars,
        });
    }
    if let Ok(s) = fs::read_to_string(base.join("cores")) {
        let mut per_core: Vec<(String, f64, Unit)> = Vec::new();
        for line in s.lines() {
            let mut core_idx: Option<u32> = None;
            let mut pairs: Vec<(String, f64, Unit)> = Vec::new();
            for part in line.split(',') {
                let Some((k, v)) = part.split_once(':') else { continue };
                let k = k.trim().to_ascii_lowercase();
                let Ok(num) = v.trim().parse::<f64>() else { continue };
                match k.as_str() {
                    "core" => core_idx = Some(num as u32),
                    "tmp" | "temp" => pairs.push(("Temp".into(), num, Unit::TempC)),
                    "vid" | "voltage" => pairs.push(("VID".into(), num, Unit::Volts)),
                    "clock" | "clk" | "fid" => {
                        pairs.push(("Clock".into(), num, Unit::Mhz))
                    }
                    _ => {}
                }
            }
            if let Some(n) = core_idx {
                for (k, v, u) in pairs {
                    per_core.push((format!("C{n:02}_{k}"), v, u));
                }
            }
        }
        if !per_core.is_empty() {
            chips.push(Chip {
                name: "smu-cores".into(),
                readings: per_core
                    .into_iter()
                    .map(|(label, value, unit)| Reading { label, value, unit })
                    .collect(),
            });
        }
    }
    chips
}
