//! SMU 自研通道: SMN 桥 + RSMU/MP1 邮箱 + pm_table 抓取。
//! 协议参考 ryzen_smu / ryzenadj; 只发只读类命令, 全部交互带超时与返回码校验。

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const CFG_PATH: &str = "/sys/bus/pci/devices/0000:00:00.0/config";

/// SMN 桥候选: (index 偏移, data 偏移), 均在 00:00.0 config 空间
const SMN_BRIDGES: &[(u64, u64)] = &[(0xC4, 0xC8), (0xB8, 0xBC)];

/// 邮箱候选: (名称, cmd, rsp, args) — SMN 地址, 来自 ryzen_smu 各代号表
const MAILBOXES: &[(&str, u32, u32, u32)] = &[
    ("RSMU-APU", 0x3B10_A20, 0x3B10_A80, 0x3B10_A88),
    ("MP1-IFv13", 0x3B10_528, 0x3B10_578, 0x3B10_998),
    ("MP1-IFv12", 0x3B10_528, 0x3B10_564, 0x3B10_998),
    ("RSMU-桌面", 0x3B10_524, 0x3B10_570, 0x3B10_A40),
    ("MP1-IFv11", 0x3B10_530, 0x3B10_57C, 0x3B10_9C4),
    ("MP1-IFv9", 0x3B10_528, 0x3B10_564, 0x3B10_598),
    ("RSMU-TR", 0x3B10_51C, 0x3B10_568, 0x3B10_590),
];

pub struct Smn {
    cfg: File,
    idx_reg: u64,
    data_reg: u64,
}

impl Smn {
    pub fn open(idx_reg: u64, data_reg: u64) -> Result<Self> {
        let cfg = OpenOptions::new()
            .read(true)
            .write(true)
            .open(CFG_PATH)
            .with_context(|| format!("打开 {CFG_PATH} 失败 (需 root)"))?;
        Ok(Self {
            cfg,
            idx_reg,
            data_reg,
        })
    }

    pub fn smn_read(&self, addr: u32) -> Result<u32> {
        self.cfg.write_at(&addr.to_le_bytes(), self.idx_reg)?;
        let mut b = [0u8; 4];
        self.cfg.read_exact_at(&mut b, self.data_reg)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn smn_write(&self, addr: u32, val: u32) -> Result<()> {
        self.cfg.write_at(&addr.to_le_bytes(), self.idx_reg)?;
        self.cfg.write_at(&val.to_le_bytes(), self.data_reg)?;
        Ok(())
    }

    /// ryzen_smu 六步握手: 等 RSP 非零 → 清零 → 写参数 → 写命令 → 等 RSP → 读参数
    pub fn send_command(
        &self,
        rsp: u32,
        cmd: u32,
        args_addr: u32,
        op: u32,
        args: [u32; 6],
    ) -> Result<(u32, [u32; 6])> {
        let mut deadline = Instant::now() + Duration::from_millis(500);
        let mut tmp;
        loop {
            tmp = self.smn_read(rsp)?;
            if tmp != 0 || Instant::now() >= deadline {
                break;
            }
        }
        if tmp == 0 {
            bail!("RSP 0x{rsp:X} 初始等待超时");
        }
        self.smn_write(rsp, 0)?;
        for (i, a) in args.iter().enumerate() {
            self.smn_write(args_addr + i as u32 * 4, *a)?;
        }
        self.smn_write(cmd, op)?;
        deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            tmp = self.smn_read(rsp)?;
            if tmp != 0 || Instant::now() >= deadline {
                break;
            }
        }
        if tmp == 0 {
            bail!("命令 0x{op:X} 处理超时");
        }
        let mut ret = [0u32; 6];
        for (i, r) in ret.iter_mut().enumerate() {
            *r = self.smn_read(args_addr + i as u32 * 4)?;
        }
        Ok((tmp, ret))
    }
}

fn version_str(v: u32) -> String {
    format!("{}.{}.{}", (v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff)
}

/// SMU 探针/直控仅限 CPU family 26 (0x1A, Zen 5 移动: Strix/Krackan/Strix Halo) —
/// 邮箱 SMN 地址与消息表按该家族硬编码, 其他平台误写这些地址有硬件风险。
/// 自负风险跳过: RUSTINFO_SMU_FORCE=1
pub fn require_supported_cpu() -> Result<()> {
    if std::env::var_os("RUSTINFO_SMU_FORCE").is_some_and(|v| v == "1") {
        return Ok(());
    }
    let s = fs::read_to_string("/proc/cpuinfo").context("读 /proc/cpuinfo")?;
    let fam = s.lines().find_map(|l| {
        l.strip_prefix("cpu family")?
            .split(':')
            .nth(1)?
            .trim()
            .parse::<u32>()
            .ok()
    });
    match fam {
        Some(26) => Ok(()),
        Some(f) => bail!(
            "CPU family {f} (0x{f:X}) 未验证 — smu/smuctl 的邮箱与消息表仅适配 family 26 (0x1A, Zen 5 移动)。确要尝试: RUSTINFO_SMU_FORCE=1"
        ),
        None => bail!("无法识别 CPU family。确要尝试: RUSTINFO_SMU_FORCE=1"),
    }
}

/// pm_table 抓取: 版本 → DRAM 基址 → 触发传输 → /dev/mem 只读读取。
/// 只用 APU 类(Renoir/Cezanne 系)已文档化的三个只读命令号,
/// 任何一个返回 UnknownCmd 就停下, 不盲试其他命令号。
pub fn smu_table() -> Result<()> {
    require_supported_cpu()?;
    let smn = Smn::open(0xC4, 0xC8)?;
    const CMD: u32 = 0x3B10_A20; // RSMU-APU (探针已验证)
    const RSP: u32 = 0x3B10_A80;
    const ARGS: u32 = 0x3B10_A88;

    println!("== 1/4 pm_table 版本 (cmd 0x06) ==");
    let (ret, a) = smn.send_command(RSP, CMD, ARGS, 0x06, [0; 6])?;
    if ret != 0x01 {
        bail!("返回 0x{ret:02X} (0xFE=UnknownCmd → 这代命令号可能平移, 需查 ryzenadj 源码)");
    }
    println!("  pm_table 版本 0x{:06X}", a[0]);

    println!("== 2/4 DRAM 基址 (cmd 0x66) ==");
    let (ret, a) = smn.send_command(RSP, CMD, ARGS, 0x66, [1, 1, 0, 0, 0, 0])?;
    if ret != 0x01 {
        bail!("返回 0x{ret:02X}");
    }
    let base = a[0] as u64 | (a[1] as u64) << 32;
    if base == 0 || base > 0x0000_8000_0000_0000 {
        bail!("基址异常: 0x{base:X}");
    }
    println!("  SMU 保留区基址 0x{base:012X}");

    println!("== 3/4 /proc/iomem 安全校验 ==");
    let (label, avail) = iomem_region(base)?;
    println!("  区域「{label}」从基址起可读 {avail:#X} 字节");
    if label.to_lowercase().contains("system ram") {
        bail!("目标落在 System RAM — 拒绝读取 (安全护栏)");
    }
    let len = avail.min(0x1000);
    if len < 0x100 {
        bail!("可读区域过小 ({len:#X})");
    }

    println!("== 4/4 表传输 (cmd 0x65) + 只读读取 ==");
    let out = "/tmp/smu_pm_table.bin";
    let buf = if let Ok(b) = fs::read("/sys/kernel/debug/rustinfo_smu/table") {
        // 自研模块: read 内部已触发全新传输 (table_refresh), 不要再从用户态发 0x65 —
        // 双发会与模块的六步握手竞争同一 SMN 桥/邮箱, 偶发超时 (实测 ~1/3 失败率)
        println!("  经 rustinfo_smu 模块读取 {:#X} 字节 → {out}", b.len());
        b
    } else {
        let (ret, _) = smn.send_command(RSP, CMD, ARGS, 0x65, [0; 6])?;
        if ret != 0x01 {
            bail!("传输命令返回 0x{ret:02X}");
        }
        let devmem = File::open("/dev/mem").context("打开 /dev/mem 失败")?;
        let mut b = vec![0u8; len as usize];
        if let Err(e) = devmem.read_exact_at(&mut b, base) {
            bail!("/dev/mem 读取失败: {e} (IO_STRICT_DEVMEM 内核请先加载 module/rustinfo_smu.ko)");
        }
        println!("  已读取 {:#X} 字节 → {out}", b.len());
        b
    };
    fs::write(out, &buf).with_context(|| format!("写 {out}"))?;

    let f32v = |off: usize| f32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
    println!("\n已知偏移候选:");
    println!("  0x7C VDDCR ≈ {:.1}", f32v(0x7C));
    println!("\n前 0x80 字节 hexdump:");
    for row in 0..8 {
        let o = row * 16;
        let hex: Vec<String> = buf[o..o + 16].iter().map(|b| format!("{b:02X}")).collect();
        println!("  {o:04X}: {}  {}", hex.join(" "), hex_f32(&buf, o));
    }
    // 全表转储: "0xOFFSET value" 每行一个 f32, 供脚本 awk '$1 ~ /^0x/' 解析
    // (替代 ryzenadj --dump-table; 0x007C=VDDCR, 0x0000/0x0008/0x0010=stapm/fast/slow limit)
    println!("\n全表 f32 转储 ({} 字节):", buf.len());
    for off in (0..buf.len() - 3).step_by(4) {
        println!("0x{off:04X} {}", f32v(off));
    }
    println!("\n用 ryzenadj --dump-table 对照同偏移即可验证; 非零 float 密集区即遥测区。");
    Ok(())
}

fn hex_f32(buf: &[u8], off: usize) -> String {
    let mut parts = Vec::new();
    for i in 0..4 {
        let o = off + i * 4;
        let v = f32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        if v.abs() > 0.001 && v.is_finite() {
            parts.push(format!("{o:04X}:{v:.2}"));
        }
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join(" ")
    }
}

/// 在 /proc/iomem 中定位物理地址所在区域, 返回 (标签, 从该地址到区域结束的字节数)
fn iomem_region(base: u64) -> Result<(String, u64)> {
    let s = fs::read_to_string("/proc/iomem").context("读 /proc/iomem (需 root)")?;
    for line in s.lines() {
        let Some((range, label)) = line.split_once(" : ") else { continue };
        let Some((start, end)) = range.trim().split_once('-') else { continue };
        let Ok(start) = u64::from_str_radix(start, 16) else { continue };
        let Ok(end) = u64::from_str_radix(end, 16) else { continue };
        if start <= base && base <= end {
            return Ok((label.trim().to_string(), end - base + 1));
        }
    }
    bail!("基址 0x{base:X} 不在任何 iomem 区域内")
}

pub fn probe() -> Result<()> {
    require_supported_cpu()?;
    println!("SMU 邮箱探针 — 只发只读命令 0x02 (GetSMUVersion), 命中即停");
    for (idx_reg, data_reg) in SMN_BRIDGES {
        let smn = match Smn::open(*idx_reg, *data_reg) {
            Ok(s) => s,
            Err(e) => {
                println!("SMN 桥 0x{idx_reg:X}/0x{data_reg:X}: {e:#}");
                continue;
            }
        };
        println!("SMN 桥 config 0x{idx_reg:X}/0x{data_reg:X}:");
        for (name, cmd, rsp, args) in MAILBOXES {
            match smn.send_command(*rsp, *cmd, *args, 0x02, [1, 0, 0, 0, 0, 0]) {
                Ok((ret, a)) => {
                    println!("  {name:<9} RSP 0x{ret:02X}  arg0 0x{:08X}", a[0]);
                    if ret == 0x01 && a[0] > 1 {
                        let v = a[0];
                        println!("  ✅ 命中: {name} — SMU 版本 {} (0x{v:08X})", version_str(v));
                        return Ok(());
                    }
                }
                Err(e) => println!("  {name:<9} {e:#}"),
            }
        }
    }
    bail!("所有候选邮箱均未命中 — 固件可能用了新邮箱布局, 需要进一步逆向");
}
