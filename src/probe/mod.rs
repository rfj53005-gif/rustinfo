//! Collectors own their parsing and rate state; this module assembles snapshots.
mod cpu;
mod files;
mod gpu;
mod hwmon;
mod io;
mod power;
mod sample;
mod system;
mod telemetry;

use crate::model::{Chip, Snapshot};
use chrono::Local;
use cpu::CpuSampler;
use files::{read_trimmed, Paths};
use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;

pub struct Prober {
    paths: Paths,
    hostname: String,
    kernel: String,
    cpu: CpuSampler,
    power: power::PowerSampler,
    hwmon: hwmon::Hwmon,
    io: io::IoSampler,
    gpu_device: Option<PathBuf>,
    drm: Option<File>,
}

impl Prober {
    pub fn new() -> Self {
        Self::with_paths(Paths::default())
    }

    fn with_paths(paths: Paths) -> Self {
        let device = gpu::find_device(&paths);
        let drm = device.as_ref().and_then(|(_, node)| File::open(node).ok());
        Self {
            hostname: read_trimmed(&paths.proc("sys/kernel/hostname")).unwrap_or_default(),
            kernel: read_trimmed(&paths.proc("sys/kernel/osrelease")).unwrap_or_default(),
            cpu: CpuSampler::new(&paths),
            power: power::PowerSampler::default(),
            hwmon: hwmon::Hwmon::default(),
            io: io::IoSampler::default(),
            gpu_device: device.map(|(dev, _)| dev),
            drm,
            paths,
        }
    }

    pub fn snapshot(&mut self) -> Snapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&mut self, now: Instant) -> Snapshot {
        let mut cpu = self.cpu.snapshot(&self.paths, now);
        let readings = self
            .power
            .read(&self.paths, &cpu.ids, self.cpu.is_amd(), now);
        cpu.pkg_w = readings
            .iter()
            .filter(|r| r.label.starts_with("Package"))
            .map(|r| r.value)
            .reduce(|a, b| a + b);
        let mut chips = self.hwmon.read(&self.paths, now);
        push_chip(&mut chips, "rapl", readings);
        push_chip(
            &mut chips,
            "amdgpu-sensor",
            self.drm
                .as_ref()
                .map(gpu::read_drm_sensors)
                .unwrap_or_default(),
        );
        push_chip(
            &mut chips,
            "amdgpu-dpm",
            self.gpu_device
                .as_deref()
                .map(gpu::read_amdgpu_dpm)
                .unwrap_or_default(),
        );
        chips.extend(self.io.read(&self.paths, now));
        push_chip(&mut chips, "psi", telemetry::read_psi(&self.paths));
        push_chip(&mut chips, "smu", telemetry::read_smu_pmtable(&self.paths));
        chips.extend(telemetry::read_smu(&self.paths));
        for chip in &mut chips {
            chip.readings.retain(|reading| reading.value.is_finite());
        }
        chips.retain(|chip| !chip.readings.is_empty());
        let (uptime_s, load) = system::read_uptime_load(&self.paths);
        let (battery, ac_online) = system::read_battery_and_ac(&self.paths);
        Snapshot {
            time: Local::now(),
            hostname: self.hostname.clone(),
            kernel: self.kernel.clone(),
            uptime_s,
            load,
            cpu,
            mem: system::read_mem(&self.paths),
            battery,
            ac_online,
            vram: self.gpu_device.as_deref().and_then(gpu::read_vram),
            chips,
        }
    }
}

impl Default for Prober {
    fn default() -> Self {
        Self::new()
    }
}

fn push_chip(chips: &mut Vec<Chip>, name: &str, readings: Vec<crate::model::Reading>) {
    if !readings.is_empty() {
        chips.push(Chip {
            id: name.into(),
            name: name.into(),
            readings,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    use std::time::Duration;
    #[test]
    fn assembled_snapshots_use_fixture_roots_and_real_cpu_ids() {
        let fixture = Fixture::new();
        fixture.write(
            "proc/cpuinfo",
            "vendor_id: AuthenticAMD\ncpu family: 26\nmodel: 36\nmodel name: Test CPU\n",
        );
        fixture.write("proc/stat", "cpu 0 0 0 100\ncpu2 0 0 0 100\n");
        fixture.write("proc/meminfo", "MemTotal: 1024 kB\nMemAvailable: 512 kB\n");
        fixture.write(
            "sys/devices/system/cpu/cpu2/cpufreq/scaling_cur_freq",
            "2000000\n",
        );
        let paths = Paths {
            proc_root: fixture.path().join("proc"),
            sys_root: fixture.path().join("sys"),
            dev_root: fixture.path().join("dev"),
        };
        let mut prober = Prober::with_paths(paths);
        let start = Instant::now();
        let first = prober.snapshot_at(start);
        assert_eq!(first.cpu.ids, vec![2]);
        assert_eq!(first.cpu.per_mhz, vec![2000.0]);
        fixture.write("proc/stat", "cpu 50 0 0 150\ncpu2 50 0 0 150\n");
        let second = prober.snapshot_at(start + Duration::from_secs(1));
        assert_eq!(second.cpu.total_pct, 50.0);
        assert_eq!(second.mem.avail_kib, 512);
        assert!(second.chips.is_empty());
    }
}
