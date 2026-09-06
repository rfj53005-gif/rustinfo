# rustinfo

HWiNFO 风格的 Linux 硬件监控,纯 Rust 实现。实时 TUI 面板 + 一次性读数 + CSV 记录三种模式,**全部数据来自内核原生接口**(hwmon / procfs / MSR / RAPL),不依赖任何专有驱动或常驻后台进程。

在 AMD Strix Point(锐龙 AI 9 365)上实测,额外实现了**自研只读 SMU 通道**(ryzen_smu/ryzenadj 在这代上读不了遥测表,见 [SMU 模块](#可选smu-遥测模块amd-实验性))。

## 功能一览

- 实时 TUI:CPU 每线程占用/频率条、内存/Swap/VRAM、电池、传感器芯片网格、温度/功率/内存历史曲线
- **每物理核功率**(MSR 直读)与 **RAPL Package 功率** —— 需 root,见[权限](#权限普通用户-vs-root)
- GPU(DPM 档位/占用率)、NPU 功耗、NVMe 温度与 IO 速率/繁忙度、Wi-Fi 信号、PSI 压力、电池健康度/续航估算
- CSV 记录模式:所有读数逐列落盘,适合长时间浸泡测试(如降压验证)

## 安装

**前置**:Rust 工具链(没有就先装 rustup:`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y`)。Linux + systemd 任意发行版;内核越新传感器越全。

### 方式一:cargo 直接安装(最简单)

```bash
cargo install --git https://github.com/rfj53005-gif/rustinfo
rustinfo          # 直接运行 (在 ~/.cargo/bin)
```

### 方式二:源码构建

```bash
git clone https://github.com/rfj53005-gif/rustinfo && cd rustinfo
cargo build --release
./target/release/rustinfo dump
```

### 方式三:./install.sh(想要全量读数时推荐)

```bash
./install.sh
```

它做四件事:① `cargo build --release`;② 把二进制复制一份 **root 属主副本**到 `/usr/local/bin/rustinfo`;③ 写一条只对这一个文件生效的 sudoers 免密规则(`/etc/sudoers.d/rustinfo`);④ 在 `~/.local/bin/rustinfo` 放一个包装器,自动免密 `sudo` 运行。改代码后重跑一次即可同步(sudoers 只创建一次)。**不想要 sudo 规则就不跑这个脚本**,用手动 `sudo rustinfo` 一样能拿全量读数。

> 需要图形会话的 polkit 授权(pkexec);纯 TTY 下请手动执行脚本里的步骤。

## 使用

### 实时面板(默认)

```bash
rustinfo            # 或 rustinfo tui
rustinfo -i 0.5     # 采样间隔 0.5 秒 (0.2~60)
```

| 按键 | 作用 |
|---|---|
| `q` / `Esc` | 退出 |
| `空格` | 暂停/恢复采样 |
| `l` | 开始/停止 CSV 记录(文件在当前目录,文件名显示在底部) |
| `+` / `-` | 增大/减小采样间隔 |

### 一次性读数(脚本友好)

```bash
rustinfo dump
```

打印主机信息、CPU(含 Package 功率)、内存、电池、全部传感器芯片后退出。

### CSV 记录(无界面,适合浸泡测试)

```bash
rustinfo log soak.csv -i 2        # 每 2 秒一行, Ctrl-C 结束
rustinfo log -i 1                 # 不指定文件名则自动 rustinfo_时间戳.csv
```

列名规则:`<芯片>.<读数>_<单位>`,例如 `k10temp.Tctl_C`、`amdgpu.PPT_W`、`rapl.C00_W`(C00-C13 是硬件核编号)、每线程为 `t00_pct`/`t00_mhz`。电压电流是瞬时值,功率/速率/占用率是两次采样间的增量均值。

典型用法(降压验证):

```bash
# 终端 A: 挂记录
sudo rustinfo log soak.csv -i 2
# 终端 B: 跑受控负载, 事后用 pandas/Excel 分析 soak.csv
```

## 权限:普通用户 vs root

| 读数 | 普通用户 | root |
|---|---|---|
| 温度/风扇/电压/电流/GPU DPM/磁盘/网络/PSI/电池 | ✅ | ✅ |
| **RAPL Package 功率**、**每物理核功率** | ❌(不显示) | ✅ |

原因:内核把 RAPL `energy_uj` 和 `/dev/cpu/*/msr` 限制为 root(防能耗侧信道攻击)。跑 `install.sh` 后包装器会自动免密提权,其余一切照旧;CSV 由 root 生成时会自动把属主还给你。

## 可选:SMU 遥测模块(AMD,实验性)

在 Strix Point 这类新平台上,ryzen_smu(不认 family 0x1A)和 ryzenadj 的表读取(被 IO_STRICT_DEVMEM 拦)都拿不到 SMU 遥测。rustinfo 自研了只读通道:**用户态探针** + **只读内核模块**(经 memremap 读 SMU 保留区,每次读触发一次全新表传输)。

```bash
# 1. 探测: 只发只读的 GetSMUVersion 命令
sudo rustinfo smu-probe
# 期望输出: ✅ 命中: RSMU-APU — SMU 版本 ... (0x0B5D0B00)

# 2. 抓一张表 (hexdump + 已知偏移候选), 写入 /tmp/smu_pm_table.bin
sudo rustinfo smu-table

# 3. 内核模块 (持续读取): 构建需要 clang (CachyOS 内核为 clang+LTO)
cd module && make LLVM=1
sudo modprobe -r ryzen_smu          # 避免竞争 (它在 0x1A 上本就未绑定)
sudo insmod rustinfo_smu.ko
sudo cat /sys/kernel/debug/rustinfo_smu/info    # 版本/基址/邮箱
sudo cat /sys/kernel/debug/rustinfo_smu/table > table.bin   # 每次读都是新鲜数据
```

安全说明:模块**只含四个只读命令**(0x02/0x06/0x66/0x65),不向固件发送任何写值命令(CO、限值碰都不碰);所有交互带超时、返回码校验、互斥锁串行化。模块是 `insmod` 手动加载的,**重启后消失**,验证稳定前不会放进开机流程。偏移表还在逆向中(见下)。

## 数据采集清单(Strix Point 实测)

- **CPU**:每线程占用/实时频率、总占用、RAPL Package 功率、每物理核功率(MSR C001_029A,标签 C00-C03 为 Zen5 大核、C08-C13 为 Zen5c 核——编号跳过 4-7 是硬件拓扑,不是 bug)
- **GPU (Radeon 880M)**:edge 温度/PPT/vddgfx、VRAM/GTT 用量、DPM 当前档+最高档(sclk/mclk/socclk/fclk/vclk/dclk)、GPU/VCN 占用率
- **NPU (amdxdna)**:功耗
- **存储**:NVMe 双温度、整盘读写速率与繁忙度
- **网络**:每网卡收发速率、Wi-Fi rssi
- **系统压力**:PSI(cpu/mem/io 的 some/full)、调速器与 EPP
- **板载**:k10temp Tctl、ACPI 温区、风扇、USB-C 输入电压电流
- **电源**:电池电压/电流/功率/剩余 Wh/健康度/续航估算、外接电源

## 常见问题

- **看不到 CPU 功耗/每核功率?** 用 `sudo` 跑,或跑一次 `install.sh` 配免密。普通用户下其余读数不受影响。
- **每核编号为什么从 C03 跳到 C08?** 硬件 core_id:Zen5 复合体占 0-3,Zen5c 占 8-13,各留 8 个编号窗口。和 `/proc/cpuinfo` 的 "core id" 一致。
- **每核功率是 0.00W?** 核进 CC6 时钟门控时计数器冻结,只计活跃能耗,深睡的核读数≈0 是正常语义。
- **`smu-probe` 无应答?** 你的平台邮箱布局可能不同(探针只覆盖已知候选),欢迎带 CPU 型号提 issue。
- **PSI 全是 0?** 机器空闲的正常表现;卡顿时 mem/io 的值会爬升。
- **NPU 温度显示 N/A?** 本机驱动不提供该读数(sensors 里也是 N/A)。

## 踩坑记录:RAPL "core" 域的真相

sysfs `intel-rapl:0:0` 名叫 "core",实测证明它**不是全部核心的合计**:背后是 MSR `C001_029A`(每物理核一份的能量计数器),而内核 intel_rapl 驱动只在 lead CPU(cpu0)上注册了一个域——那个值是 **cpu0 一颗核的功率**。验证:空闲/钉核满载两个窗口,sysfs core 增量与 cpu0 计数器×单位**精确相等**;钉载其他核时 sysfs "core" 纹丝不动。rustinfo 因此绕过它,按核直读 MSR(单位 2^-Esu J,Esu 从 `C001_0299[12:8]` 读)。

## 平台与已知局限

- 在 AMD Strix Point + CachyOS(内核 7.2.3)上完整实测;其他 AMD Zen 平台大体可用(hwmon 部分通用,SMU/每核功率随平台而定);Intel 未测试,欢迎反馈。
- SMU 每核 VID/电流:pm_table 偏移逆向进行中(已确认表活性,钉载差分能定位负载响应字段)。
- RAPL 的 powercap 限额文件在此 APU 上不存在;有效频率(APERF/MPERF)缺 P0 基准,均未实现。
- 终端建议 ≥110 列、≥30 行。

## 致谢

- [RyzenAdj (FlyGoat)](https://github.com/FlyGoat/RyzenAdj) — 本机 CO 调整工作流的核心工具,其 SMU 服务实现是本项目邮箱协议的参考之一
- [ryzen_smu (leogx9r)](https://github.com/leogx9r/ryzen_smu) — SMN 桥寄存器布局与邮箱握手流程的参考实现
- [ratatui](https://ratatui.rs/) 与 [awesome-ratatui](https://github.com/ratatui/awesome-ratatui) 生态中的监控类 TUI 架构
