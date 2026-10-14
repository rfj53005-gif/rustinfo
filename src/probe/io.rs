use super::files::{read_num, Paths};
use super::sample::CounterRate;
use crate::model::{Chip, Reading, Unit};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

#[derive(Default)]
pub(super) struct IoSampler {
    disk: HashMap<String, CounterRate<3>>,
    net: HashMap<String, CounterRate<2>>,
}

impl IoSampler {
    pub fn read(&mut self, paths: &Paths, now: Instant) -> Vec<Chip> {
        let mut chips = self.disks(paths, now);
        let net = self.network(paths, now);
        if !net.is_empty() {
            chips.push(Chip {
                id: "net".into(),
                name: "net".into(),
                readings: net,
            });
        }
        chips
    }

    fn disks(&mut self, paths: &Paths, now: Instant) -> Vec<Chip> {
        let Ok(text) = fs::read_to_string(paths.proc("diskstats")) else {
            self.disk.clear();
            return Vec::new();
        };
        let mut chips = Vec::new();
        let mut seen = HashSet::new();
        for line in text.lines() {
            let parts = line.split_whitespace().collect::<Vec<_>>();
            if parts.len() < 14 {
                continue;
            }
            let name = parts[2];
            let block = paths.sys(&format!("class/block/{name}"));
            // The kernel tells us whether this is a partition, including vdX and future device types.
            if !block.join("device").exists() || block.join("partition").exists() {
                continue;
            }
            seen.insert(name.to_string());
            let state = self.disk.entry(name.into()).or_default();
            let parsed = [parts[5], parts[9], parts[12]].map(str::parse::<u64>);
            let [Ok(read), Ok(write), Ok(busy)] = parsed else {
                state.invalidate();
                continue;
            };
            if let Some([r, w, b]) = state.update([read, write, busy], now, [None; 3]) {
                let readings = vec![
                    reading("read", r * 512.0 / 1048576.0, Unit::RateMBs),
                    reading("write", w * 512.0 / 1048576.0, Unit::RateMBs),
                    reading("busy", (b / 10.0).clamp(0.0, 100.0), Unit::Pct),
                ];
                chips.push(Chip {
                    id: format!("disk:{name}"),
                    name: format!("{name} IO"),
                    readings,
                });
            }
        }
        self.disk.retain(|id, _| seen.contains(id));
        chips
    }

    fn network(&mut self, paths: &Paths, now: Instant) -> Vec<Reading> {
        let Ok(entries) = fs::read_dir(paths.sys("class/net")) else {
            self.net.clear();
            return Vec::new();
        };
        let mut dirs = entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        dirs.sort();
        let mut seen = HashSet::new();
        let mut readings = Vec::new();
        for dir in dirs {
            let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if !dir.join("device").exists() {
                continue;
            }
            seen.insert(name.to_string());
            let state = self.net.entry(name.into()).or_default();
            let (Some(rx), Some(tx)) = (
                read_num::<u64>(&dir.join("statistics/rx_bytes")),
                read_num::<u64>(&dir.join("statistics/tx_bytes")),
            ) else {
                state.invalidate();
                continue;
            };
            if let Some([r, w]) = state.update([rx, tx], now, [None; 2]) {
                readings.push(reading(
                    &format!("{name} down"),
                    r / 1048576.0,
                    Unit::RateMBs,
                ));
                readings.push(reading(&format!("{name} up"), w / 1048576.0, Unit::RateMBs));
            }
        }
        self.net.retain(|id, _| seen.contains(id));
        if let Ok(text) = fs::read_to_string(paths.proc("net/wireless")) {
            for line in text.lines().skip(2) {
                let Some((name, rest)) = line.split_once(':') else {
                    continue;
                };
                if !seen.contains(name.trim()) {
                    continue;
                }
                if let Some(dbm) = rest
                    .split_whitespace()
                    .nth(2)
                    .and_then(|v| v.trim_end_matches('.').parse::<f64>().ok())
                    .filter(|v| v.is_finite())
                {
                    readings.push(reading(&format!("{} rssi", name.trim()), dbm, Unit::Dbm));
                }
            }
        }
        readings
    }
}

fn reading(label: &str, value: f64, unit: Unit) -> Reading {
    Reading {
        key: None,
        label: label.into(),
        value,
        unit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use std::time::Duration;
    #[test]
    fn missing_network_counters_do_not_reset_to_zero_or_generate_false_traffic() {
        let f = Fixture::new();
        f.write("sys/class/net/eth0/device/present", "");
        f.write("sys/class/net/eth0/statistics/rx_bytes", "1048576");
        f.write("sys/class/net/eth0/statistics/tx_bytes", "0");
        let paths = Paths {
            proc_root: f.path().join("proc"),
            sys_root: f.path().join("sys"),
            dev_root: f.path().join("dev"),
        };
        let mut sampler = IoSampler::default();
        let t = Instant::now();
        assert!(sampler.network(&paths, t).is_empty());
        std::fs::remove_file(f.path().join("sys/class/net/eth0/statistics/rx_bytes")).unwrap();
        assert!(sampler
            .network(&paths, t + Duration::from_secs(1))
            .is_empty());
        f.write("sys/class/net/eth0/statistics/rx_bytes", "10485760");
        assert!(sampler
            .network(&paths, t + Duration::from_secs(2))
            .is_empty());
        f.write("sys/class/net/eth0/statistics/rx_bytes", "11534336");
        assert_eq!(
            sampler.network(&paths, t + Duration::from_secs(3))[0].value,
            1.0
        );
    }
}
