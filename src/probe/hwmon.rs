use super::files::{read_num, read_trimmed, Paths};
use crate::model::{Chip, Reading, Unit};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};
/// hwmon 一次性枚举结果: 路径 + 标签 + 单位/缩放, 采样周期只需重读数值
struct ChipProbe {
    id: String,
    name: String,
    items: Vec<ReadingProbe>,
}

struct ReadingProbe {
    path: PathBuf,
    label: String,
    unit: Unit,
    scale: f64,
}

fn enum_hwmon(paths: &Paths) -> Vec<ChipProbe> {
    let mut chips = Vec::new();
    let Ok(entries) = fs::read_dir(paths.sys("class/hwmon")) else {
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
            let Some(base) = fname
                .strip_suffix("_input")
                .or_else(|| fname.strip_suffix("_average"))
            else {
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
                _ => (Unit::Pct, 2.55),
            };
            let (file, label) = if kind == "pwm" {
                (format!("pwm{n}"), format!("pwm{n}"))
            } else {
                (
                    if dir.join(format!("{kind}{n}_input")).exists() {
                        format!("{kind}{n}_input")
                    } else {
                        format!("{kind}{n}_average")
                    },
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
        let identity = fs::canonicalize(dir.join("device"))
            .or_else(|_| fs::canonicalize(&dir))
            .unwrap_or(dir);
        chips.push(ChipProbe {
            id: format!("hwmon:{}", identity.display()),
            name,
            items,
        });
    }
    chips
}

#[derive(Default)]
pub(super) struct Hwmon {
    probes: Vec<ChipProbe>,
    last_scan: Option<Instant>,
}
impl Hwmon {
    pub fn read(&mut self, paths: &Paths, now: Instant) -> Vec<Chip> {
        if self
            .last_scan
            .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(10))
        {
            self.probes = enum_hwmon(paths);
            self.last_scan = Some(now);
        }
        self.probes
            .iter()
            .filter_map(|probe| {
                let readings = probe
                    .items
                    .iter()
                    .filter_map(|item| {
                        let raw = read_num::<f64>(&item.path)?;
                        if !raw.is_finite() {
                            return None;
                        }
                        let value = raw / item.scale;
                        Some(Reading {
                            key: item
                                .path
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned()),
                            label: item.label.clone(),
                            value,
                            unit: item.unit,
                        })
                    })
                    .collect::<Vec<_>>();
                (!readings.is_empty()).then(|| Chip {
                    id: probe.id.clone(),
                    name: probe.name.clone(),
                    readings,
                })
            })
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    #[test]
    fn pwm_is_a_percentage_and_average_only_power_is_discovered() {
        let f = Fixture::new();
        f.write("sys/class/hwmon/hwmon0/name", "test");
        f.write("sys/class/hwmon/hwmon0/pwm1", "255");
        f.write("sys/class/hwmon/hwmon0/power1_average", "12500000");
        let paths = Paths {
            proc_root: f.path().join("proc"),
            sys_root: f.path().join("sys"),
            dev_root: f.path().join("dev"),
        };
        let chip = Hwmon::default().read(&paths, Instant::now()).remove(0);
        let pwm = chip.readings.iter().find(|r| r.label == "pwm1").unwrap();
        assert!((pwm.value - 100.0).abs() < 1e-9);
        assert_eq!(
            chip.readings
                .iter()
                .find(|r| r.label == "power1")
                .unwrap()
                .value,
            12.5
        );
    }
}
