use super::files::{read_num, read_trimmed, Paths};
use super::sample::CounterRate;
use crate::model::{Reading, Unit};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::time::Instant;

const MSR_RAPL_POWER_UNIT: u64 = 0xC001_0299;
const MSR_CORE_ENERGY: u64 = 0xC001_029A;

struct MsrCore {
    label: String,
    file: File,
    rate: CounterRate<1>,
}
struct MsrState {
    cores: Vec<MsrCore>,
    j_per_lsb: f64,
}

impl MsrState {
    fn open(paths: &Paths, ids: &[usize]) -> Option<Self> {
        let mut groups = Vec::new();
        let mut seen = HashSet::new();
        for &id in ids {
            let topology = paths.sys(&format!("devices/system/cpu/cpu{id}/topology"));
            let (Some(core), Some(siblings)) = (
                read_trimmed(&topology.join("core_id")),
                read_trimmed(&topology.join("thread_siblings_list")),
            ) else {
                continue;
            };
            if seen.contains(&siblings) {
                continue;
            }
            let package =
                read_trimmed(&topology.join("physical_package_id")).unwrap_or_else(|| "0".into());
            let Ok(file) = File::open(paths.dev(&format!("cpu/{id}/msr"))) else {
                continue;
            };
            seen.insert(siblings);
            groups.push((package, core, file));
        }
        let mut units = [0u8; 8];
        let raw = groups.iter().find_map(|(_, _, file)| {
            file.read_exact_at(&mut units, MSR_RAPL_POWER_UNIT).ok()?;
            Some(u64::from_le_bytes(units))
        })?;
        let j_per_lsb = 2f64.powi(-(((raw >> 8) & 0x1f) as i32));
        let multiple_packages = groups
            .iter()
            .map(|(package, _, _)| package)
            .collect::<HashSet<_>>()
            .len()
            > 1;
        let cores = groups
            .into_iter()
            .map(|(package, core, file)| MsrCore {
                label: if multiple_packages {
                    format!("P{package}_C{core:0>2}")
                } else {
                    format!("C{core:0>2}")
                },
                file,
                rate: CounterRate::default(),
            })
            .collect();
        Some(Self { cores, j_per_lsb })
    }

    fn read(&mut self, now: Instant) -> Vec<Reading> {
        let mut readings = Vec::new();
        for core in &mut self.cores {
            let mut bytes = [0u8; 8];
            if core
                .file
                .read_exact_at(&mut bytes, MSR_CORE_ENERGY)
                .is_err()
            {
                core.rate.invalidate();
                continue;
            }
            let energy = u64::from_le_bytes(bytes) & 0xffff_ffff;
            if let Some([rate]) = core.rate.update([energy], now, [Some(1u64 << 32)]) {
                let watts = rate * self.j_per_lsb;
                if (0.0..200.0).contains(&watts) {
                    readings.push(Reading {
                        key: None,
                        label: core.label.clone(),
                        value: watts,
                        unit: Unit::Watts,
                    });
                }
            }
        }
        readings
    }
}

#[derive(Default)]
pub(super) struct PowerSampler {
    rapl: HashMap<String, CounterRate<1>>,
    msr: Option<MsrState>,
    last_msr_probe: Option<Instant>,
    cpu_ids: Vec<usize>,
}

impl PowerSampler {
    pub fn read(&mut self, paths: &Paths, ids: &[usize], amd: bool, now: Instant) -> Vec<Reading> {
        let mut readings = Vec::new();
        let mut seen = HashSet::new();
        let mut packages = HashSet::new();
        if let Ok(entries) = fs::read_dir(paths.sys("class/powercap")) {
            let mut zones = entries.flatten().map(|e| e.path()).collect::<Vec<_>>();
            zones.sort();
            let mut valid = Vec::new();
            for dir in zones {
                let Some(name) =
                    read_trimmed(&dir.join("name")).filter(|name| name.starts_with("package"))
                else {
                    continue;
                };
                if packages.contains(&name) {
                    continue;
                }
                let id = dir.to_string_lossy().into_owned();
                seen.insert(id.clone());
                let state = self.rapl.entry(id).or_default();
                let Some(energy) = read_num::<u64>(&dir.join("energy_uj")) else {
                    state.invalidate();
                    continue;
                };
                packages.insert(name.clone());
                let range = read_num::<u64>(&dir.join("max_energy_range_uj")).filter(|v| *v > 0);
                if let Some([rate]) = state.update([energy], now, [range]) {
                    let watts = rate / 1e6;
                    if (0.0..500.0).contains(&watts) {
                        valid.push((name, watts));
                    }
                }
            }
            let multiple = packages.len() > 1;
            readings.extend(valid.into_iter().map(|(name, value)| Reading {
                key: Some(name.clone()),
                label: if multiple {
                    format!("Package ({name})")
                } else {
                    "Package".into()
                },
                value,
                unit: Unit::Watts,
            }));
        }
        self.rapl.retain(|id, _| seen.contains(id));
        // Unsupported/missing MSR files are retried occasionally, not on every poll.
        if amd
            && (ids != self.cpu_ids
                || (self.msr.is_none()
                    && self
                        .last_msr_probe
                        .is_none_or(|at| now.saturating_duration_since(at).as_secs() >= 10)))
        {
            self.msr = MsrState::open(paths, ids);
            self.last_msr_probe = Some(now);
            self.cpu_ids = ids.to_vec();
        }
        if let Some(msr) = &mut self.msr {
            readings.extend(msr.read(now));
        }
        readings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use std::time::Duration;
    #[test]
    fn two_interfaces_for_one_rapl_package_are_not_double_counted() {
        let f = Fixture::new();
        for zone in ["intel-rapl:0", "intel-rapl-mmio:0"] {
            f.write(&format!("sys/class/powercap/{zone}/name"), "package-0");
            f.write(&format!("sys/class/powercap/{zone}/energy_uj"), "0");
        }
        let paths = Paths {
            proc_root: f.path().join("proc"),
            sys_root: f.path().join("sys"),
            dev_root: f.path().join("dev"),
        };
        let mut sampler = PowerSampler::default();
        let start = Instant::now();
        assert!(sampler.read(&paths, &[], false, start).is_empty());
        for zone in ["intel-rapl:0", "intel-rapl-mmio:0"] {
            f.write(&format!("sys/class/powercap/{zone}/energy_uj"), "1000000");
        }
        let readings = sampler.read(&paths, &[], false, start + Duration::from_secs(1));
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, 1.0);
    }
}
