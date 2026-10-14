//! Serialized userspace SMN transport and the read-only Strix Point mailbox tools.
use crate::platform::CpuIdentity;
use anyhow::{bail, Context, Result};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::time::{Duration, Instant};

const CFG_PATH: &str = "/sys/bus/pci/devices/0000:00:00.0/config";
const SMN_BRIDGES: &[(u64, u64)] = &[(0xC4, 0xC8), (0xB8, 0xBC)];

#[derive(Clone, Copy)]
pub struct Mailbox {
    pub name: &'static str,
    pub msg: u32,
    pub rsp: u32,
    pub arg: u32,
}
pub const CONTROL: Mailbox = Mailbox {
    name: "Strix MP1",
    msg: 0x3B10_928,
    rsp: 0x3B10_978,
    arg: 0x3B10_998,
};
const TABLE: Mailbox = Mailbox {
    name: "RSMU-APU",
    msg: 0x3B10_A20,
    rsp: 0x3B10_A80,
    arg: 0x3B10_A88,
};
const MAILBOXES: &[Mailbox] = &[
    TABLE,
    Mailbox {
        name: "MP1-IFv13",
        msg: 0x3B10_528,
        rsp: 0x3B10_578,
        arg: 0x3B10_998,
    },
    Mailbox {
        name: "MP1-IFv12",
        msg: 0x3B10_528,
        rsp: 0x3B10_564,
        arg: 0x3B10_998,
    },
    Mailbox {
        name: "RSMU-桌面",
        msg: 0x3B10_524,
        rsp: 0x3B10_570,
        arg: 0x3B10_A40,
    },
    Mailbox {
        name: "MP1-IFv11",
        msg: 0x3B10_530,
        rsp: 0x3B10_57C,
        arg: 0x3B10_9C4,
    },
    Mailbox {
        name: "MP1-IFv9",
        msg: 0x3B10_528,
        rsp: 0x3B10_564,
        arg: 0x3B10_598,
    },
    Mailbox {
        name: "RSMU-TR",
        msg: 0x3B10_51C,
        rsp: 0x3B10_568,
        arg: 0x3B10_590,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    UnknownCommand,
    Prerequisite,
    Busy,
    Other(u32),
}
impl Status {
    fn from_code(code: u32) -> Self {
        match code {
            1 => Self::Ok,
            0xff => Self::Failed,
            0xfe => Self::UnknownCommand,
            0xfd => Self::Prerequisite,
            0xfc => Self::Busy,
            other => Self::Other(other),
        }
    }
}
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("OK"),
            Self::Failed => f.write_str("Failed"),
            Self::UnknownCommand => f.write_str("UnknownCmd"),
            Self::Prerequisite => f.write_str("RejectedPrereq"),
            Self::Busy => f.write_str("Busy"),
            Self::Other(code) => write!(f, "0x{code:X}"),
        }
    }
}
pub struct Reply {
    pub status: Status,
    pub args: [u32; 6],
}
impl Reply {
    pub fn ensure_ok(self) -> Result<[u32; 6]> {
        if self.status != Status::Ok {
            bail!("SMU 响应: {}", self.status)
        }
        Ok(self.args)
    }
}

trait RegisterIo {
    fn read(&self, address: u32) -> Result<u32>;
    fn write(&self, address: u32, value: u32) -> Result<()>;
}
struct Timing {
    ready: Duration,
    response: Duration,
    poll: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            ready: Duration::from_millis(500),
            response: Duration::from_millis(1500),
            poll: Duration::from_micros(100),
        }
    }
}

fn wait_response(
    io: &impl RegisterIo,
    address: u32,
    timeout: Duration,
    poll: Duration,
) -> Result<u32> {
    let start = Instant::now();
    loop {
        let status = io.read(address)?;
        if status != 0 {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            bail!("SMU 响应寄存器 0x{address:X} 等待超时")
        }
        std::thread::sleep(poll);
    }
}

fn exchange(
    io: &impl RegisterIo,
    mailbox: Mailbox,
    operation: u32,
    args: [u32; 6],
    timing: &Timing,
) -> Result<Reply> {
    wait_response(io, mailbox.rsp, timing.ready, timing.poll).context("邮箱初始等待")?;
    io.write(mailbox.rsp, 0)?;
    for (index, value) in args.iter().enumerate() {
        io.write(mailbox.arg + index as u32 * 4, *value)?;
    }
    io.write(mailbox.msg, operation)?;
    let code = wait_response(io, mailbox.rsp, timing.response, timing.poll)
        .with_context(|| format!("命令 0x{operation:X}"))?;
    let status = Status::from_code(code);
    let mut args = [0u32; 6];
    if status == Status::Ok {
        for (index, value) in args.iter_mut().enumerate() {
            *value = io.read(mailbox.arg + index as u32 * 4)?;
        }
    }
    Ok(Reply { status, args })
}

pub struct Smn {
    cfg: File,
    index: u64,
    data: u64,
    _lock: File,
}
impl Smn {
    pub fn open(index: u64, data: u64) -> Result<Self> {
        let vendor = fs::read_to_string("/sys/bus/pci/devices/0000:00:00.0/vendor")
            .context("读取 PCI Host Bridge 厂商")?;
        if vendor.trim() != "0x1022" {
            bail!("PCI 00:00.0 不是 AMD Host Bridge, 拒绝访问 SMN")
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open("/run/lock/rustinfo-smu.lock")
            .context("打开 SMU 锁失败 (需 root)")?;
        // SAFETY: flock operates on the valid descriptor owned by lock; closing releases it.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("另一个 rustinfo 进程正在访问 SMU");
        }
        let cfg = OpenOptions::new()
            .read(true)
            .write(true)
            .open(CFG_PATH)
            .with_context(|| format!("打开 {CFG_PATH} 失败 (需 root)"))?;
        Ok(Self {
            cfg,
            index,
            data,
            _lock: lock,
        })
    }
    pub fn send_command(&self, mailbox: Mailbox, operation: u32, args: [u32; 6]) -> Result<Reply> {
        exchange(self, mailbox, operation, args, &Timing::default())
    }
}
impl RegisterIo for Smn {
    fn read(&self, address: u32) -> Result<u32> {
        self.cfg.write_all_at(&address.to_le_bytes(), self.index)?;
        let mut bytes = [0u8; 4];
        self.cfg.read_exact_at(&mut bytes, self.data)?;
        Ok(u32::from_le_bytes(bytes))
    }
    fn write(&self, address: u32, value: u32) -> Result<()> {
        self.cfg.write_all_at(&address.to_le_bytes(), self.index)?;
        self.cfg.write_all_at(&value.to_le_bytes(), self.data)?;
        Ok(())
    }
}

pub fn require_supported_cpu() -> Result<()> {
    if std::env::var_os("RUSTINFO_SMU_FORCE").is_some_and(|value| value == "1") {
        return Ok(());
    }
    let identity =
        CpuIdentity::parse(&fs::read_to_string("/proc/cpuinfo").context("读 /proc/cpuinfo")?);
    if !identity.supports_smu() {
        bail!("CPU 未验证 ({}/{:?}/{:?}): SMU 邮箱仅验证 AMD Strix Point family 0x1A/model 0x24; 显式跳过检查: RUSTINFO_SMU_FORCE=1",identity.vendor,identity.family,identity.model)
    }
    Ok(())
}

pub fn smu_table() -> Result<()> {
    require_supported_cpu()?;
    let module_table = std::path::Path::new("/sys/kernel/debug/rustinfo_smu/table");
    let buf = if module_table.exists() {
        // The module owns its mailbox transaction. Do not send competing PCI commands first.
        let buf = fs::read(module_table).context("读取 rustinfo_smu 遥测表失败")?;
        println!("经 rustinfo_smu 模块读取 {} 字节", buf.len());
        buf
    } else {
        let smn = Smn::open(0xC4, 0xC8)?;
        let version = smn.send_command(TABLE, 0x06, [0; 6])?.ensure_ok()?;
        println!("pm_table 版本 0x{:06X}", version[0]);
        let base_args = smn
            .send_command(TABLE, 0x66, [1, 1, 0, 0, 0, 0])?
            .ensure_ok()?;
        let base = base_args[0] as u64 | ((base_args[1] as u64) << 32);
        if base == 0 || base > 0x0000_8000_0000_0000 {
            bail!("DRAM 基址异常: 0x{base:X}")
        }
        let map = fs::read_to_string("/proc/iomem").context("读 /proc/iomem")?;
        let (label, available) = iomem_region(&map, base)?;
        if label.to_ascii_lowercase().contains("system ram") {
            bail!("目标落在 System RAM, 拒绝读取")
        }
        let size = available.min(0x1000) as usize;
        if size < 0x100 {
            bail!("SMU 保留区过小 ({size:#X})")
        }
        smn.send_command(TABLE, 0x65, [0; 6])?.ensure_ok()?;
        let file = File::open("/dev/mem").context("打开 /dev/mem; 可改用 rustinfo_smu 模块")?;
        let mut buf = vec![0; size];
        file.read_exact_at(&mut buf, base)
            .context("读取 SMU 保留区")?;
        buf
    };
    dump_table(&buf)
}

fn dump_table(buf: &[u8]) -> Result<()> {
    if buf.len() < 0x80 {
        bail!("SMU 遥测表被截断: {} 字节", buf.len())
    }
    let out = "/tmp/smu_pm_table.bin";
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(out)
        .context("创建 SMU 表转储")?;
    use std::io::Write;
    output.write_all(buf)?;
    println!(
        "已写入 {out}; VDDCR 原始值 {:.1}",
        f32::from_le_bytes(buf[0x7c..0x80].try_into()?)
    );
    for (row, bytes) in buf.as_chunks::<16>().0.iter().take(8).enumerate() {
        println!(
            "{:04X}: {}",
            row * 16,
            bytes
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    for (index, bytes) in buf.as_chunks::<4>().0.iter().enumerate() {
        println!("0x{:04X} {}", index * 4, f32::from_le_bytes(*bytes));
    }
    Ok(())
}

fn iomem_region(text: &str, base: u64) -> Result<(String, u64)> {
    let mut matches = Vec::new();
    for line in text.lines() {
        let Some((range, label)) = line.split_once(':') else {
            continue;
        };
        let Some((start, end)) = range.trim().split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        if start <= base && base <= end {
            matches.push((end.saturating_sub(start), label.trim(), end - base));
        }
    }
    let (_, label, remaining) = matches
        .into_iter()
        .min_by_key(|(size, _, _)| *size)
        .context("SMU 基址不在 /proc/iomem 中")?;
    Ok((label.into(), remaining.saturating_add(1)))
}

pub fn probe() -> Result<()> {
    require_supported_cpu()?;
    let module_info = std::path::Path::new("/sys/kernel/debug/rustinfo_smu/info");
    if module_info.exists() {
        println!(
            "rustinfo_smu 已加载:\n{}",
            fs::read_to_string(module_info).context("读取模块信息")?
        );
        return Ok(());
    }
    println!("SMU 邮箱探针 (GetSMUVersion 0x02)");
    for &(index, data) in SMN_BRIDGES {
        let smn = Smn::open(index, data)?;
        for &mailbox in MAILBOXES {
            match smn.send_command(mailbox, 0x02, [1, 0, 0, 0, 0, 0]) {
                Ok(reply) if reply.status == Status::Ok && reply.args[0] > 1 => {
                    let version = reply.args[0];
                    println!(
                        "命中 {}: {}.{}.{} (0x{version:08X})",
                        mailbox.name,
                        (version >> 16) & 0xff,
                        (version >> 8) & 0xff,
                        version & 0xff
                    );
                    return Ok(());
                }
                Ok(reply) => println!("{}: {}", mailbox.name, reply.status),
                Err(error) => println!("{}: {error:#}", mailbox.name),
            }
        }
    }
    bail!("所有已知邮箱均未命中")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    struct Fake {
        responses: RefCell<VecDeque<u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        values: RefCell<HashMap<u32, u32>>,
    }
    impl Fake {
        fn new(responses: &[u32]) -> Self {
            Self {
                responses: RefCell::new(responses.iter().copied().collect()),
                writes: RefCell::new(Vec::new()),
                values: RefCell::new(HashMap::new()),
            }
        }
    }
    impl RegisterIo for Fake {
        fn read(&self, address: u32) -> Result<u32> {
            if address == CONTROL.rsp {
                Ok(self.responses.borrow_mut().pop_front().unwrap_or(0))
            } else {
                Ok(*self.values.borrow().get(&address).unwrap_or(&0))
            }
        }
        fn write(&self, address: u32, value: u32) -> Result<()> {
            self.writes.borrow_mut().push((address, value));
            self.values.borrow_mut().insert(address, value);
            Ok(())
        }
    }
    fn timing() -> Timing {
        Timing {
            ready: Duration::ZERO,
            response: Duration::ZERO,
            poll: Duration::ZERO,
        }
    }
    #[test]
    fn mailbox_handshake_writes_all_arguments_before_the_command() {
        let fake = Fake::new(&[1, 1]);
        let reply = exchange(&fake, CONTROL, 0x4c, [0x0fffffe5, 0, 0, 0, 0, 0], &timing()).unwrap();
        assert_eq!(reply.ensure_ok().unwrap()[0], 0x0fffffe5);
        let writes = fake.writes.borrow();
        assert_eq!(writes.len(), 8);
        assert_eq!(writes[0], (CONTROL.rsp, 0));
        assert_eq!(writes[1], (CONTROL.arg, 0x0fffffe5));
        assert_eq!(writes[7], (CONTROL.msg, 0x4c));
    }
    #[test]
    fn a_busy_initial_mailbox_performs_no_writes() {
        let fake = Fake::new(&[0]);
        assert!(exchange(&fake, CONTROL, 0x4c, [0; 6], &timing()).is_err());
        assert!(fake.writes.borrow().is_empty());
    }
    #[test]
    fn firmware_rejection_and_response_timeout_are_errors() {
        for code in [0xff, 0xfe, 0xfd, 0xfc, 0xab] {
            let fake = Fake::new(&[1, code]);
            assert!(exchange(&fake, CONTROL, 0x4c, [0; 6], &timing())
                .unwrap()
                .ensure_ok()
                .is_err());
        }
        assert!(exchange(&Fake::new(&[1, 0]), CONTROL, 0x4c, [0; 6], &timing()).is_err());
    }
    #[test]
    fn reserved_child_regions_take_precedence_over_system_ram_parents() {
        let (label, size) = iomem_region(
            "00001000-00009fff : System RAM\n  00004000-00004fff : reserved\n",
            0x4000,
        )
        .unwrap();
        assert_eq!(label, "reserved");
        assert_eq!(size, 0x1000);
        assert!(iomem_region("", 0x4000).is_err());
    }
    #[test]
    fn a_short_module_dump_returns_an_error_without_indexing_it() {
        assert!(dump_table(&[0; 3]).is_err());
    }
}
