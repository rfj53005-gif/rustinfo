use super::files::{read_num, read_trimmed, Paths};
use super::sample::MIN_WINDOW;
use crate::model::CpuSnapshot;
use crate::platform::CpuIdentity;
use std::collections::BTreeMap;
use std::fs;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
struct Ticks {
    idle: u64,
    total: u64,
}

#[derive(Debug)]
struct CpuTicks {
    total: Ticks,
    per: BTreeMap<usize, Ticks>,
}

fn parse_stat(text: &str) -> Option<CpuTicks> {
    let mut total = None;
    let mut per = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(label) = parts.next() else { continue };
        if !label.starts_with("cpu") {
            break;
        }
        // guest and guest_nice are already included in user and nice.
        let fields = parts
            .take(8)
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if fields.len() < 4 {
            return None;
        }
        let ticks = Ticks {
            idle: fields[3].checked_add(fields.get(4).copied().unwrap_or(0))?,
            total: fields.iter().try_fold(0u64, |sum, n| sum.checked_add(*n))?,
        };
        if label == "cpu" {
            total = Some(ticks);
        } else if let Ok(id) = label[3..].parse() {
            per.insert(id, ticks);
        }
    }
    Some(CpuTicks { total: total?, per })
}

fn usage(current: Ticks, previous: Ticks) -> f32 {
    let Some(total) = current.total.checked_sub(previous.total).filter(|n| *n > 0) else {
        return 0.0;
    };
    // iowait may decrease; that must not underflow into a bogus busy percentage.
    let idle = current.idle.saturating_sub(previous.idle).min(total);
    (100.0 * (total - idle) as f64 / total as f64) as f32
}

pub(super) struct CpuSampler {
    identity: CpuIdentity,
    previous: Option<(Instant, CpuTicks)>,
    cached: (f32, BTreeMap<usize, f32>),
}

impl CpuSampler {
    pub fn new(paths: &Paths) -> Self {
        let text = fs::read_to_string(paths.proc("cpuinfo")).unwrap_or_default();
        Self {
            identity: CpuIdentity::parse(&text),
            previous: None,
            cached: (0.0, BTreeMap::new()),
        }
    }

    pub fn is_amd(&self) -> bool {
        self.identity.vendor == "AuthenticAMD"
    }

    fn update(&mut self, current: CpuTicks, now: Instant) {
        if let Some((at, previous)) = &self.previous {
            if now.saturating_duration_since(*at) < MIN_WINDOW {
                return;
            }
            self.cached.0 = usage(current.total, previous.total);
            self.cached.1 = current
                .per
                .iter()
                .map(|(&id, &ticks)| {
                    (
                        id,
                        previous
                            .per
                            .get(&id)
                            .map(|&old| usage(ticks, old))
                            .unwrap_or(0.0),
                    )
                })
                .collect();
        }
        self.previous = Some((now, current));
    }

    pub fn snapshot(&mut self, paths: &Paths, now: Instant) -> CpuSnapshot {
        let parsed = fs::read_to_string(paths.proc("stat"))
            .ok()
            .and_then(|text| parse_stat(&text));
        let ids = parsed
            .as_ref()
            .map(|t| t.per.keys().copied().collect::<Vec<_>>())
            .unwrap_or_else(|| self.cached.1.keys().copied().collect());
        if let Some(ticks) = parsed {
            self.update(ticks, now);
        }
        let per_pct = ids
            .iter()
            .map(|id| self.cached.1.get(id).copied().unwrap_or(0.0))
            .collect();
        let per_mhz = ids
            .iter()
            .map(|id| {
                read_num::<f64>(&paths.sys(&format!(
                    "devices/system/cpu/cpu{id}/cpufreq/scaling_cur_freq"
                )))
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|khz| (khz / 1000.0) as f32)
                .unwrap_or(0.0)
            })
            .collect();
        let lead = ids.first().copied().unwrap_or(0);
        let frequency = paths.sys(&format!("devices/system/cpu/cpu{lead}/cpufreq"));
        CpuSnapshot {
            model: if self.identity.brand.is_empty() {
                "未知 CPU".into()
            } else {
                self.identity.brand.clone()
            },
            ids,
            total_pct: self.cached.0,
            per_pct,
            per_mhz,
            pkg_w: None,
            governor: read_trimmed(&frequency.join("scaling_governor")).unwrap_or_default(),
            epp: read_trimmed(&frequency.join("energy_performance_preference")).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn guest_ticks_are_not_counted_twice_and_cpu_ids_can_be_sparse() {
        let ticks = parse_stat(
            "cpu 10 5 5 80 0 0 0 0 7 3\ncpu2 10 5 5 80 0 0 0 0 7 3\ncpu10 0 0 0 100\nintr 1\n",
        )
        .unwrap();
        assert_eq!(ticks.total.total, 100);
        assert_eq!(ticks.per.keys().copied().collect::<Vec<_>>(), vec![2, 10]);
        assert!(parse_stat("cpu 1 bad 3 4").is_none());
    }
    #[test]
    fn tiny_windows_do_not_consume_cpu_ticks_or_shift_a_hotplugged_cpu() {
        let start = Instant::now();
        let mut sampler = CpuSampler {
            identity: CpuIdentity::default(),
            previous: None,
            cached: (0.0, BTreeMap::new()),
        };
        sampler.update(parse_stat("cpu 0 0 0 100\ncpu2 0 0 0 100").unwrap(), start);
        sampler.update(
            parse_stat("cpu 1 0 0 100\ncpu2 1 0 0 100").unwrap(),
            start + Duration::from_millis(1),
        );
        sampler.update(
            parse_stat("cpu 50 0 0 150\ncpu2 50 0 0 150\ncpu10 99 0 0 1").unwrap(),
            start + Duration::from_secs(1),
        );
        assert_eq!(sampler.cached.0, 50.0);
        assert_eq!(sampler.cached.1[&2], 50.0);
        assert_eq!(sampler.cached.1[&10], 0.0);
    }
}
