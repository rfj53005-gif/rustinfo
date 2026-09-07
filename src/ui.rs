use std::collections::VecDeque;

use ratatui::prelude::*;
use ratatui::symbols;
use ratatui::widgets::{Block, Paragraph, Sparkline};

use crate::model::{Snapshot, Unit};

// ---------- 通用小件 ----------

/// 1/8 精度进度条, 空位用空格 (比 ░ 干净)
const BAR8: [char; 9] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

fn bar8(pct: f64, width: usize) -> String {
    let units = (pct / 100.0 * width as f64 * 8.0).round() as usize;
    let full = (units / 8).min(width);
    let frac = units % 8;
    let mut s: String = "█".repeat(full);
    if full < width && frac > 0 {
        s.push(BAR8[frac]);
    }
    s
}

fn usage_color(pct: f64) -> Color {
    if pct < 40.0 {
        Color::Green
    } else if pct < 70.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn temp_color(v: f64) -> Color {
    if v < 60.0 {
        Color::Green
    } else if v < 75.0 {
        Color::Yellow
    } else if v < 85.0 {
        Color::LightRed
    } else {
        Color::Red
    }
}

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(Color::DarkGray))
}

fn panel(title: String) -> Block<'static> {
    Block::bordered()
        .border_set(symbols::border::ROUNDED)
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Cyan),
        ))
}

fn reading_color(r: &Unit, v: f64) -> Color {
    match r {
        Unit::TempC => temp_color(v),
        Unit::Watts if v >= 25.0 => Color::Yellow,
        Unit::Dbm => {
            if v > -60.0 {
                Color::Green
            } else if v > -75.0 {
                Color::Yellow
            } else {
                Color::Red
            }
        }
        _ => Color::Reset,
    }
}

// ---------- 历史 ----------

pub struct History {
    pub tctl: VecDeque<u64>,
    pub power: VecDeque<u64>,
    pub mem: VecDeque<u64>,
    cap: usize,
}

impl History {
    pub fn new(cap: usize) -> Self {
        Self {
            tctl: VecDeque::with_capacity(cap),
            power: VecDeque::with_capacity(cap),
            mem: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, s: &Snapshot) {
        if let Some(t) = find_tctl(s) {
            push_ring_cap(&mut self.tctl, (t * 10.0) as u64, self.cap);
        }
        let w = s
            .cpu
            .pkg_w
            .or_else(|| s.battery.as_ref().and_then(|b| b.watts).map(f64::abs));
        if let Some(w) = w {
            push_ring_cap(&mut self.power, (w * 10.0) as u64, self.cap);
        }
        let used = s.mem.total_kib.saturating_sub(s.mem.avail_kib);
        let mem_pct = if s.mem.total_kib > 0 {
            used as f64 / s.mem.total_kib as f64 * 1000.0
        } else {
            0.0
        };
        push_ring_cap(&mut self.mem, mem_pct as u64, self.cap);
    }
}

fn push_ring_cap(v: &mut VecDeque<u64>, val: u64, cap: usize) {
    if v.len() == cap {
        v.pop_front();
    }
    v.push_back(val);
}

fn find_tctl(s: &Snapshot) -> Option<f64> {
    let k = s
        .chips
        .iter()
        .find(|c| c.name.contains("k10temp") || c.name.contains("zenpower"))
        .or_else(|| s.chips.first())?;
    k.readings
        .iter()
        .find(|r| r.unit == Unit::TempC)
        .map(|r| r.value)
}

// ---------- 顶层绘制 ----------

pub fn draw(f: &mut Frame, s: &Snapshot, hist: &History, paused: bool, interval: f64, log_path: Option<&str>) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(13),
        Constraint::Min(4),
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .split(f.area());
    draw_header(f, rows[0], s);
    draw_top(f, rows[1], s);
    draw_sensors(f, rows[2], s);
    draw_charts(f, rows[3], hist);
    draw_footer(f, rows[4], paused, interval, log_path);
}

fn draw_header(f: &mut Frame, area: Rect, s: &Snapshot) {
    let t = s.time.format("%H:%M:%S");
    let line = Line::from(vec![
        Span::styled(
            " rustinfo ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        dim("主机 "),
        Span::raw(s.hostname.clone()),
        dim(" │ 内核 "),
        Span::raw(s.kernel.clone()),
        dim(" │ 运行 "),
        Span::raw(fmt_uptime(s.uptime_s)),
        dim(" │ 负载 "),
        Span::raw(format!("{:.2} {:.2} {:.2}", s.load[0], s.load[1], s.load[2])),
        dim(" │ "),
        Span::raw(format!("{t}")),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_top(f: &mut Frame, area: Rect, s: &Snapshot) {
    let cols = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).split(area);

    let cpu_block = panel(format!("CPU · {}", truncate(&s.cpu.model, 48)));
    let cpu_inner = cpu_block.inner(cols[0]);
    f.render_widget(cpu_block, cols[0]);
    let tc = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(cpu_inner);
    let half = s.cpu.per_mhz.len().div_ceil(2);
    for (i, col) in tc.iter().enumerate() {
        let mut lines = Vec::new();
        for t in (i * half)..((i + 1) * half).min(s.cpu.per_mhz.len()) {
            let pct = s.cpu.per_pct.get(t).copied().unwrap_or(0.0);
            let mhz = s.cpu.per_mhz[t];
            let color = usage_color(pct as f64);
            lines.push(Line::from(vec![
                Span::styled(format!("T{t:02} "), Style::default().fg(Color::DarkGray)),
                Span::styled(bar8(pct as f64, 8), Style::default().fg(color)),
                Span::styled(format!(" {:>5.1}%", pct), Style::default().fg(color)),
                Span::styled(format!(" {:>4.0}MHz", mhz), Style::default().fg(Color::DarkGray)),
            ]));
        }
        f.render_widget(Paragraph::new(lines), *col);
    }

    let sys_block = panel("系统".into());
    let sys_inner = sys_block.inner(cols[1]);
    f.render_widget(sys_block, cols[1]);
    let mut lines: Vec<Line> = Vec::new();

    let total_color = usage_color(s.cpu.total_pct as f64);
    lines.push(Line::from(vec![
        dim("总占用 "),
        Span::styled(
            format!("{:>5.1}%", s.cpu.total_pct),
            Style::default().fg(total_color),
        ),
        dim(format!("  均 {:.0}MHz", avg_mhz(s))),
    ]));
    if let Some(w) = s.cpu.pkg_w {
        lines.push(Line::from(vec![
            dim("CPU功耗 "),
            Span::styled(format!("{w:.2} W"), Style::default().fg(reading_color(&Unit::Watts, w))),
        ]));
    }
    let used = s.mem.total_kib.saturating_sub(s.mem.avail_kib);
    let mem_pct = if s.mem.total_kib > 0 {
        used as f64 / s.mem.total_kib as f64 * 100.0
    } else {
        0.0
    };
    lines.push(Line::from(vec![
        dim("内存 "),
        Span::styled(
            bar8(mem_pct, 12),
            Style::default().fg(usage_color(mem_pct)),
        ),
        Span::raw(format!(" {:>4.0}% ", mem_pct)),
        dim(format!("{:.1}/{:.1}G", used as f64 / 1048576.0, s.mem.total_kib as f64 / 1048576.0)),
    ]));
    let swap_used = s.mem.swap_total_kib.saturating_sub(s.mem.swap_free_kib);
    lines.push(Line::from(vec![
        dim("Swap "),
        Span::styled(
            bar8(
                if s.mem.swap_total_kib > 0 {
                    swap_used as f64 / s.mem.swap_total_kib as f64 * 100.0
                } else {
                    0.0
                },
                12,
            ),
            Style::default().fg(Color::DarkGray),
        ),
        dim(format!(
            " {:.1}/{:.1}G",
            swap_used as f64 / 1048576.0,
            s.mem.swap_total_kib as f64 / 1048576.0
        )),
    ]));
    if let Some(v) = &s.vram {
        let mut l = vec![
            dim("VRAM "),
            Span::raw(format!("{:.0}/{:.0}M", v.used_mib, v.total_mib)),
        ];
        if let Some(g) = v.gtt_used_mib {
            l.push(dim(format!("  GTT {g:.0}M")));
        }
        lines.push(Line::from(l));
    }
    if let Some(b) = &s.battery {
        let cap = b.capacity_pct.unwrap_or(0.0);
        let batt_color = match b.status.as_str() {
            "Charging" | "Full" => Color::Green,
            "Discharging" if cap < 20.0 => Color::Red,
            "Discharging" => Color::Yellow,
            _ => Color::DarkGray,
        };
        lines.push(Line::from(vec![
            dim("电池 "),
            Span::styled(bar8(cap, 10), Style::default().fg(batt_color)),
            Span::styled(format!(" {:>2.0}%", cap), Style::default().fg(batt_color)),
            Span::styled(format!(" {}", batt_status_zh(&b.status)), Style::default().fg(batt_color)),
        ]));
        let mut l = vec![dim("      ")];
        if let Some(v) = b.volts {
            l.push(Span::raw(format!("{v:.2}V ")));
        }
        if let Some(a) = b.amps {
            l.push(Span::raw(format!("{a:.2}A ")));
        }
        if let Some(w) = b.watts {
            l.push(Span::styled(format!("{:.1}W ", w.abs()), Style::default().fg(reading_color(&Unit::Watts, w.abs()))));
        }
        if let (Some(en), Some(ef)) = (b.energy_now_wh, b.energy_full_wh) {
            l.push(dim(format!("{:.1}/{:.1}Wh", en, ef)));
        }
        lines.push(Line::from(l));
        let mut l = vec![dim("      ")];
        if let Some(h) = b.health_pct {
            l.push(dim("健康 "));
            l.push(Span::styled(format!("{:.0}%", h), Style::default().fg(if h < 80.0 { Color::Yellow } else { Color::Green })));
        }
        if let Some(m) = b.runtime_min {
            l.push(dim(format!("  续航 ~{}h{:02}m", m / 60, m % 60)));
        }
        if l.len() > 1 {
            lines.push(Line::from(l));
        }
    }
    match s.ac_online {
        Some(true) => lines.push(Line::from(vec![dim("外接电源 "), Span::styled("在线", Style::default().fg(Color::Green))])),
        Some(false) => lines.push(Line::from(vec![dim("外接电源 "), Span::styled("离线", Style::default().fg(Color::DarkGray))])),
        None => {}
    }
    if !s.cpu.governor.is_empty() {
        let mut l = vec![dim("调速 "), Span::raw(s.cpu.governor.clone())];
        if !s.cpu.epp.is_empty() {
            l.push(dim(format!(" · {}", s.cpu.epp)));
        }
        lines.push(Line::from(l));
    }
    f.render_widget(Paragraph::new(lines), sys_inner);
}

fn avg_mhz(s: &Snapshot) -> f32 {
    if s.cpu.per_mhz.is_empty() {
        0.0
    } else {
        s.cpu.per_mhz.iter().sum::<f32>() / s.cpu.per_mhz.len() as f32
    }
}

fn draw_sensors(f: &mut Frame, area: Rect, s: &Snapshot) {
    let block = panel(format!("传感器 · {} 芯片", s.chips.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let ncols = if s.chips.len() > 12 { 4 } else { 3 };

    // 每芯片先渲染成行 (smu 类芯片的每核 clk/W/VID 三读数压缩为一行),
    // 再按高度降序贪心装柱 (first-fit decreasing) — 轮转分配会让大芯片
    // (smu 40+ 读数) 整片被裁掉, 顺序分配则柱子高矮悬殊
    let mut views: Vec<Vec<Line>> = s.chips.iter().map(chip_lines).collect();
    views.sort_by_key(|v| usize::MAX - v.len()); // 稳定降序
    let mut cols: Vec<Vec<Line>> = (0..ncols).map(|_| Vec::new()).collect();
    let mut heights = vec![0usize; ncols];
    for lines in views {
        let c = heights
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| *h)
            .map(|(i, _)| i)
            .unwrap_or(0);
        heights[c] += lines.len();
        cols[c].extend(lines);
    }

    let constraints: Vec<Constraint> = (0..ncols)
        .map(|_| Constraint::Ratio(1, ncols as u32))
        .collect();
    let rects = Layout::horizontal(constraints).split(inner);
    for (i, lines) in cols.into_iter().enumerate() {
        f.render_widget(Paragraph::new(lines), rects[i]);
    }
}

/// 识别 C<数字>_<kind> 形式的每核读数 (smu / smu-cores 芯片)
fn core_triplet(label: &str) -> Option<(&str, &str)> {
    let (c, kind) = label.split_once('_')?;
    if c.len() >= 2 && c.starts_with('C') && c[1..].chars().all(|x| x.is_ascii_digit()) {
        Some((c, kind))
    } else {
        None
    }
}

fn chip_lines(chip: &crate::model::Chip) -> Vec<Line<'static>> {
    use std::collections::BTreeMap;
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("▸ {}", chip.name),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))];
    // 每核读数按核聚合: C00_clk/C00_w/C00_vid → 一行 "C00 1505MHz 0.99W 0.95V"
    let mut cores: BTreeMap<String, Vec<Span<'static>>> = BTreeMap::new();
    for r in &chip.readings {
        // 零值 (时钟门控/风扇停转) 压暗, 让活跃读数更醒目
        let color = if r.value == 0.0 {
            Color::DarkGray
        } else {
            reading_color(&r.unit, r.value)
        };
        let text = format!("{}{}", r.unit.fmt_value(r.value), r.unit.suffix());
        if let Some((c, _)) = core_triplet(&r.label) {
            cores.entry(c.to_string())
                .or_default()
                .push(Span::styled(format!(" {text:>9}"), Style::default().fg(color)));
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<13}", r.label),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(format!("{text:>10} "), Style::default().fg(color)),
        ]));
    }
    for (c, spans) in cores {
        let mut l = vec![Span::styled(
            format!(" {c:<6}"),
            Style::default().add_modifier(Modifier::DIM),
        )];
        l.extend(spans);
        lines.push(Line::from(l));
    }
    lines.push(Line::from(""));
    lines
}

fn draw_charts(f: &mut Frame, area: Rect, hist: &History) {
    let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(area);
    chart(f, cols[0], &hist.tctl, Color::Red, "CPU 温度", 10.0, "°C");
    chart(f, cols[1], &hist.power, Color::Cyan, "功率", 10.0, "W");
    chart(f, cols[2], &hist.mem, Color::Green, "内存", 10.0, "%");
}

fn chart(f: &mut Frame, area: Rect, data: &VecDeque<u64>, color: Color, name: &str, scale: f64, unit: &str) {
    let v: Vec<u64> = data.iter().copied().collect();
    let title = match (v.last(), v.iter().max()) {
        (Some(last), Some(max)) => format!(
            " {name}  {:.1}{unit}  峰值 {:.1}{unit} ",
            *last as f64 / scale,
            *max as f64 / scale
        ),
        _ => format!(" {name} "),
    };
    f.render_widget(
        Sparkline::default()
            .data(&v)
            .style(Style::default().fg(color))
            .block(panel(title)),
        area,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, paused: bool, interval: f64, log_path: Option<&str>) {
    fn key(t: &str) -> Span<'static> {
        Span::styled(
            format!(" {t} "),
            Style::default().fg(Color::Black).bg(Color::DarkGray),
        )
    }
    let mut spans = vec![
        key("q 退出"),
        Span::raw(" "),
        key("空格 暂停"),
        Span::raw(" "),
        key("l CSV"),
        Span::raw(" "),
        key("+/- 间隔"),
        Span::raw("  "),
        dim(format!("采样 {:.1}s ", interval)),
    ];
    if paused {
        spans.push(Span::styled(
            " ‖ 已暂停 ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    if let Some(p) = log_path {
        spans.push(Span::styled(
            format!(" ● {p} "),
            Style::default().fg(Color::Black).bg(Color::Green),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

pub fn fmt_uptime(secs: f64) -> String {
    let s = secs as u64;
    let d = s / 86400;
    let h = (s % 86400) / 3600;
    let m = (s % 3600) / 60;
    if d > 0 {
        format!("{d}d{h}h{m}m")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

pub fn batt_status_zh(s: &str) -> &'static str {
    match s {
        "Discharging" => "放电",
        "Charging" => "充电",
        "Full" => "已满",
        "Not charging" => "未充电",
        _ => "未知",
    }
}
