# rustinfo

HWiNFO 风格的 Linux 硬件监控,纯 Rust 实现。**全部数据来自内核原生接口**(`/sys/class/hwmon`、`/proc`、power_supply、DRM、RAPL),不依赖任何专有驱动,不需要常驻后台进程。

## 功能

| 子命令 | 说明 |
|---|---|
| `rustinfo` / `rustinfo tui` | 实时终端面板:CPU 每线程占用/频率、内存/Swap、VRAM/GTT、电池、hwmon 传感器、RAPL 功率、GPU DPM、磁盘/网速、历史曲线 |
| `rustinfo dump` | 打印一次所有读数后退出(先预热采样 600ms,占用率/功率/速率是增量量) |
| `rustinfo log [文件] [-i 秒]` | 无界面 CSV 记录,Ctrl-C 结束;列覆盖 CPU 每线程、内存、电池、全部传感器 |

公共参数:`-i 秒数` 采样间隔(0.2~60)。TUI 按键:`q` 退出、`空格` 暂停、`l` 开关 CSV 记录、`+`/`-` 调整间隔。

## 采集清单(本机实测,Ryzen AI 9 365 / Strix Point)

- **CPU**:`/proc/stat` 总占用 + 每线程占用、`cpufreq` 每线程实时频率、**RAPL Package 功率**、**每物理核功率**(MSR C001_029A 按核直读,标签 C00-C03 为 Zen5 大核、C08-C13 为 Zen5c 核)
- **GPU (Radeon 880M)**:hwmon(edge 温度/PPT/vddgfx/vddnb)、VRAM/GTT 用量、DPM 当前档与最高档(sclk/mclk/socclk/fclk/vclk/dclk,时钟门控时当前档记 0)、GPU/VCN 占用率
- **NPU (amdxdna)**:功耗(温度本机驱动不提供读数)
- **存储**:NVMe 双温度传感器 + 整盘读写速率 MiB/s 与繁忙度 %(/proc/diskstats)
- **网络**:每网卡收发速率 MiB/s、Wi-Fi 信号强度 rssi dBm(/proc/net/wireless)
- **系统压力**:PSI(cpu/mem/io 的 some/full 阻塞率 avg10)、调速器与 EPP 策略
- **板载**:k10temp Tctl、ACPI 温区、双风扇、Wi-Fi 网卡温度、USB-C 输入电压电流
- **电源**:电池电压/电流/功率/剩余 Wh、**健康度**(满充/设计容量,本机 87%)、**续航估算**(剩余电荷/放电电流)、外接电源状态
- **SMU 遥测**(每核 VID/时钟/电流):`read_smu()` 已实现 `/sys/kernel/debug/ryzen_smu` 自动探测,**当前 ryzen_smu 0.1.7 尚不支持 Strix Point**(dmesg 确认识别出代号后未注册接口),目录出现即自动接入,无需改代码

## 为什么比 Windows 的 HWiNFO"信息少"

HWiNFO 在 Windows 上自带内核驱动,直接对 Super I/O、EC、SMU 寄存器做端口 I/O,所以能看到 VRM 温度、每核 VID、EC 风扇表等一切。Linux 的模型是**每个硬件要一个内核驱动**:数据分散在各接口,且笔记本 EC/VRM 普遍没有开源驱动——这部分是平台差距,不是工具差距。rustinfo 的原则是把内核**已经暴露**的接口挖尽。

已探明但暂时拿不到/放弃的:

- **SMU 每核 VID/电流**:ryzen_smu 0.1.7 不支持 Strix Point(debugfs 未注册),代码已留自动探测;
- **PPT 限额**:powercap constraint_* 文件在这颗 APU 上不存在(只能等 amd-pstate 暴露或读 ryzenadj 表);
- **有效频率(APERF/MPERF)**:amd-pstate-epp 无 base_frequency(P0 基准),校准不可靠,放弃。

## RAPL "core" 域的真相(踩坑记录)

sysfs `intel-rapl:0:0` 名叫 "core",但实测证明它**不是全部核心的合计**,而是 MSR `C001_029A`(每物理核一份的能量计数器)在 **lead CPU(cpu0)一颗核**上的读数——内核 intel_rapl 驱动只注册了一个域、只读一颗核。验证:空闲/钉核满载两个窗口,sysfs core 增量与 cpu0 计数器×单位**精确相等**(0.515J/0.788J);而把死循环钉在 cpu3 上,cpu3 计数器暴涨 191 万、sysfs "core" 纹丝不动。rustinfo 因此绕过 sysfs core 子域,直接按核读 MSR:

- 计数器 32 位,单位 2^-Esu J(Esu 从 `C001_0299[12:8]` 读,本机 Esu=16 → 15.26µJ/LSB),SMT 兄弟线程共享同一计数器;
- 核心进 CC6 时钟门控时计数器冻结,深睡的核读数≈0,是正常语义(只计活跃能耗);
- 单核满载实测 ~9.5W,与 Package 分解自洽(Σ每核 + GPU PPT + SoC ≈ Package)。

## SMU 自研通道(已打通 ✅)

ryzen_smu 0.1.7 的探测代码只认 CPU family 0x17/0x19,对 family 0x1A(Strix Point)直接报错退出,**从未尝试过邮箱握手**;ryzenadj 在这台机器上 monitor 路径也是坏的(`CONFIG_IO_STRICT_DEVMEM=y` 拦死 /dev/mem,ryzen_smu 模块又无兼容表 → "Unable to init power metric table")。rustinfo 实现了完整的自研通道:

- **用户态探针**(`rustinfo smu-probe`):SMN 桥 = PCI `00:00.0` config `0xC4/0xC8`,RSMU-APU 邮箱(`0x3B10A20/0x3B10A80/0x3B10A88`)六步握手,SMU 版本 `0x0B5D0B00`;
- **只读内核模块**(`module/rustinfo_smu.c`,clang 构建):把上述协议搬进内核,memremap SMU 保留区(基址 `0x85E180000`,iomem 标注 "RAM buffer"),经 debugfs `/sys/kernel/debug/rustinfo_smu/{table,info}` 输出。**每次读 table 触发一次全新传输**。模块只含四个只读命令(0x02/0x06/0x66/0x65),全部超时+返回码校验+互斥锁,不给固件发任何写值命令;
- **pm_table 已确认活性**(表版本 `0x5D0009`,8KB):空闲/钉核负载差分显示多个字段跟随负载变化(功率类 0x04/0x0C/0x14,电压类 0x74/0x7C/0x3C,温度类 0x44/0x4C/0x54/0x5C),95.00 常量(0x40-0x58)疑似温度墙,5.05/3.30 常量区疑似限值档。

偏移逆向进行中:钉核差分定位每核字段 → 与 MSR 每核功率/k10temp/RAPL 交叉验证 → 进 TUI。

## 权限模型(sudo 免密)

RAPL 的 `energy_uj` 与 `/dev/cpu/*/msr` 现代内核仅 root 可读(防能耗侧信道攻击),所以 **RAPL/每核功率需要 root**,其余读数普通用户即可。部署方式(`install.sh` 自动完成):

1. 二进制复制一份 **root 属主**副本到 `/usr/local/bin/rustinfo`——sudoers 只信任这份,项目目录里的开发版无特权;
2. `/etc/sudoers.d/rustinfo`:`ncf ALL=(root) NOPASSWD: /usr/local/bin/rustinfo`(仅限这一个二进制);
3. `~/.local/bin/rustinfo` 包装器:免密规则可用时自动 `sudo -n` 走 root,否则直接运行(降级无 RAPL 功率);
4. root 运行时生成的 CSV 会自动 chown 回发起 sudo 的用户,不留 root 属主文件。

**改代码后重跑 `./install.sh`** 同步 `/usr/local/bin` 副本(sudoers 只创建一次)。安全边界:任何能改写 /usr/local/bin/rustinfo 的人=能免密 root,该文件 root 属主,普通用户(含恶意软件)改不了。

## 致谢

- [RyzenAdj (FlyGoat)](https://github.com/FlyGoat/RyzenAdj) — 本机 CO 调整工作流的核心工具,其 SMU 服务实现是本项目邮箱协议的参考之一
- [ryzen_smu (leogx9r)](https://github.com/leogx9r/ryzen_smu) — SMN 桥寄存器布局与邮箱握手流程的参考实现
- [ratatui](https://ratatui.rs/) 与 [awesome-ratatui](https://github.com/ratatui/awesome-ratatui) 生态中的监控类 TUI 架构

## 构建

```
./install.sh          # 构建 + 部署
```

依赖仅 `ratatui` + `chrono` + `anyhow`,无其他运行时依赖。
