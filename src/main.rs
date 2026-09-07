mod logger;
mod model;
mod probe;
mod smuctl;
mod smu;
mod ui;

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;

use crate::probe::Prober;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustinfo: 错误: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let mut interval = 1.0f64;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-i" | "--interval" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    bail!("-i 需要一个秒数参数");
                };
                interval = parse_interval(v)?;
            }
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    interval = interval.clamp(0.2, 60.0);

    let cmd = positional.first().map(String::as_str).unwrap_or("tui");
    match cmd {
        "tui" => run_tui(interval),
        "dump" => run_dump(),
        "log" => run_log(positional.get(1).cloned(), interval),
        "smu-probe" => smu::probe(),
        "smu-table" => smu::smu_table(),
        "smuctl" => smuctl::run(&positional[1..]),
        other => {
            eprintln!("未知子命令: {other}");
            print_help();
            Ok(())
        }
    }
}

fn parse_interval(s: &str) -> Result<f64> {
    let v: f64 = s.trim().trim_end_matches('s').trim().parse()?;
    if v <= 0.0 {
        bail!("采样间隔必须大于 0");
    }
    Ok(v)
}

fn print_help() {
    println!("rustinfo — Linux 硬件监控 (HWiNFO 风格, 纯 sysfs/procfs 采集, Rust 实现)");
    println!();
    println!("用法: rustinfo [子命令] [-i 秒数]");
    println!("  tui           实时终端面板 (默认)");
    println!("  dump          打印一次所有读数后退出");
    println!("  log [文件]    无界面 CSV 记录, Ctrl-C 结束 (默认 rustinfo_时间戳.csv)");
    println!("  smu-probe     SMU 邮箱通道探测");
    println!("  smu-table     抓取 SMU pm_table 遥测");
    println!("  smuctl        SMU 直控 (替代 ryzenadj, 仅 family 0x1A): smuctl list | smuctl <名称> <值>");
    println!();
    println!("按键 (tui): q 退出 │ 空格 暂停 │ l CSV记录 │ +/- 调整采样间隔");
}

fn run_tui(interval: f64) -> Result<()> {
    let mut prober = Prober::new();
    let mut terminal = ratatui::init();
    let res = tui_loop(&mut terminal, &mut prober, interval);
    ratatui::restore();
    res
}

fn tui_loop(terminal: &mut DefaultTerminal, prober: &mut Prober, mut interval: f64) -> Result<()> {
    let mut snapshot = prober.snapshot()?;
    let mut hist = ui::History::new(150);
    hist.push(&snapshot);
    let mut logger: Option<logger::Logger> = None;
    let mut paused = false;
    let mut last_tick = Instant::now();
    let mut dirty = true;

    loop {
        // 距下次采样的剩余时间作为 poll 超时; 暂停时纯等按键, 不再定时刷新
        let timeout = if paused {
            Duration::from_secs(3600)
        } else {
            let remain = interval - last_tick.elapsed().as_secs_f64();
            Duration::from_secs_f64(remain.clamp(0.02, interval))
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char(' ') => paused = !paused,
                        KeyCode::Char('l') => {
                            logger = match logger.take() {
                                Some(mut l) => {
                                    l.finish()?;
                                    None
                                }
                                None => Some(logger::Logger::create(&snapshot, None)?),
                            };
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => interval = (interval * 1.5).min(60.0),
                        KeyCode::Char('-') | KeyCode::Char('_') => interval = (interval / 1.5).max(0.2),
                        _ => {}
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        if !paused && last_tick.elapsed().as_secs_f64() >= interval {
            snapshot = prober.snapshot()?;
            hist.push(&snapshot);
            if let Some(l) = &mut logger {
                l.write(&snapshot)?;
            }
            last_tick = Instant::now();
            dirty = true;
        }

        // 只有数据/按键/窗口尺寸变化才重绘, 避免空转
        if dirty {
            terminal.draw(|f| {
                ui::draw(f, &snapshot, &hist, paused, interval, logger.as_ref().map(|l| l.path()))
            })?;
            dirty = false;
        }
    }

    if let Some(mut l) = logger {
        l.finish()?;
    }
    Ok(())
}

fn run_dump() -> Result<()> {
    let mut prober = Prober::new();
    let _ = prober.snapshot()?; // 预热一次, 占用率/RAPL 功率需要增量
    std::thread::sleep(Duration::from_millis(600));
    let s = prober.snapshot()?;
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
        print!("电池 {} {:.0}%", ui::batt_status_zh(&b.status), b.capacity_pct.unwrap_or(0.0));
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

fn run_log(path: Option<String>, interval: f64) -> Result<()> {
    let mut prober = Prober::new();
    let _ = prober.snapshot()?; // 预热: 占用/功率/速率均为增量量, 首个快照无值
    std::thread::sleep(Duration::from_secs_f64(interval.max(0.5)));
    let s = prober.snapshot()?;
    let mut logger = logger::Logger::create(&s, path.as_deref())?;
    logger.write(&s)?;
    println!("记录到 {}  (Ctrl-C 结束)", logger.path());
    loop {
        let s = prober.snapshot()?;
        logger.write(&s)?;
        std::thread::sleep(Duration::from_secs_f64(interval));
    }
}
