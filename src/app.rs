use crate::{logger, probe::Prober, ui};
use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use std::time::{Duration, Instant};

pub fn run_tui(interval: f64) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("TUI 需要交互式终端; 非交互模式请用 dump 或 log");
    }
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(error) => {
            ratatui::restore();
            return Err(error.into());
        }
    };
    let mut prober = Prober::new();
    let res = tui_loop(&mut terminal, &mut prober, interval);
    ratatui::restore();
    res
}

fn tui_loop(terminal: &mut DefaultTerminal, prober: &mut Prober, mut interval: f64) -> Result<()> {
    let mut snapshot = prober.snapshot();
    let mut hist = ui::History::new(150);
    hist.push(&snapshot);
    let mut logger: Option<logger::Logger> = None;
    let mut paused = false;
    let mut last_tick = Instant::now();
    let mut dirty = true;
    let mut sensor_scroll = 0u16;

    loop {
        // 只有数据/按键/窗口尺寸变化才重绘, 避免空转
        if dirty {
            terminal.draw(|f| {
                ui::draw(
                    f,
                    &snapshot,
                    &hist,
                    paused,
                    interval,
                    logger.as_ref().map(|l| l.path()),
                    &mut sensor_scroll,
                )
            })?;
            dirty = false;
        }
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
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char(' ') => paused = !paused,
                        KeyCode::Char('l') => {
                            logger = match logger.take() {
                                Some(mut l) => {
                                    l.finish()?;
                                    None
                                }
                                None => {
                                    std::thread::sleep(
                                        Duration::from_millis(50)
                                            .saturating_sub(last_tick.elapsed()),
                                    );
                                    snapshot = prober.snapshot();
                                    last_tick = Instant::now();
                                    Some(logger::Logger::create(&snapshot, None)?)
                                }
                            };
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            interval = (interval * 1.5).min(60.0)
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            interval = (interval / 1.5).max(0.2)
                        }
                        KeyCode::PageDown => sensor_scroll = sensor_scroll.saturating_add(5),
                        KeyCode::PageUp => sensor_scroll = sensor_scroll.saturating_sub(5),
                        KeyCode::Home => sensor_scroll = 0,
                        KeyCode::End => sensor_scroll = u16::MAX,
                        _ => {}
                    }
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }

        if !paused && last_tick.elapsed().as_secs_f64() >= interval {
            snapshot = prober.snapshot();
            hist.push(&snapshot);
            if let Some(l) = &mut logger {
                l.write(&snapshot)?;
            }
            last_tick = Instant::now();
            dirty = true;
        }
    }

    if let Some(mut l) = logger {
        l.finish()?;
    }
    Ok(())
}

pub fn run_log(path: Option<String>, interval: f64) -> Result<()> {
    let shutdown = crate::shutdown::Shutdown::install()?;
    let mut prober = Prober::new();
    let _ = prober.snapshot(); // 预热: 占用/功率/速率均为增量量, 首个快照无值
    if shutdown.wait_until(Instant::now() + Duration::from_secs_f64(interval.max(0.5))) {
        return Ok(());
    }
    let s = prober.snapshot();
    if shutdown.requested() {
        return Ok(());
    }
    let mut logger = logger::Logger::create(&s, path.as_deref())?;
    logger.write(&s)?;
    println!("记录到 {}  (Ctrl-C 结束)", logger.path());
    let period = Duration::from_secs_f64(interval);
    let mut deadline = Instant::now() + period;
    loop {
        if shutdown.wait_until(deadline) {
            break;
        }
        let s = prober.snapshot();
        if shutdown.requested() {
            break;
        }
        logger.write(&s)?;
        deadline += period;
        if deadline <= Instant::now() {
            deadline = Instant::now() + period;
        }
    }
    logger.finish()
}

/// 开发用: 渲染一帧 TUI 到 TestBackend 并打印纯文本 (无颜色), 用于布局检查
pub fn render_test() -> Result<()> {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut prober = Prober::new();
    let _ = prober.snapshot();
    std::thread::sleep(Duration::from_millis(600));
    let s = prober.snapshot();
    let mut hist = ui::History::new(150);
    for _ in 0..40 {
        hist.push(&s);
    }
    let mut terminal = Terminal::new(TestBackend::new(160, 50))?;
    let mut scroll = 0;
    terminal.draw(|f| ui::draw(f, &s, &hist, false, 1.0, None, &mut scroll))?;
    println!("{}", terminal.backend());
    Ok(())
}
