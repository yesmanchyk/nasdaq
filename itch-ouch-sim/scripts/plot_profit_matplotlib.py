#!/usr/bin/env python3
"""Matplotlib / Pandas plotter for historical profit per ms ($/ms).
Requires: pip install matplotlib pandas (or run via uv run --with matplotlib --with pandas)
"""

import argparse
import csv
from datetime import datetime
import os
import sys

try:
    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
except ImportError:
    print("Error: matplotlib is required for this script. Install with: pip install matplotlib", file=sys.stderr)
    sys.exit(1)

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CSV = os.path.normpath(os.path.join(SCRIPT_DIR, "..", "sim_history", "runs.csv"))
DEFAULT_OUT = os.path.normpath(os.path.join(SCRIPT_DIR, "..", "sim_history", "profit_per_ms.png"))

BOT_STYLE = {
    1: {"color": "#ef4444", "marker": "o", "label": "Bot-1 (MM + MeanRev)"},
    2: {"color": "#10b981", "marker": "s", "label": "Bot-2 (Mom + Taker)"},
    3: {"color": "#3b82f6", "marker": "^", "label": "Bot-3 (Smart AS+OBI)"},
    4: {"color": "#a855f7", "marker": "D", "label": "Bot-4 (RL Q+Bandit)"},
    5: {"color": "#f59e0b", "marker": "P", "label": "Bot-5 (C++ LowLat)"},
}

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--csv", default=DEFAULT_CSV)
    parser.add_argument("--out", default=DEFAULT_OUT)
    args = parser.parse_args()

    runs = {}
    with open(args.csv, "r", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            ts = int(row["timestamp_unix_s"])
            b_id = int(row["bot_id"])
            dur = float(row["duration_ms"])
            pnl = float(row["mtm_pnl_dollars"])
            rate = pnl / dur if dur > 0 else 0.0

            if ts not in runs:
                runs[ts] = {
                    "time": datetime.fromtimestamp(ts).strftime("%m-%d %H:%M:%S"),
                    "dur": dur,
                    "bots": {},
                }
            runs[ts]["bots"][b_id] = rate

    sorted_ts = sorted(runs.keys())
    x_labels = [f"{runs[ts]['time']}\n({runs[ts]['dur']:.0f}ms)" for ts in sorted_ts]

    fig, ax = plt.subplots(figsize=(12, 6.5), facecolor="#0f172a")
    ax.set_facecolor("#1e293b")

    for b_id, style in BOT_STYLE.items():
        y_vals = [runs[ts]["bots"].get(b_id, float("nan")) for ts in sorted_ts]
        ax.plot(
            range(len(sorted_ts)),
            y_vals,
            color=style["color"],
            marker=style["marker"],
            linewidth=2.2,
            markersize=7,
            label=style["label"]
        )

    ax.axhline(0, color="#94a3b8", linestyle="--", linewidth=1.5, alpha=0.8)
    ax.set_xticks(range(len(sorted_ts)))
    ax.set_xticklabels(x_labels, color="#94a3b8", fontsize=9)
    ax.tick_params(colors="#94a3b8")
    ax.set_ylabel("Profit Rate ($/ms)", color="#cbd5e1", fontsize=11, fontweight="bold")
    ax.set_title("Historical Profit Rate per Millisecond ($/ms) Across Simulation Runs", color="#f8fafc", fontsize=14, fontweight="bold", pad=14)
    ax.grid(True, color="#334155", linestyle=":", alpha=0.7)

    sec_ax = ax.secondary_yaxis("right", functions=(lambda x: x * 1000.0, lambda x: x / 1000.0))
    sec_ax.set_ylabel("Normalized Profit Rate ($/sec)", color="#cbd5e1", fontsize=11, fontweight="bold")
    sec_ax.tick_params(colors="#94a3b8")

    for spine in ax.spines.values():
        spine.set_color("#475569")
    for spine in sec_ax.spines.values():
        spine.set_color("#475569")

    ax.legend(facecolor="#0f172a", edgecolor="#475569", labelcolor="#f1f5f9", loc="upper left")
    plt.tight_layout()
    plt.savefig(args.out, dpi=200, facecolor=fig.get_facecolor())
    plt.close()
    print(f"Generated plot: {args.out}")

if __name__ == "__main__":
    main()
