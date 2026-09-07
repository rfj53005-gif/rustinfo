#!/usr/bin/env python3
"""pm_table 偏移关联分析: 把 /tmp/pm_sweep 的快照与基准真值对齐,
输出每偏移与 Tctl/Package功率的相关系数、单核选择性归属、常量区。
"""
import glob
import json
import math
import re
import struct

SWEEP = "/tmp/pm_sweep"


def pearson(xs, ys):
    n = len(xs)
    if n < 3:
        return 0.0
    mx, my = sum(xs) / n, sum(ys) / n
    sx = math.sqrt(sum((x - mx) ** 2 for x in xs))
    sy = math.sqrt(sum((y - my) ** 2 for y in ys))
    if sx < 1e-9 or sy < 1e-9:
        return 0.0
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / (sx * sy)


def parse_dump(text):
    pkg = re.search(r"Package ([\d.]+)W", text)
    cores = {int(m[0]): float(m[1]) for m in re.findall(r"C(\d+)\s+([\d.]+)W", text)}
    tctl = re.search(r"Tctl\s+([\d.]+)", text)
    return {
        "pkg_w": float(pkg.group(1)) if pkg else None,
        "core_w": cores,
        "tctl_dump": float(tctl.group(1)) if tctl else None,
    }


def main():
    runs = []
    for jf in sorted(glob.glob(f"{SWEEP}/*.json")):
        gt = json.load(open(jf))
        tag = gt["tag"]
        tbl = open(f"{SWEEP}/{tag}.bin", "rb").read()
        d = parse_dump(gt["dump"])
        ncore = len(gt["freqs_khz"]) // 2
        freq_mhz = {
            c: max(gt["freqs_khz"][c], gt["freqs_khz"][c + ncore]) / 1000.0
            for c in range(ncore)
        }
        runs.append({"tag": tag, "cpu": gt["cpu"], "tbl": tbl, "tctl": gt["tctl_c"],
                     "pkg_w": d["pkg_w"], "core_w": d["core_w"], "freq_mhz": freq_mhz})
    runs.sort(key=lambda r: (r["tag"] != "idle0", r["tag"]))
    print(f"{len(runs)} 个快照: {[r['tag'] for r in runs]}\n")

    ncore = len(runs[0]["freq_mhz"])
    n = len(runs[0]["tbl"]) // 4
    tctls = [r["tctl"] for r in runs]
    pkgs = [r["pkg_w"] or 0.0 for r in runs]

    # 每核真值序列: 该核被钉载的 run 里其他核在低载 → 单核选择性
    result = {"global": {}, "per_core": {}, "const": {}}
    rows = []
    for off in range(n * 4):
        if off % 4:
            continue
        vals = []
        ok = True
        for r in runs:
            v = struct.unpack_from("<f", r["tbl"], off)[0]
            if not math.isfinite(v) or abs(v) > 1e9:
                ok = False
                break
            vals.append(v)
        if not ok:
            continue
        vmin, vmax = min(vals), max(vals)
        if vmax - vmin < 1e-4 and abs(vmax) < 1000:
            result["const"].setdefault(round(vmin, 2), []).append(off)
            continue
        r_tctl = pearson(vals, tctls)
        r_pkg = pearson(vals, pkgs)
        # 单核选择性: 哪个 core 的 run 里该偏移是全场最高
        sel_core, sel_gain = None, 0.0
        for c in range(ncore):
            own = vals[[r["tag"] for r in runs].index(f"core{c}")]
            others = [v for r, v in zip(runs, vals)
                      if not (r["tag"] == f"core{c}" or r["tag"].startswith("idle"))]
            if not others:
                continue
            gain = own - max(others)
            if own > 0.1 and gain > sel_gain:
                sel_core, sel_gain = c, gain
        rows.append((off, vals, r_tctl, r_pkg, sel_core, sel_gain))

    print("== 与 Tctl 强相关 (|r|>0.9) ==")
    for off, vals, rt, rp, sc, sg in sorted(rows, key=lambda x: -abs(x[2])):
        if abs(rt) > 0.9:
            print(f"  0x{off:04X}  r_tctl={rt:+.3f} r_pkg={rp:+.3f}  值: {vals[0]:.2f} → {vals[-1]:.2f}  极差 {min(vals):.1f}~{max(vals):.1f}")
            result["global"].setdefault("tctl", []).append({"off": off, "r": round(rt, 3)})

    print("\n== 与 Package 功率强相关 (|r|>0.9) ==")
    for off, vals, rt, rp, sc, sg in sorted(rows, key=lambda x: -abs(x[3])):
        if abs(rp) > 0.9 and abs(pearson(vals, tctls)) <= 0.9:
            print(f"  0x{off:04X}  r_pkg={rp:+.3f} r_tctl={rt:+.3f}  极差 {min(vals):.1f}~{max(vals):.1f}")
            result["global"].setdefault("pkg_w", []).append({"off": off, "r": round(rp, 3)})

    print("\n== 单核选择字段 (该核钉载时唯一飙升) ==")
    for c in range(ncore):
        mine = [(off, vals, rt, rp) for off, vals, rt, rp, sc, sg in rows
                if sc == c and sg > 0.2]
        if not mine:
            continue
        # 与该核频率/功率的相关, 区分时钟/功率/电压
        fr_c = [r["freq_mhz"][c] for r in runs]
        w_c = [r["core_w"].get(c, 0.0) for r in runs]
        out = []
        for off, vals, rt, rp in sorted(mine):
            r_fr = pearson(vals, fr_c)
            r_w = pearson(vals, w_c)
            mid = vals[[r["tag"] for r in runs].index(f"core{c}")]
            kind = "freq" if (abs(r_fr) > abs(r_w) and abs(r_fr) > 0.5) else (
                   "watt" if abs(r_w) > 0.5 else "?")
            out.append((off, kind, r_fr, r_w, mid))
            result["per_core"].setdefault(str(c), []).append(
                {"off": off, "kind": kind, "r_freq": round(r_fr, 3), "r_watt": round(r_w, 3),
                 "val_own_run": round(mid, 3)})
        print(f"  核 {c}: " + ", ".join(f"0x{o:04X}[{k} rf={rf:+.2f} rw={rw:+.2f} v={v:.2f}]"
                                        for o, k, rf, rw, v in out[:8]))

    print("\n== 常量区 (从未变化的值) ==")
    for v in sorted(result["const"], key=lambda x: -abs(x)):
        offs = result["const"][v]
        print(f"  {v:>10}: {len(offs)} 个偏移 (如 " + ", ".join(f"0x{o:04X}" for o in offs[:6]) + ")")

    json.dump(result, open("/tmp/pm_map.json", "w"), indent=1)
    print("\n候选地图已存 /tmp/pm_map.json")


if __name__ == "__main__":
    main()
