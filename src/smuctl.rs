//! Strix Point control requests are parsed and encoded before opening hardware.
use crate::smu::{require_supported_cpu, Smn, CONTROL};
use anyhow::{bail, Context, Result};

pub struct Cmd {
    pub names: &'static [&'static str],
    pub id: u32,
    pub unit: &'static str,
    pub desc: &'static str,
}

/// Strix Point (family 1Ah, PMFW 93.11.0) 已知消息表
pub const CMDS: &[Cmd] = &[
    Cmd {
        names: &["stapm-limit", "stapm"],
        id: 0x14,
        unit: "mW",
        desc: "STAPM 持续功耗墙",
    },
    Cmd {
        names: &["fast-limit", "fast"],
        id: 0x15,
        unit: "mW",
        desc: "PPT 快速功耗墙",
    },
    Cmd {
        names: &["slow-limit", "slow"],
        id: 0x16,
        unit: "mW",
        desc: "PPT 慢速功耗墙",
    },
    Cmd {
        names: &["slow-time"],
        id: 0x17,
        unit: "s",
        desc: "PPT 慢速时间常数",
    },
    Cmd {
        names: &["stapm-time"],
        id: 0x18,
        unit: "s",
        desc: "STAPM 时间常数",
    },
    Cmd {
        names: &["tctl-temp", "tctl"],
        id: 0x19,
        unit: "°C",
        desc: "温度墙",
    },
    Cmd {
        names: &["vrm-current", "tdc"],
        id: 0x1A,
        unit: "mA",
        desc: "TDC VDD 电流限制",
    },
    Cmd {
        names: &["vrmsoc-current"],
        id: 0x1B,
        unit: "mA",
        desc: "TDC SoC 电流限制",
    },
    Cmd {
        names: &["vrmmax-current", "edc"],
        id: 0x1C,
        unit: "mA",
        desc: "EDC VDD 电流限制",
    },
    Cmd {
        names: &["vrmsocmax-current"],
        id: 0x1D,
        unit: "mA",
        desc: "EDC SoC 电流限制",
    },
    Cmd {
        names: &["prochot-ramp"],
        id: 0x1F,
        unit: "?",
        desc: "Prochot 释放斜坡时间",
    },
    Cmd {
        names: &["dgpu-skin-temp"],
        id: 0x34,
        unit: "°C",
        desc: "dGPU 表皮温度限制",
    },
    Cmd {
        names: &["skin-temp-power"],
        id: 0x4A,
        unit: "mW",
        desc: "表皮温度功耗限制",
    },
    Cmd {
        names: &["coall", "co"],
        id: 0x4C,
        unit: "CO",
        desc: "全核 Curve Optimizer (28 位补码, 负值=降压)",
    },
    Cmd {
        names: &["coper"],
        id: 0x4B,
        unit: "CO",
        desc: "单核 Curve Optimizer (参数结构未验证)",
    },
    Cmd {
        names: &["psi0-current"],
        id: 0x1E,
        unit: "mA",
        desc: "PSI0 电流限制 (判别实验: 45000 被接受且不钉频)",
    },
    Cmd {
        names: &["apu-slow-limit"],
        id: 0x23,
        unit: "mW",
        desc: "APU 慢速功耗墙 (判别实验: 45000mW 接受且不钉频, mW 语义实锤)",
    },
    Cmd {
        names: &["power-saving"],
        id: 0x12,
        unit: "?",
        desc: "省电模式 (未验证)",
    },
    Cmd {
        names: &["max-performance"],
        id: 0x11,
        unit: "?",
        desc: "最大性能模式 (未验证)",
    },
];

fn find(name: &str) -> Option<&'static Cmd> {
    CMDS.iter().find(|cmd| {
        cmd.names
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

#[derive(Clone, Copy, Debug)]
enum Value {
    Signed(i64),
    Raw(u32),
}
impl Value {
    fn parse(text: &str) -> Result<Self> {
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            Ok(Self::Raw(
                u32::from_str_radix(hex, 16).with_context(|| format!("无效十六进制: {text}"))?,
            ))
        } else {
            Ok(Self::Signed(
                text.parse()
                    .with_context(|| format!("值不是整数: {text}"))?,
            ))
        }
    }
    fn unsigned(self) -> Result<u32> {
        match self {
            Self::Raw(value) => Ok(value),
            Self::Signed(value) => u32::try_from(value).context("参数必须在 0..=4294967295 范围内"),
        }
    }
    fn curve_all(self) -> Result<u32> {
        match self {
            Self::Raw(value) if value < 1 << 28 => Ok(value),
            Self::Signed(value) if (-(1i64 << 27)..(1i64 << 28)).contains(&value) => {
                Ok((value as u32) & 0x0fff_ffff)
            }
            _ => bail!("全核 CO 需要 28 位编码; 例如 -27 或 0x0FFFFFE5"),
        }
    }
}

enum Request {
    List,
    Write {
        cmd: &'static Cmd,
        raw: u32,
        input: String,
    },
}
impl Request {
    fn parse(args: &[String]) -> Result<Self> {
        let Some(name) = args.first() else {
            return Ok(Self::List);
        };
        if matches!(name.as_str(), "list" | "-h" | "--help") {
            if args.len() > 1 {
                bail!("list 不接受值参数")
            }
            return Ok(Self::List);
        }
        let cmd = find(name)
            .with_context(|| format!("未知消息 {name}; 用 rustinfo smuctl list 查看名称"))?;
        let (raw, input) = if cmd.id == 0x4b {
            if args.len() != 3 {
                bail!("用法: rustinfo smuctl coper <硬件核号 0-3/8-13> <值>")
            }
            let core = args[1].parse::<u32>().context("核心号不是整数")?;
            let value = args[2].parse::<i64>().context("单核 CO 值不是整数")?;
            if !matches!(core,0..=3|8..=13) {
                bail!("核心号 {core} 不在本机已验证的 0-3/8-13 范围内")
            }
            if !(-32768..=32767).contains(&value) {
                bail!("单核 CO 超出 16 位范围")
            }
            (
                ((core & 0xf) << 20) | ((value as u32) & 0xffff),
                format!("核{core} {}", args[2]),
            )
        } else {
            if args.len() != 2 {
                bail!("用法: rustinfo smuctl {name} <值>")
            }
            let value = Value::parse(&args[1])?;
            let raw = if cmd.id == 0x4c {
                value.curve_all()?
            } else {
                value.unsigned()?
            };
            (raw, args[1].clone())
        };
        Ok(Self::Write { cmd, raw, input })
    }
}

pub fn run(args: &[String]) -> Result<()> {
    match Request::parse(args)? {
        Request::List => {
            list();
            Ok(())
        }
        Request::Write { cmd, raw, input } => {
            require_supported_cpu()?;
            let smn = Smn::open(0xb8, 0xbc)?;
            let returned = smn
                .send_command(CONTROL, cmd.id, [raw, 0, 0, 0, 0, 0])?
                .ensure_ok()?;
            println!(
                "{} (0x{:02X}) ← {input} {} — SMU 响应 OK",
                cmd.names[0], cmd.id, cmd.unit
            );
            if cmd.id == 0x4c {
                println!(
                    "返回 arg0 = 0x{:08X} (邮箱返回参数; 实际降压效果需用遥测验证)",
                    returned[0]
                );
            }
            Ok(())
        }
    }
}

fn list() {
    println!("{:<22} {:>6} {:>8}  说明", "名称(别名)", "msg", "单位");
    for cmd in CMDS {
        println!(
            "{:<22} 0x{:04X} {:>8}  {}",
            cmd.names.join("/"),
            cmd.id,
            cmd.unit,
            cmd.desc
        );
    }
    println!("\n用法: rustinfo smuctl <名称> <值>\n示例: rustinfo smuctl co -27 | rustinfo smuctl co 0x0FFFFFE5\n写入仅影响易失状态; SMU 响应 OK 不等于稳定性验证。");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(args: &[&str]) -> Result<Request> {
        Request::parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    fn raw(args: &[&str]) -> u32 {
        match request(args).unwrap() {
            Request::Write { raw, .. } => raw,
            Request::List => panic!("expected write"),
        }
    }
    #[test]
    fn decimal_and_legacy_raw_curve_values_have_identical_28_bit_payloads() {
        assert_eq!(raw(&["co", "-27"]), 0x0fffffe5);
        assert_eq!(raw(&["co", "0x0FFFFFE5"]), 0x0fffffe5);
        assert_eq!(raw(&["co", "268435429"]), 0x0fffffe5);
        assert_eq!(raw(&["COALL", "0"]), 0);
        assert!(request(&["co", "0xFFFFFFE5"]).is_err());
    }
    #[test]
    fn invalid_unsigned_values_and_surplus_args_cannot_reach_hardware() {
        for args in [
            vec!["stapm-limit", "-1"],
            vec!["fast", "4294967296"],
            vec!["co", "-27", "extra"],
            vec!["not-a-command", "1"],
            vec!["list", "1"],
        ] {
            assert!(request(&args).is_err(), "{args:?}");
        }
        assert_eq!(raw(&["fast", "4294967295"]), u32::MAX);
    }
    #[test]
    fn per_core_requests_require_a_valid_hardware_core_and_signed_16_bit_value() {
        assert_eq!(raw(&["CoPer", "8", "-27"]), 0x0080ffe5);
        for args in [
            vec!["coper", "-27"],
            vec!["coper", "4", "-1"],
            vec!["coper", "14", "-1"],
            vec!["coper", "0", "32768"],
            vec!["coper", "0", "-32769"],
        ] {
            assert!(request(&args).is_err());
        }
    }
}
