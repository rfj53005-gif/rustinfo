//! SMU 直控 — Strix Point MP1 消息手册的 Rust 实现, 目标是替代 ryzenadj。
//! 邮箱三件套 (msg/rep/arg) 与全部消息 ID 来自 ryzenadj api.c 的社区逆向,
//! 其中 0x1E/0x23 (FCLK 软上限/硬下限) 为本机实测验证的新发现。
//! 所有写入只影响易失状态, 重启即回 BIOS 默认 — 持久化请走 systemd/定时器并遵守浸泡验证铁律。

use anyhow::{bail, Context, Result};

use crate::smu::Smn;

/// RSMU MP1 邮箱 (ryzenadj STRIXPOINT 三件套, CO/FCLK 均实测验证)
const MSG: u32 = 0x3B10_928;
const REP: u32 = 0x3B10_978;
const ARG: u32 = 0x3B10_998;
/// SMN 桥: 00:00.0 config 空间的 index/data 偏移
const BRIDGE: (u64, u64) = (0xB8, 0xBC);

pub struct Cmd {
    pub names: &'static [&'static str],
    pub id: u32,
    pub unit: &'static str,
    pub desc: &'static str,
}

/// Strix Point (family 1Ah, PMFW 93.11.0) 已知消息表
pub const CMDS: &[Cmd] = &[
    Cmd { names: &["stapm-limit", "stapm"], id: 0x14, unit: "mW", desc: "STAPM 持续功耗墙" },
    Cmd { names: &["fast-limit", "fast"], id: 0x15, unit: "mW", desc: "PPT 快速功耗墙" },
    Cmd { names: &["slow-limit", "slow"], id: 0x16, unit: "mW", desc: "PPT 慢速功耗墙" },
    Cmd { names: &["slow-time"], id: 0x17, unit: "s", desc: "PPT 慢速时间常数" },
    Cmd { names: &["stapm-time"], id: 0x18, unit: "s", desc: "STAPM 时间常数" },
    Cmd { names: &["tctl-temp", "tctl"], id: 0x19, unit: "°C", desc: "温度墙" },
    Cmd { names: &["vrm-current", "tdc"], id: 0x1A, unit: "mA", desc: "TDC VDD 电流限制" },
    Cmd { names: &["vrmsoc-current"], id: 0x1B, unit: "mA", desc: "TDC SoC 电流限制" },
    Cmd { names: &["vrmmax-current", "edc"], id: 0x1C, unit: "mA", desc: "EDC VDD 电流限制" },
    Cmd { names: &["vrmsocmax-current"], id: 0x1D, unit: "mA", desc: "EDC SoC 电流限制" },
    Cmd { names: &["prochot-ramp"], id: 0x1F, unit: "?", desc: "Prochot 释放斜坡时间" },
    Cmd { names: &["dgpu-skin-temp"], id: 0x34, unit: "°C", desc: "dGPU 表皮温度限制" },
    Cmd { names: &["skin-temp-power"], id: 0x4A, unit: "mW", desc: "表皮温度功耗限制" },
    Cmd { names: &["coall", "co"], id: 0x4C, unit: "CO", desc: "全核 Curve Optimizer (28 位补码, 负值=降压)" },
    Cmd { names: &["coper"], id: 0x4B, unit: "CO", desc: "单核 Curve Optimizer (参数结构未验证)" },
    Cmd { names: &["psi0-current"], id: 0x1E, unit: "mA", desc: "PSI0 电流限制 (判别实验: 45000 被接受且不钉频)" },
    Cmd { names: &["apu-slow-limit"], id: 0x23, unit: "mW", desc: "APU 慢速功耗墙 (判别实验: 45000mW 接受且不钉频, mW 语义实锤)" },
    Cmd { names: &["power-saving"], id: 0x12, unit: "?", desc: "省电模式 (未验证)" },
    Cmd { names: &["max-performance"], id: 0x11, unit: "?", desc: "最大性能模式 (未验证)" },
];

fn find(name: &str) -> Option<&'static Cmd> {
    CMDS
        .iter()
        .find(|c| c.names.iter().any(|n| n.eq_ignore_ascii_case(name)))
}

/// CLI 入口: rustinfo smuctl [list | <名称> <值>]
pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("list") | Some("-h") | Some("--help") => {
            list();
            Ok(())
        }
        Some(name) => {
            let Some(v) = args.get(1) else {
                bail!("缺少值: rustinfo smuctl {name} <值>   (list 查看全部消息)");
            };
            let val: i64 = v
                .parse()
                .with_context(|| format!("值 \"{v}\" 不是整数 (CO 负值直接写 -27)"))?;
            set(name, val)
        }
    }
}

pub fn list() {
    println!(
        "{:<22} {:>6} {:>8}  {}",
        "名称(别名)", "msg", "单位", "说明"
    );
    for c in CMDS {
        println!(
            "{:<22} 0x{:04X} {:>8}  {}",
            c.names.join("/"),
            c.id,
            c.unit,
            c.desc
        );
    }
    println!();
    println!("用法: rustinfo smuctl <名称> <值>");
    println!("示例: rustinfo smuctl co -27   |   rustinfo smuctl fclk-max 800");
    println!("所有写入均为易失状态, 重启回 BIOS 默认; 响应码实时校验 (OK/Failed/UnknownCmd/Rejected)。");
}

pub fn set(name: &str, value: i64) -> Result<()> {
    let Some(c) = find(name) else {
        bail!(
            "未知消息 \"{name}\" — 用 `rustinfo smuctl list` 查看全部 ({})",
            CMDS.len()
        );
    };
    let raw: u32 = if c.id == 0x4C || c.id == 0x4B {
        // CO 走 28 位补码: -27 → 0x0FFFFFE5 (32 位补码 0xFFFFFFE5 会被 SMU 拒绝)
        if !(-(1i64 << 27)..(1i64 << 27)).contains(&value) {
            bail!("CO 值 {value} 超出 28 位补码范围 (±134217728)");
        }
        if value < 0 {
            (value + (1i64 << 28)) as u32
        } else {
            value as u32
        }
    } else {
        value as u32 // 其余消息 (mW/mA/MHz/s) 为普通正数
    };
    let smn = Smn::open(BRIDGE.0, BRIDGE.1)?;
    let (resp, args) = smn.send_command(REP, MSG, ARG, c.id, [raw, 0, 0, 0, 0, 0])?;
    match resp {
        1 => {
            println!("✅ {name} (0x{:02X}) ← {value} {} — SMU 响应 OK", c.id, c.unit);
            if c.id == 0x4C {
                let back = args[0] as i32;
                println!("   SMU 读回 arg0 = {back} (CO 步进值)");
            }
            Ok(())
        }
        0xFF => bail!("❌ SMU 拒绝执行 (Failed) — 值可能超出固件允许范围"),
        0xFE => bail!("❌ UnknownCmd — 此 PMFW 不支持该消息"),
        0xFD => bail!("❌ RejectedPrereq — 前置条件不满足"),
        0xFC => bail!("❌ Busy — SMU 忙, 稍后重试"),
        r => bail!("未知响应 0x{r:X}"),
    }
}
