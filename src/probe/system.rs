use super::files::Paths;
use super::files::{read_num, read_trimmed};
use crate::model::{BatterySnapshot, MemSnapshot};
use std::fs;
pub(super) fn read_mem(paths: &Paths) -> MemSnapshot {
    let mut m = MemSnapshot::default();
    let Ok(s) = fs::read_to_string(paths.proc("meminfo")) else {
        return m;
    };
    let val = |line: &str| -> Option<u64> { line.split_whitespace().nth(1)?.parse().ok() };
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

pub(super) fn read_battery_and_ac(paths: &Paths) -> (Option<BatterySnapshot>, Option<bool>) {
    let mut batt = None;
    let mut ac = None;
    let mut ac_usb = None;
    let Ok(entries) = fs::read_dir(paths.sys("class/power_supply")) else {
        return (None, None);
    };
    let mut directories = entries
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    directories.sort();
    for dir in directories {
        match read_trimmed(&dir.join("type")).unwrap_or_default().as_str() {
            "Battery" if batt.is_none() => {
                let status = read_trimmed(&dir.join("status")).unwrap_or_default();
                let capacity_pct = read_num::<f64>(&dir.join("capacity"));
                let volts = read_num::<f64>(&dir.join("voltage_now")).map(|v| v / 1e6);
                let amps = read_num::<f64>(&dir.join("current_now")).map(|v| v / 1e6);
                let watts = read_num::<f64>(&dir.join("power_now"))
                    .map(|v| v / 1e6)
                    .or_else(|| amps.zip(volts).map(|(a, v)| a * v));
                let cycles = read_num::<u32>(&dir.join("cycle_count"));
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
                // Pair capacities with a design value in the same unit; never mix uAh and uWh.
                let health_pct = ratio(c_full, read_num::<f64>(&dir.join("charge_full_design")))
                    .or_else(|| ratio(e_full, read_num::<f64>(&dir.join("energy_full_design"))));
                let runtime_min = if status == "Discharging" {
                    c_now
                        .zip(amps)
                        .filter(|(_, amps)| amps.abs() > 0.05)
                        .map(|(charge, amps)| charge / 1e6 / amps.abs() * 60.0)
                        .or_else(|| {
                            energy_now_wh
                                .zip(watts)
                                .filter(|(_, power)| power.abs() > 0.5)
                                .map(|(energy, power)| energy / power.abs() * 60.0)
                        })
                        .filter(|minutes| minutes.is_finite() && *minutes >= 0.0)
                        .map(|minutes| minutes.min(6000.0) as u64)
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
            // 市电类优先; USB-C PD 口插外设也会 online=1, 只作兜底
            "Mains" | "ADP" | "UPS" => {
                if let Some(online) = read_num::<u32>(&dir.join("online")) {
                    ac = Some(ac.unwrap_or(false) || online == 1);
                }
            }
            "USB" | "USB_C" | "USB_PD" => {
                if let Some(online) = read_num::<u32>(&dir.join("online")) {
                    ac_usb = Some(ac_usb.unwrap_or(false) || online == 1);
                }
            }
            _ => {}
        }
    }
    (batt, ac.or(ac_usb))
}

pub(super) fn read_uptime_load(paths: &Paths) -> (f64, [f32; 3]) {
    let uptime_s = fs::read_to_string(paths.proc("uptime"))
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let mut load = [0.0f32; 3];
    if let Ok(l) = fs::read_to_string(paths.proc("loadavg")) {
        for (i, v) in l.split_whitespace().take(3).enumerate() {
            load[i] = v.parse().unwrap_or(0.0);
        }
    }
    (uptime_s, load)
}

fn ratio(full: Option<f64>, design: Option<f64>) -> Option<f64> {
    let (full, design) = full.zip(design)?;
    if full <= 0.0 || design <= 0.0 {
        return None;
    }
    let percentage = full / design * 100.0;
    (percentage.is_finite() && (1.0..150.0).contains(&percentage)).then_some(percentage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Fixture;
    fn paths(f: &Fixture) -> Paths {
        Paths {
            proc_root: f.path().join("proc"),
            sys_root: f.path().join("sys"),
            dev_root: f.path().join("dev"),
        }
    }
    #[test]
    fn battery_health_never_pairs_charge_with_an_energy_design_value() {
        let f = Fixture::new();
        for (name, value) in [
            ("type", "Battery"),
            ("charge_full", "2000000"),
            ("energy_full_design", "5000000"),
        ] {
            f.write(&format!("sys/class/power_supply/BAT0/{name}"), value);
        }
        let battery = read_battery_and_ac(&paths(&f)).0.unwrap();
        assert_eq!(battery.health_pct, None);
        f.write("sys/class/power_supply/BAT0/charge_full_design", "2500000");
        assert_eq!(
            read_battery_and_ac(&paths(&f)).0.unwrap().health_pct,
            Some(80.0)
        );
    }
    #[test]
    fn energy_only_batteries_can_estimate_discharge_runtime() {
        let f = Fixture::new();
        for (name, value) in [
            ("type", "Battery"),
            ("status", "Discharging"),
            ("energy_now", "10000000"),
            ("energy_full", "20000000"),
            ("power_now", "10000000"),
        ] {
            f.write(&format!("sys/class/power_supply/BAT0/{name}"), value);
        }
        assert_eq!(
            read_battery_and_ac(&paths(&f)).0.unwrap().runtime_min,
            Some(60)
        );
    }
    #[test]
    fn any_online_mains_input_wins_over_an_offline_input_and_usb() {
        let f = Fixture::new();
        for (dir, kind, value) in [
            ("AC0", "Mains", "0"),
            ("AC1", "Mains", "1"),
            ("USB0", "USB", "0"),
        ] {
            f.write(&format!("sys/class/power_supply/{dir}/type"), kind);
            f.write(&format!("sys/class/power_supply/{dir}/online"), value);
        }
        assert_eq!(read_battery_and_ac(&paths(&f)).1, Some(true));
    }
}
