use super::files::{read_num, read_trimmed, Paths};
use crate::model::{Reading, Unit, VramSnapshot};
use std::fs::File;
use std::path::{Path, PathBuf};
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

/// amdgpu DRM 传感器: ioctl 直读 SMU 实时值 (GFX/MCLK 时钟、负载、功率)
/// 这是 pp_dpm_mclk 之外的细粒度实时频率来源 (nvtop 同款)
pub(super) fn read_drm_sensors(f: &File) -> Vec<Reading> {
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
            // SAFETY: info and its return buffer remain live, aligned, and writable during ioctl.
            let ret = unsafe {
                libc::ioctl(
                    fd,
                    DRM_IOCTL_AMDGPU_INFO as libc::c_ulong,
                    &mut info as *mut DrmAmdgpuInfo,
                )
            };
            if ret == 0 {
                out.push(Reading {
                    key: None,
                    label: $label.into(),
                    value: val as f64 * $scale,
                    unit: $unit,
                });
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

pub(super) fn find_device(paths: &Paths) -> Option<(PathBuf, PathBuf)> {
    let mut entries = std::fs::read_dir(paths.sys("class/drm"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect::<Vec<_>>();
    entries.sort();
    for card in entries {
        let name = card.file_name()?.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        let device = card.join("device");
        if device.join("mem_info_vram_total").exists() {
            return Some((device, paths.dev(&format!("dri/{name}"))));
        }
    }
    None
}

pub(super) fn read_vram(dev: &Path) -> Option<VramSnapshot> {
    let total_mib = read_num::<f64>(&dev.join("mem_info_vram_total")).unwrap_or(0.0) / 1048576.0;
    let used_mib = read_num::<f64>(&dev.join("mem_info_vram_used")).unwrap_or(0.0) / 1048576.0;
    let gtt_used_mib = read_num::<f64>(&dev.join("mem_info_gtt_used")).map(|v| v / 1048576.0);
    Some(VramSnapshot {
        used_mib,
        total_mib,
        gtt_used_mib,
    })
}

/// amdgpu DPM 状态与占用率 (pp_dpm_* 中带 * 的是当前档位; 时钟门控时无标记, 报最高档 _max)
pub(super) fn read_amdgpu_dpm(dev: &Path) -> Vec<Reading> {
    let mut out = Vec::new();
    for (file, label) in [
        ("gpu_busy_percent", "busy"),
        ("vcn_busy_percent", "vcn_busy"),
    ] {
        if let Some(v) = read_num::<f64>(&dev.join(file)) {
            out.push(Reading {
                key: None,
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
            key: None,
            label: label.into(),
            value: cur.unwrap_or(0.0),
            unit: Unit::Mhz,
        });
        if let Some(max) = max {
            out.push(Reading {
                key: None,
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
