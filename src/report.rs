use crate::{probe::Prober, ui};
use anyhow::Result;
use std::time::Duration;

pub fn run_dump() -> Result<()> {
    let mut prober = Prober::new();
    let _ = prober.snapshot(); // 预热一次, 占用率/RAPL 功率需要增量
    std::thread::sleep(Duration::from_millis(600));
    let s = prober.snapshot();
    println!(
        "rustinfo v{} — {}",
        env!("CARGO_PKG_VERSION"),
        s.time.format("%Y-%m-%d %H:%M:%S")
    );
    println!(
        "主机 {}  内核 {}  运行 {}  负载 {:.2} {:.2} {:.2}",
        s.hostname,
        s.kernel,
        ui::fmt_uptime(s.uptime_s),
        s.load[0],
        s.load[1],
        s.load[2]
    );
    let avg_mhz = if s.cpu.per_mhz.is_empty() {
        0.0
    } else {
        s.cpu.per_mhz.iter().sum::<f32>() / s.cpu.per_mhz.len() as f32
    };
    print!(
        "CPU {}  {}线程  占用 {:.1}%  均 {:.0}MHz",
        s.cpu.model,
        s.cpu.per_mhz.len(),
        s.cpu.total_pct,
        avg_mhz
    );
    if let Some(w) = s.cpu.pkg_w {
        print!("  Package {:.2}W", w);
    }
    println!();
    let mem_used_g = s.mem.total_kib.saturating_sub(s.mem.avail_kib) as f64 / 1048576.0;
    println!(
        "内存 {:.1}/{:.1} GiB  Swap {:.1}/{:.1} GiB",
        mem_used_g,
        s.mem.total_kib as f64 / 1048576.0,
        s.mem.swap_total_kib.saturating_sub(s.mem.swap_free_kib) as f64 / 1048576.0,
        s.mem.swap_total_kib as f64 / 1048576.0
    );
    if let Some(b) = &s.battery {
        print!(
            "电池 {} {:.0}%",
            ui::batt_status_zh(&b.status),
            b.capacity_pct.unwrap_or(0.0)
        );
        if let Some(v) = b.volts {
            print!(" {:.2}V", v);
        }
        if let Some(a) = b.amps {
            print!(" {:.2}A", a);
        }
        if let Some(w) = b.watts {
            print!(" {:.1}W", w);
        }
        if let Some(c) = b.cycles {
            print!(" (循环 {c})");
        }
        if let Some(h) = b.health_pct {
            print!(" 健康{:.0}%", h);
        }
        if let Some(m) = b.runtime_min {
            print!(" 续航~{}h{:02}m", m / 60, m % 60);
        }
        println!();
    }
    if let Some(v) = &s.vram {
        println!(
            "VRAM {:.0}/{:.0} MiB  GTT {:.0} MiB",
            v.used_mib,
            v.total_mib,
            v.gtt_used_mib.unwrap_or(0.0)
        );
    }
    println!("传感器:");
    for c in &s.chips {
        print!("  {:<24}", c.name);
        for r in &c.readings {
            print!(" {} {}", r.label, r.unit.format(r.value));
        }
        println!();
    }
    Ok(())
}
