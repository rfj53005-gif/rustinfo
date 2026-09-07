#!/usr/bin/env python3
"""pm_table 偏移逆向扫表: 逐核钉载采样 + 基准真值采集。
需 root 运行 (debugfs 表读取)。输出 /tmp/pm_sweep/{tag}.bin + {tag}.json
"""
import json
import os
import subprocess
import sys
import time

TABLE = "/sys/kernel/debug/rustinfo_smu/table"
RUSTINFO = "/usr/local/bin/rustinfo"
OUT = "/tmp/pm_sweep"
NCPU = os.cpu_count() or 20

os.makedirs(OUT, exist_ok=True)


def hwmon_val(chip: str, attr: str) -> float:
    for d in os.listdir("/sys/class/hwmon"):
        base = f"/sys/class/hwmon/{d}"
        try:
            if open(f"{base}/name").read().strip() != chip:
                continue
            for f in os.listdir(base):
                if f.endswith("_input") and f.startswith(attr):
                    return float(open(f"{base}/{f}").read())
        except OSError:
            continue
    return float("nan")


def ground_truth():
    freqs = []
    for i in range(NCPU):
        try:
            freqs.append(
                float(open(f"/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq").read())
            )
        except OSError:
            freqs.append(0.0)
    return {
        "freqs_khz": freqs,
        "tctl_c": hwmon_val("k10temp", "temp") / 1000.0,
        "dump": subprocess.run([RUSTINFO, "dump"], capture_output=True, text=True).stdout,
    }


def snap(tag: str, cpu=None):
    tbl = open(TABLE, "rb").read()
    open(f"{OUT}/{tag}.bin", "wb").write(tbl)
    gt = ground_truth()
    gt["tag"] = tag
    gt["cpu"] = cpu
    gt["ts"] = time.time()
    json.dump(gt, open(f"{OUT}/{tag}.json", "w"))
    print(f"  {tag} 完成 (tctl={gt['tctl_c']:.1f}C)", flush=True)


def spin(cpu: int, secs: int):
    return subprocess.Popen(
        ["taskset", "-c", str(cpu), "sh", "-c",
         f"end=$((SECONDS+{secs})); while [ $SECONDS -lt $end ]; do :; done"],
        stdout=subprocess.DEVNULL,
    )


def main():
    print("== 空闲基线 ==", flush=True)
    snap("idle0")
    time.sleep(0.5)

    cores = list(range(NCPU // 2))  # 每物理核一个代表线程
    print(f"== 逐核钉载 (物理核 {cores}) ==", flush=True)
    for c in cores:
        p = spin(c, 6)
        time.sleep(3.0)  # 等 boost / 功率 / 表刷新稳定
        snap(f"core{c}", cpu=c)
        p.wait()
        time.sleep(0.8)

    print("== 空闲恢复 ==", flush=True)
    time.sleep(1.0)
    snap("idle1")

    print("== 全核满载 ==", flush=True)
    ps = [spin(c, 6) for c in range(NCPU)]
    time.sleep(3.0)
    snap("allcore")
    for p in ps:
        p.wait()

    print(f"完成: {len(os.listdir(OUT))} 个文件 → {OUT}")


if __name__ == "__main__":
    sys.exit(main())
