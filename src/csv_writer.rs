//! Privileged sampling must not grant privileged filesystem writes through CSV paths.
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

pub const HELPER: &str = "__csv-writer";

pub enum Output {
    Local(BufWriter<File>),
    Caller(CallerWriter),
}
pub struct CallerWriter {
    input: Option<ChildStdin>,
    replies: BufReader<ChildStdout>,
    pending: Vec<u8>,
    child: Child,
}

fn caller() -> Result<Option<(u32, u32)>> {
    // SAFETY: geteuid has no arguments and does not access Rust-owned memory.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(None);
    }
    let Ok(uid) = std::env::var("SUDO_UID") else {
        return Ok(None);
    };
    let uid = uid.parse::<u32>().context("SUDO_UID 无效")?;
    if uid == 0 {
        return Ok(None);
    }
    let gid = std::env::var("SUDO_GID")
        .context("缺少 SUDO_GID")?
        .parse::<u32>()
        .context("SUDO_GID 无效")?;
    Ok(Some((uid, gid)))
}

fn open_file(path: &str, exclusive: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true);
    }
    let file = options
        .open(path)
        .with_context(|| format!("无法创建 CSV {path}"))?;
    if !file.metadata()?.is_file() {
        bail!("CSV 路径必须是普通文件")
    }
    if !exclusive {
        file.set_len(0)?;
    }
    Ok(file)
}

impl Output {
    pub fn open(path: &str, exclusive: bool) -> Result<Self> {
        if let Some((uid, gid)) = caller()? {
            // CommandExt::uid clears supplementary groups before dropping the child's UID.
            let mut child = Command::new(std::env::current_exe()?)
                .args([HELPER, path, if exclusive { "new" } else { "replace" }])
                .uid(uid)
                .gid(gid)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("启动普通用户 CSV 写入器")?;
            let input = child.stdin.take().context("CSV 写入管道未打开")?;
            let stdout = child.stdout.take().context("CSV 写入器没有就绪通道")?;
            let mut writer = CallerWriter {
                input: Some(input),
                replies: BufReader::new(stdout),
                pending: Vec::new(),
                child,
            };
            let mut ready = String::new();
            writer.replies.read_line(&mut ready)?;
            if ready != "READY\n" {
                let error = writer.error_text();
                bail!("CSV 写入器无法创建文件: {error}")
            }
            Ok(Self::Caller(writer))
        } else {
            Ok(Self::Local(BufWriter::new(open_file(path, exclusive)?)))
        }
    }
    pub fn finish(&mut self) -> Result<()> {
        match self {
            Self::Local(file) => file.flush().context("刷新 CSV"),
            Self::Caller(writer) => {
                writer.flush_record()?;
                writer.input.take();
                let status = writer.child.wait()?;
                if !status.success() {
                    bail!("CSV 写入器失败: {}", writer.error_text())
                }
                Ok(())
            }
        }
    }
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Local(file) => file.write(bytes),
            Self::Caller(writer) => {
                if writer.input.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "CSV 写入器已关闭",
                    ));
                }
                writer.pending.extend_from_slice(bytes);
                Ok(bytes.len())
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Local(file) => file.flush(),
            Self::Caller(writer) => writer.flush_record(),
        }
    }
}
const MAX_RECORD: usize = 16 * 1024 * 1024;
impl CallerWriter {
    fn flush_record(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if self.pending.len() > MAX_RECORD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "CSV 行超过 16MiB",
            ));
        }
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "CSV 写入器已关闭"))?;
        input.write_all(&(self.pending.len() as u64).to_le_bytes())?;
        input.write_all(&self.pending)?;
        input.flush()?;
        let mut reply = String::new();
        self.replies.read_line(&mut reply)?;
        if reply != "OK\n" {
            return Err(io::Error::other(format!(
                "CSV 写入失败: {}",
                self.error_text()
            )));
        }
        self.pending.clear();
        Ok(())
    }

    fn error_text(&mut self) -> String {
        let mut text = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut text);
        }
        text.trim().to_owned()
    }
}
impl Drop for CallerWriter {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.wait();
    }
}

pub fn serve(args: &[String]) -> Result<()> {
    // This mode is also directly invocable via sudo, so it must refuse root itself.
    // SAFETY: geteuid is a simple process credential query.
    if unsafe { libc::geteuid() } == 0 {
        bail!("CSV 写入器不能以 root 身份运行")
    }
    if args.len() != 2 || !matches!(args[1].as_str(), "new" | "replace") {
        bail!("CSV 写入器参数无效")
    }
    let mut file = BufWriter::new(open_file(&args[0], args[1] == "new")?);
    // The parent handles cancellation and closes the pipe after flushing. A foreground
    // Ctrl-C also reaches this child; ignoring it avoids cutting the final record short.
    // SAFETY: SIG_IGN is the platform's defined signal-disposition constant.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    println!("READY");
    io::stdout().flush()?;
    let mut input = io::stdin().lock();
    let mut row = Vec::new();
    loop {
        let mut length = [0u8; 8];
        if input.read(&mut length[..1])? == 0 {
            break;
        }
        input.read_exact(&mut length[1..])?;
        let length = u64::from_le_bytes(length);
        if length > MAX_RECORD as u64 {
            bail!("CSV 行超过 16MiB")
        }
        row.resize(length as usize, 0);
        input.read_exact(&mut row)?;
        file.write_all(&row)?;
        file.flush()?;
        println!("OK");
        io::stdout().flush()?;
    }
    file.flush()?;
    Ok(())
}
