use anyhow::{bail, Context, Result};
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub enum Command {
    Tui,
    Dump,
    Log(Option<String>),
    SmuProbe,
    SmuTable,
    Smuctl(Vec<String>),
    RenderTest,
    Help,
    Version,
}

#[derive(Debug)]
pub struct Options {
    pub command: Command,
    pub interval: Duration,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut interval = Duration::from_secs(1);
        let mut positional = Vec::new();
        let mut i = 0;
        while i < args.len() {
            if positional.first().is_some_and(|s: &String| s == "smuctl") {
                positional.extend_from_slice(&args[i..]);
                break;
            }
            match args[i].as_str() {
                "-i" | "--interval" => {
                    i += 1;
                    interval = parse_interval(args.get(i).context("-i 需要一个秒数参数")?)?;
                }
                "-h" | "--help" => {
                    return Ok(Self {
                        command: Command::Help,
                        interval,
                    })
                }
                "-V" | "--version" => {
                    return Ok(Self {
                        command: Command::Version,
                        interval,
                    })
                }
                "--" => {
                    positional.extend_from_slice(&args[i + 1..]);
                    break;
                }
                option if option.starts_with('-') => bail!("未知选项 {option}"),
                value => positional.push(value.to_string()),
            }
            i += 1;
        }
        let tail = positional.get(1..).unwrap_or_default();
        let command = match positional.first().map(String::as_str).unwrap_or("tui") {
            "tui" => Command::Tui,
            "dump" => Command::Dump,
            "log" => {
                if tail.len() > 1 {
                    bail!("用法: rustinfo log [文件] [-i 秒数]")
                }
                Command::Log(tail.first().cloned())
            }
            "smu-probe" => Command::SmuProbe,
            "smu-table" => Command::SmuTable,
            "smuctl" => Command::Smuctl(tail.to_vec()),
            "render-test" => Command::RenderTest,
            other => bail!("未知子命令 {other}; 用 rustinfo --help 查看用法"),
        };
        if !matches!(command, Command::Log(_) | Command::Smuctl(_)) && !tail.is_empty() {
            bail!("多余参数: {}", tail.join(" "))
        }
        Ok(Self { command, interval })
    }
}

fn parse_interval(text: &str) -> Result<Duration> {
    let seconds = text
        .trim()
        .strip_suffix('s')
        .unwrap_or(text.trim())
        .trim()
        .parse::<f64>()
        .context("采样间隔必须是秒数")?;
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("采样间隔必须是有限正数")
    }
    Ok(Duration::from_secs_f64(seconds.clamp(0.2, 60.0)))
}

pub fn print_help() {
    println!("rustinfo — Linux 硬件监控\n\n用法: rustinfo [子命令] [-i 秒数]\n  tui           实时终端面板 (默认)\n  dump          打印一次读数\n  log [文件]    CSV 记录, Ctrl-C 结束\n  smu-probe     SMU 邮箱探测\n  smu-table     抓取 SMU 遥测表\n  smuctl        SMU 直控: list | <名称> <值>\n  -i, --interval  采样间隔, 0.2~60 秒\n  -V, --version   版本\n\n按键: q 退出 │ 空格 暂停 │ l CSV │ +/- 间隔 │ PgUp/Dn 滚动");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<Options> {
        Options::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn invalid_arguments_are_errors_not_successful_commands() {
        for args in [
            vec!["missing"],
            vec!["dump", "extra"],
            vec!["--unknown"],
            vec!["-i"],
            vec!["-i", "NaN"],
            vec!["-i", "inf"],
            vec!["-i", "0"],
            vec!["-i", "-1"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn signed_co_arguments_and_contextual_help_reach_the_control_parser() {
        assert_eq!(
            parse(&["smuctl", "co", "-27"]).unwrap().command,
            Command::Smuctl(vec!["co".into(), "-27".into()])
        );
        assert_eq!(
            parse(&["smuctl", "--help"]).unwrap().command,
            Command::Smuctl(vec!["--help".into()])
        );
        assert_eq!(
            parse(&["log", "--", "-data.csv"]).unwrap().command,
            Command::Log(Some("-data.csv".into()))
        );
    }
    #[test]
    fn interval_is_bounded_before_duration_conversion() {
        assert_eq!(
            parse(&["log", "out.csv", "-i", "0.5s"]).unwrap().interval,
            Duration::from_millis(500)
        );
        assert_eq!(
            parse(&["-i", "1e300"]).unwrap().interval,
            Duration::from_secs(60)
        );
    }
}
