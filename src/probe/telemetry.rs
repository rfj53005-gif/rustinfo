use super::files::{read_num, Paths};
use crate::model::{Chip, Reading, Unit};
use std::fs;
/// PSI 压力信息: 资源阻塞时间占比 (avg10, %)
pub(super) fn read_psi(paths: &Paths) -> Vec<Reading> {
    let mut out = Vec::new();
    for (file, name) in [("cpu", "cpu"), ("memory", "mem"), ("io", "io")] {
        let Ok(s) = fs::read_to_string(paths.proc(&format!("pressure/{file}"))) else {
            continue;
        };
        for line in s.lines() {
            let Some((kind, rest)) = line.split_once(' ') else {
                continue;
            };
            let Some(avg10) = rest
                .split_whitespace()
                .find_map(|t| t.strip_prefix("avg10="))
            else {
                continue;
            };
            let Ok(v) = avg10.parse::<f64>() else {
                continue;
            };
            let label = if kind == "some" {
                name.to_string()
            } else {
                format!("{name}_full")
            };
            out.push(Reading {
                key: None,
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
pub(super) fn read_smu_pmtable(paths: &Paths) -> Vec<Reading> {
    let info = fs::read_to_string(paths.sys("kernel/debug/rustinfo_smu/info")).unwrap_or_default();
    if !known_table_version(&info) {
        return Vec::new();
    }
    let Ok(raw) = fs::read(paths.sys("kernel/debug/rustinfo_smu/table")) else {
        return Vec::new();
    };
    parse_pmtable(&raw)
}

pub(super) fn parse_pmtable(raw: &[u8]) -> Vec<Reading> {
    let f32v = |off: usize| -> Option<f64> {
        let v = f32::from_le_bytes(raw.get(off..off + 4)?.try_into().ok()?);
        v.is_finite().then_some(v as f64)
    };
    let mut out = Vec::new();
    let mut push = |label: &str, off: usize, unit: Unit, scale: f64| {
        if let Some(v) = f32v(off) {
            if v >= 0.0 {
                out.push(Reading {
                    key: None,
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
                if v >= 0.0 {
                    out.push(Reading {
                        key: None,
                        label: format!("C{core:02}_clk"),
                        value: v * 0.25,
                        unit: Unit::Mhz,
                    });
                }
            }
            if let Some(v) = watt {
                if v >= 0.0 {
                    out.push(Reading {
                        key: None,
                        label: format!("C{core:02}_w"),
                        value: v,
                        unit: Unit::Watts,
                    });
                }
            }
            if let Some(v) = vid {
                if v >= 0.0 {
                    out.push(Reading {
                        key: None,
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
pub(super) fn read_smu(paths: &Paths) -> Vec<Chip> {
    let base = paths.sys("kernel/debug/ryzen_smu");
    if !base.is_dir() {
        return Vec::new();
    }
    let mut scalars: Vec<Reading> = Vec::new();
    for f in ["mclk", "fclk"] {
        if let Some(v) = read_num::<f64>(&base.join(f)) {
            scalars.push(Reading {
                key: None,
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
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                let Ok(raw) = v
                    .trim()
                    .trim_end_matches(char::is_alphabetic)
                    .trim()
                    .parse::<f64>()
                else {
                    continue;
                };
                scalars.push(Reading {
                    key: None,
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
            id: "ryzen_smu".into(),
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
                let Some((k, v)) = part.split_once(':') else {
                    continue;
                };
                let k = k.trim().to_ascii_lowercase();
                let Ok(num) = v.trim().parse::<f64>() else {
                    continue;
                };
                match k.as_str() {
                    "core" => core_idx = Some(num as u32),
                    "tmp" | "temp" => pairs.push(("Temp".into(), num, Unit::TempC)),
                    "vid" | "voltage" => pairs.push(("VID".into(), num, Unit::Volts)),
                    "clock" | "clk" | "fid" => pairs.push(("Clock".into(), num, Unit::Mhz)),
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
                id: "ryzen_smu-cores".into(),
                name: "smu-cores".into(),
                readings: per_core
                    .into_iter()
                    .map(|(label, value, unit)| Reading {
                        key: None,
                        label,
                        value,
                        unit,
                    })
                    .collect(),
            });
        }
    }
    chips
}

fn known_table_version(info: &str) -> bool {
    info.lines()
        .find_map(|line| line.strip_prefix("table_version=0x"))
        .and_then(|value| u32::from_str_radix(value.trim(), 16).ok())
        == Some(0x5d0009)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_firmware_tables_are_not_decoded_with_the_strix_offsets() {
        assert!(known_table_version(
            "smu_version=0x0B5D0B00\ntable_version=0x5D0009\n"
        ));
        for text in ["", "table_version=0x64020c", "table_version=bogus"] {
            assert!(!known_table_version(text));
        }
    }
    #[test]
    fn truncated_tables_and_non_finite_fields_cannot_panic() {
        for size in [0, 1, 3, 4, 0x5b7, 0xca9] {
            let _ = parse_pmtable(&vec![0; size]);
        }
        let mut bytes = vec![0; 0xcac];
        bytes[0x5c..0x60].copy_from_slice(&f32::NAN.to_le_bytes());
        let readings = parse_pmtable(&bytes);
        assert!(!readings.iter().any(|r| r.label == "tctl"));
        assert!(readings.iter().all(|r| r.value.is_finite()));
    }
    #[test]
    fn gated_core_clocks_keep_their_column_and_the_verified_slot_mapping() {
        let mut bytes = vec![0; 0xcac];
        bytes[0xc1c + 6 * 4..0xc20 + 6 * 4].copy_from_slice(&8000f32.to_le_bytes());
        let readings = parse_pmtable(&bytes);
        assert_eq!(
            readings
                .iter()
                .find(|r| r.label == "C09_clk")
                .unwrap()
                .value,
            2000.0
        );
        assert_eq!(
            readings
                .iter()
                .find(|r| r.label == "C00_clk")
                .unwrap()
                .value,
            0.0
        );
    }
}
