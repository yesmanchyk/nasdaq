#!/usr/bin/env python3
"""Plot historical profit per millisecond ($/ms and $/s) for each bot from runs.csv.

Supports:
- Generating standalone SVG chart (default, zero external dependencies)
- Generating interactive HTML chart with Chart.js
- Generating PNG/PDF chart if matplotlib is available
- Printing terminal ASCII summary table and stats

Usage:
    python3 plot_profit_per_ms.py [--csv PATH] [--out OUTPUT_PATH] [--html] [--ascii]
"""

import argparse
import csv
from datetime import datetime
import os
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
DEFAULT_CSV = os.path.normpath(os.path.join(SCRIPT_DIR, "..", "sim_history", "runs.csv"))
DEFAULT_OUT = os.path.normpath(os.path.join(SCRIPT_DIR, "..", "sim_history", "profit_per_ms.svg"))

BOT_PALETTE = {
    1: {"color": "#ef4444", "light": "#f87171", "label": "Bot-1 (MM + MeanRev)"},
    2: {"color": "#10b981", "light": "#34d399", "label": "Bot-2 (Mom + Taker)"},
    3: {"color": "#3b82f6", "light": "#60a5fa", "label": "Bot-3 (Smart AS+OBI)"},
    4: {"color": "#a855f7", "light": "#c084fc", "label": "Bot-4 (RL Q+Bandit)"},
    5: {"color": "#f59e0b", "light": "#fbbf24", "label": "Bot-5 (C++ LowLat)"},
}

FALLBACK_COLORS = ["#f59e0b", "#ec4899", "#14b8a6", "#6366f1"]


def load_data(csv_path):
    if not os.path.exists(csv_path):
        print(f"Error: CSV file not found at: {csv_path}", file=sys.stderr)
        sys.exit(1)

    runs = {}  # timestamp -> { 'meta': ..., 'bots': {bot_id: row} }
    with open(csv_path, mode="r", encoding="utf-8") as f:
        reader = csv.DictReader(f)
        for row in reader:
            ts = int(row["timestamp_unix_s"])
            bot_id = int(row["bot_id"])
            dur_ms = float(row["duration_ms"])
            pnl = float(row["mtm_pnl_dollars"])
            profit_per_ms = pnl / dur_ms if dur_ms > 0 else 0.0
            profit_per_sec = profit_per_ms * 1000.0

            entry = {
                "bot_id": bot_id,
                "bot_name": row["bot_name"],
                "source": row["source"],
                "duration_ms": dur_ms,
                "mtm_pnl": pnl,
                "equity": float(row["equity_dollars"]),
                "cash": float(row["cash_dollars"]),
                "position": int(row["position_shares"]),
                "fills": int(row["fills"]),
                "profit_per_ms": profit_per_ms,
                "profit_per_sec": profit_per_sec,
            }

            if ts not in runs:
                runs[ts] = {
                    "timestamp": ts,
                    "datetime": datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M:%S"),
                    "duration_ms": dur_ms,
                    "sources": set(),
                    "ohlc": None,
                    "bots": {},
                }
            runs[ts]["sources"].add(row["source"])
            if row["source"] != "cpp_bot":
                runs[ts]["duration_ms"] = dur_ms
            if runs[ts]["ohlc"] is None:
                o_s = (row.get("open_price") or "").strip()
                h_s = (row.get("high_price") or "").strip()
                l_s = (row.get("low_price") or "").strip()
                c_s = (row.get("close_price") or "").strip()
                if o_s and h_s and l_s and c_s:
                    runs[ts]["ohlc"] = (float(o_s), float(h_s), float(l_s), float(c_s))
            runs[ts]["bots"][bot_id] = entry

    # Normalize combined run sources (e.g., sim + cpp_bot -> "sim+cpp")
    for run in runs.values():
        srcs = run.pop("sources")
        if "sim" in srcs and "cpp_bot" in srcs:
            run["source"] = "sim+cpp"
        elif "sim" in srcs:
            run["source"] = "sim"
        elif "gui_builtin" in srcs:
            run["source"] = "gui_builtin"
        else:
            run["source"] = "+".join(sorted(srcs))

    # Sort runs chronologically
    sorted_runs = [runs[k] for k in sorted(runs.keys())]
    return sorted_runs


def render_ascii_table(runs):
    bot_ids = sorted({b for r in runs for b in r["bots"]}) or [1, 2, 3, 4, 5]
    has_ohlc = any(r.get("ohlc") is not None for r in runs)
    col_w = 22
    ohlc_w = 37
    header_parts = [f"{'Run Date/Time':<20}", f"{'Source':<11}", f"{'Dur(ms)':<8}"]
    if has_ohlc:
        header_parts.append(f"{'OHLC (O / H / L / C)':<{ohlc_w}}")
    for b_id in bot_ids:
        header_parts.append(f"{f'Bot-{b_id} $/ms':<{col_w}}")
    header = " | ".join(header_parts)
    sep_w = max(108, len(header))

    print("=" * sep_w)
    print("  NASDAQ ITCH-OUCH HISTORICAL PERFORMANCE: PROFIT PER MILLISECOND ($/ms & $/s)")
    print("=" * sep_w)
    print(header)
    print("-" * sep_w)

    bot_rates = {b: [] for b in bot_ids}

    for run in runs:
        cols = [f"{run['datetime']:<20}", f"{run['source']:<11}", f"{run['duration_ms']:<8.0f}"]
        if has_ohlc:
            ohlc = run.get("ohlc")
            if ohlc is not None:
                o, h, l, c = ohlc
                ohlc_cell = f"{o:7.4f}/{h:7.4f}/{l:7.4f}/{c:7.4f}"
                cols.append(f"{ohlc_cell:<{ohlc_w}}")
            else:
                cols.append(f"{'N/A':^{ohlc_w}}")
        for b_id in bot_ids:
            if b_id in run["bots"]:
                rate_ms = run["bots"][b_id]["profit_per_ms"]
                rate_sec = run["bots"][b_id]["profit_per_sec"]
                bot_rates[b_id].append(rate_ms)
                sign = "+" if rate_ms >= 0 else ""
                cell = f"{sign}{rate_ms:8.5f} (${rate_sec:+.2f}/s)"
                cols.append(f"{cell:<{col_w}}")
            else:
                cols.append(f"{'N/A':^{col_w}}")
        print(" | ".join(cols))

    print("-" * sep_w)
    print("AVERAGE PROFIT RATE ACROSS ALL RUNS:")
    for b_id in bot_ids:
        rates = bot_rates[b_id]
        if rates:
            avg_ms = sum(rates) / len(rates)
            avg_sec = avg_ms * 1000.0
            name = BOT_PALETTE.get(b_id, {}).get("label", f"Bot {b_id}")
            sign = "+" if avg_ms >= 0 else ""
            print(f"  • {name:<26}: {sign}{avg_ms:8.5f} $/ms  ({sign}{avg_sec:7.2f} $/s)")
    print("=" * sep_w)


def render_svg(runs, out_path):
    """Render a standalone, high-resolution SVG chart with dark theme styling."""
    width = 1100
    height = 680
    margin = {"top": 80, "right": 220, "bottom": 120, "left": 100}

    plot_w = width - margin["left"] - margin["right"]
    plot_h = height - margin["top"] - margin["bottom"]

    # Collect distinct bots and values
    bot_names = {}
    all_values = []
    for run in runs:
        for b_id, b_data in run["bots"].items():
            bot_names[b_id] = b_data["bot_name"]
            all_values.append(b_data["profit_per_ms"])

    if not all_values:
        return

    min_val = min(all_values)
    max_val = max(all_values)
    # Add symmetric or pleasant padding
    pad = (max_val - min_val) * 0.12 if max_val != min_val else 0.05
    y_min = min(0.0, min_val - pad)
    y_max = max(0.0, max_val + pad)

    def y_coord(val):
        pct = (val - y_min) / (y_max - y_min)
        return margin["top"] + plot_h - (pct * plot_h)

    def x_coord(i, n):
        if n <= 1:
            return margin["left"] + plot_w / 2
        return margin["left"] + (i / (n - 1)) * plot_w

    n_runs = len(runs)
    y_zero = y_coord(0.0)

    svg = []
    svg.append(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width} {height}" width="{width}" height="{height}">')
    svg.append("""<style>
      .bg { fill: #0f172a; }
      .grid { stroke: #334155; stroke-dasharray: 4,4; stroke-width: 1; }
      .axis { stroke: #64748b; stroke-width: 1.5; }
      .zero-line { stroke: #94a3b8; stroke-width: 2; stroke-dasharray: 6,4; }
      .text-title { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; font-size: 20px; font-weight: bold; fill: #f8fafc; }
      .text-sub { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; font-size: 13px; fill: #94a3b8; }
      .text-axis { font-family: monospace; font-size: 11px; fill: #94a3b8; }
      .text-legend { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; font-size: 12px; font-weight: 500; fill: #e2e8f0; }
      .bar:hover, .point:hover { filter: drop-shadow(0 0 6px white); cursor: pointer; }
    </style>""")

    # Background
    svg.append(f'<rect width="{width}" height="{height}" class="bg" rx="12"/>')

    # Header title
    svg.append(f'<text x="{margin["left"]}" y="36" class="text-title">NASDAQ ITCH-OUCH: Historical Profit Rate per Millisecond ($/ms)</text>')
    svg.append(f'<text x="{margin["left"]}" y="56" class="text-sub">Source: {DEFAULT_CSV} ({n_runs} recorded simulation runs)</text>')

    # Grid lines and Y ticks
    n_ticks = 8
    for i in range(n_ticks + 1):
        v = y_min + (i / n_ticks) * (y_max - y_min)
        y = y_coord(v)
        svg.append(f'<line x1="{margin["left"]}" y1="{y}" x2="{margin["left"] + plot_w}" y2="{y}" class="grid" />')
        sec_v = v * 1000.0
        svg.append(f'<text x="{margin["left"] - 12}" y="{y + 4}" text-anchor="end" class="text-axis">{v:+.3f} $/ms ({sec_v:+.1f} $/s)</text>')

    # Zero line
    svg.append(f'<line x1="{margin["left"]}" y1="{y_zero}" x2="{margin["left"] + plot_w}" y2="{y_zero}" class="zero-line" />')

    # Grouped bars per run
    sorted_bot_ids = sorted(bot_names.keys())
    k_bots = len(sorted_bot_ids)
    group_width = (plot_w / n_runs) * 0.75
    bar_width = max(3.0, group_width / k_bots)

    for i, run in enumerate(runs):
        center_x = x_coord(i, n_runs)
        start_x = center_x - (group_width / 2)

        # X-axis label
        dt_label = run["datetime"].split(" ")[1]  # HH:MM:SS
        dur_label = f"{run['duration_ms']:.0f}ms"
        svg.append(f'<text x="{center_x}" y="{margin["top"] + plot_h + 24}" text-anchor="middle" class="text-axis">{dt_label}</text>')
        svg.append(f'<text x="{center_x}" y="{margin["top"] + plot_h + 40}" text-anchor="middle" class="text-axis" fill="#64748b">[{dur_label}]</text>')

        for b_idx, b_id in enumerate(sorted_bot_ids):
            if b_id in run["bots"]:
                val = run["bots"][b_id]["profit_per_ms"]
                bar_x = start_x + (b_idx * bar_width)
                bar_y = y_coord(max(0.0, val))
                bar_h = abs(y_coord(val) - y_zero)
                color = BOT_PALETTE.get(b_id, {}).get("color", FALLBACK_COLORS[b_idx % len(FALLBACK_COLORS)])

                pnl = run["bots"][b_id]["mtm_pnl"]
                title_tooltip = (
                    f"{run['bots'][b_id]['bot_name']}\n"
                    f"Rate: {val:+.5f} $/ms ({val*1000:+.2f} $/s)\n"
                    f"Total PnL: ${pnl:+.2f}\n"
                    f"Duration: {run['duration_ms']:.0f} ms\n"
                    f"Time: {run['datetime']}"
                )
                svg.append(
                    f'<rect class="bar" x="{bar_x:.1f}" y="{bar_y:.1f}" width="{bar_width*0.85:.1f}" height="{max(1.0, bar_h):.1f}" '
                    f'fill="{color}" rx="2">'
                    f'<title>{title_tooltip}</title></rect>'
                )

    # Line overlay for trend per bot
    for b_idx, b_id in enumerate(sorted_bot_ids):
        color = BOT_PALETTE.get(b_id, {}).get("color", FALLBACK_COLORS[b_idx % len(FALLBACK_COLORS)])
        points = []
        for i, run in enumerate(runs):
            if b_id in run["bots"]:
                cx = x_coord(i, n_runs)
                cy = y_coord(run["bots"][b_id]["profit_per_ms"])
                points.append((cx, cy))

        if len(points) > 1:
            pts_str = " ".join(f"{x:.1f},{y:.1f}" for x, y in points)
            svg.append(f'<polyline points="{pts_str}" fill="none" stroke="{color}" stroke-width="2" opacity="0.8" />')

        for cx, cy in points:
            svg.append(f'<circle class="point" cx="{cx:.1f}" cy="{cy:.1f}" r="3.5" fill="{color}" stroke="#0f172a" stroke-width="1.5"/>')

    # Legend on right side
    leg_x = margin["left"] + plot_w + 30
    leg_y = margin["top"] + 20
    svg.append(f'<text x="{leg_x}" y="{leg_y}" class="text-title" font-size="14">Bots Legend</text>')

    for idx, b_id in enumerate(sorted_bot_ids):
        curr_y = leg_y + 30 + (idx * 55)
        color = BOT_PALETTE.get(b_id, {}).get("color", FALLBACK_COLORS[idx % len(FALLBACK_COLORS)])
        name = BOT_PALETTE.get(b_id, {}).get("label", bot_names.get(b_id, f"Bot {b_id}"))

        # Calculate average $/ms
        rates = [r["bots"][b_id]["profit_per_ms"] for r in runs if b_id in r["bots"]]
        avg_rate = sum(rates) / len(rates) if rates else 0.0

        svg.append(f'<rect x="{leg_x}" y="{curr_y}" width="16" height="16" fill="{color}" rx="3"/>')
        svg.append(f'<text x="{leg_x + 24}" y="{curr_y + 13}" class="text-legend">{name}</text>')
        svg.append(f'<text x="{leg_x + 24}" y="{curr_y + 27}" class="text-axis" fill="{color}">Avg: {avg_rate:+.4f} $/ms ({avg_rate*1000:+.1f} $/s)</text>')

    # Axis border line
    svg.append(f'<line x1="{margin["left"]}" y1="{margin["top"] + plot_h}" x2="{margin["left"] + plot_w}" y2="{margin["top"] + plot_h}" class="axis" />')

    # Footer note
    svg.append(f'<text x="{margin["left"]}" y="{height - 25}" class="text-sub">X-Axis: Simulation Runs in chronological sequence | Profit Rate = MTM PnL ($) / Duration (ms)</text>')

    svg.append("</svg>")

    with open(out_path, "w", encoding="utf-8") as f:
        f.write("\n".join(svg))
    print(f"-> Generated standalone SVG plot: {out_path}")


def render_html(runs, out_path):
    """Render an interactive HTML dashboard using Chart.js."""
    labels = [f"Run {r['datetime'].split(' ')[1]} ({r['duration_ms']:.0f}ms)" for r in runs]
    bot_ids = sorted({b for r in runs for b in r["bots"]})

    datasets = []
    for b_id in bot_ids:
        color = BOT_PALETTE.get(b_id, {}).get("color", "#64748b")
        name = BOT_PALETTE.get(b_id, {}).get("label", f"Bot {b_id}")
        data = [r["bots"].get(b_id, {}).get("profit_per_ms", None) for r in runs]
        datasets.append({
            "label": name,
            "data": data,
            "borderColor": color,
            "backgroundColor": color + "33",
            "borderWidth": 2,
            "tension": 0.2,
            "pointRadius": 5,
        })

    import json
    chart_config = {
        "type": "line",
        "data": {
            "labels": labels,
            "datasets": datasets,
        },
        "options": {
            "responsive": True,
            "maintainAspectRatio": False,
            "plugins": {
                "title": {
                    "display": True,
                    "text": "NASDAQ ITCH-OUCH: Historical Profit Rate per Millisecond ($/ms)",
                    "color": "#f8fafc",
                    "font": {"size": 18, "weight": "bold"},
                },
                "subtitle": {
                    "display": True,
                    "text": "Computed as mtm_pnl_dollars / duration_ms for each completed run",
                    "color": "#94a3b8",
                },
                "legend": {
                    "labels": {"color": "#e2e8f0"}
                },
                "tooltip": {
                    "callbacks": {}
                }
            },
            "scales": {
                "x": {
                    "ticks": {"color": "#94a3b8"},
                    "grid": {"color": "#334155"}
                },
                "y": {
                    "title": {"display": True, "text": "Profit Rate ($/ms)", "color": "#94a3b8"},
                    "ticks": {"color": "#94a3b8"},
                    "grid": {"color": "#334155"}
                }
            }
        }
    }

    has_ohlc = any(r.get("ohlc") is not None for r in runs)
    ohlc_config = None
    if has_ohlc:
        ohlc_config = {
            "type": "line",
            "data": {
                "labels": labels,
                "datasets": [
                    {
                        "label": "High ($)",
                        "data": [r["ohlc"][1] if r.get("ohlc") else None for r in runs],
                        "borderColor": "#22c55e",
                        "backgroundColor": "#22c55e22",
                        "borderWidth": 1.5,
                        "borderDash": [4, 3],
                        "tension": 0.15,
                        "pointRadius": 3,
                    },
                    {
                        "label": "Low ($)",
                        "data": [r["ohlc"][2] if r.get("ohlc") else None for r in runs],
                        "borderColor": "#ef4444",
                        "backgroundColor": "#38bdf81a",
                        "borderWidth": 1.5,
                        "borderDash": [4, 3],
                        "fill": "-1",
                        "tension": 0.15,
                        "pointRadius": 3,
                    },
                    {
                        "label": "Open ($)",
                        "data": [r["ohlc"][0] if r.get("ohlc") else None for r in runs],
                        "borderColor": "#94a3b8",
                        "backgroundColor": "#94a3b833",
                        "borderWidth": 1.5,
                        "tension": 0.15,
                        "pointRadius": 4,
                    },
                    {
                        "label": "Close ($)",
                        "data": [r["ohlc"][3] if r.get("ohlc") else None for r in runs],
                        "borderColor": "#38bdf8",
                        "backgroundColor": "#38bdf833",
                        "borderWidth": 2.5,
                        "tension": 0.15,
                        "pointRadius": 5,
                    },
                ],
            },
            "options": {
                "responsive": True,
                "maintainAspectRatio": False,
                "plugins": {
                    "title": {
                        "display": True,
                        "text": "Exchange Trade Price OHLC per Run ($)",
                        "color": "#f8fafc",
                        "font": {"size": 16, "weight": "bold"},
                    },
                    "legend": {"labels": {"color": "#e2e8f0"}},
                    "tooltip": {"callbacks": {}},
                },
                "scales": {
                    "x": {"ticks": {"color": "#94a3b8"}, "grid": {"color": "#334155"}},
                    "y": {
                        "title": {"display": True, "text": "Price ($)", "color": "#94a3b8"},
                        "ticks": {"color": "#94a3b8"},
                        "grid": {"color": "#334155"},
                    },
                },
            },
        }

    ohlc_box_html = (
        '\n    <div class="chart-box" style="height: 360px; margin-top: 32px;">\n'
        '      <canvas id="ohlcCanvas"></canvas>\n'
        "    </div>"
        if has_ohlc
        else ""
    )
    ohlc_script_js = (
        f"\n    const ohlcConfig = {json.dumps(ohlc_config)};\n"
        "    ohlcConfig.options.plugins.tooltip.callbacks.label = function(context) {\n"
        "      const val = context.parsed.y;\n"
        "      return val == null ? '' : `${context.dataset.label}: $${val.toFixed(4)}`;\n"
        "    };\n"
        "    new Chart(document.getElementById('ohlcCanvas'), ohlcConfig);"
        if has_ohlc
        else ""
    )

    html = f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>ITCH-OUCH Simulation: Profit Rate per ms & OHLC</title>
  <script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
  <style>
    body {{
      background-color: #0b1120;
      color: #e2e8f0;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
      margin: 0;
      padding: 24px;
    }}
    .container {{
      max-width: 1200px;
      margin: 0 auto;
      background: #0f172a;
      border: 1px solid #1e293b;
      border-radius: 12px;
      padding: 24px;
      box-shadow: 0 10px 25px -5px rgba(0, 0, 0, 0.5);
    }}
    .chart-box {{
      position: relative;
      height: 520px;
      margin-top: 20px;
    }}
  </style>
</head>
<body>
  <div class="container">
    <div class="chart-box">
      <canvas id="chartCanvas"></canvas>
    </div>{ohlc_box_html}
  </div>
  <script>
    const config = {json.dumps(chart_config)};
    config.options.plugins.tooltip.callbacks.label = function(context) {{
      const val = context.parsed.y;
      const valSec = val * 1000;
      return `${{context.dataset.label}}: ${{val >= 0 ? '+' : ''}}${{val.toFixed(5)}} $/ms (${{valSec >= 0 ? '+' : ''}}${{valSec.toFixed(2)}} $/s)`;
    }};
    new Chart(document.getElementById('chartCanvas'), config);{ohlc_script_js}
  </script>
</body>
</html>
"""
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(html)
    print(f"-> Generated interactive HTML plot: {out_path}")


def try_matplotlib_plot(runs, out_path):
    """If matplotlib is installed, also save a PNG plot."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        return False

    labels = [r["datetime"].split(" ")[1] for r in runs]
    bot_ids = sorted({b for r in runs for b in r["bots"]})

    plt.figure(figsize=(12, 6), facecolor="#0f172a")
    ax = plt.gca()
    ax.set_facecolor("#0f172a")

    for b_id in bot_ids:
        color = BOT_PALETTE.get(b_id, {}).get("color", "#64748b")
        name = BOT_PALETTE.get(b_id, {}).get("label", f"Bot {b_id}")
        vals = [r["bots"].get(b_id, {}).get("profit_per_ms", float("nan")) for r in runs]
        ax.plot(labels, vals, marker="o", linewidth=2, color=color, label=name)

    ax.axhline(0, color="#94a3b8", linestyle="--", alpha=0.7)
    ax.set_title("NASDAQ ITCH-OUCH: Profit Rate per Millisecond ($/ms)", color="#f8fafc", fontsize=14, weight="bold")
    ax.set_xlabel("Run Timestamp", color="#94a3b8")
    ax.set_ylabel("Profit Rate ($/ms)", color="#94a3b8")
    ax.tick_params(colors="#94a3b8")
    for spine in ax.spines.values():
        spine.set_color("#334155")
    ax.grid(True, color="#334155", linestyle=":", alpha=0.6)
    ax.legend(facecolor="#1e293b", edgecolor="#334155", labelcolor="#e2e8f0")

    png_path = os.path.splitext(out_path)[0] + ".png"
    plt.tight_layout()
    try:
        plt.savefig(png_path, dpi=200, facecolor=plt.gcf().get_facecolor())
        print(f"-> Generated Matplotlib PNG plot: {png_path}")
        return True
    except Exception as e:
        print(f"Note: Could not save PNG via matplotlib ({e})")
        return False
    finally:
        plt.close()


def main():
    parser = argparse.ArgumentParser(description="Plot historical profit per millisecond for each bot from runs.csv.")
    parser.add_argument("--csv", default=DEFAULT_CSV, help=f"Path to runs.csv (default: {DEFAULT_CSV})")
    parser.add_argument("--out", default=DEFAULT_OUT, help=f"Path to output plot (default: {DEFAULT_OUT})")
    parser.add_argument("--html", action="store_true", help="Generate an interactive HTML plot (Chart.js)")
    parser.add_argument("--no-ascii", action="store_true", help="Suppress terminal ASCII table output")
    args = parser.parse_args()

    runs = load_data(args.csv)

    if not args.no_ascii:
        render_ascii_table(runs)

    render_svg(runs, args.out)

    if args.html:
        html_out = os.path.splitext(args.out)[0] + ".html"
        render_html(runs, html_out)

    # Also attempt matplotlib if available
    try_matplotlib_plot(runs, args.out)


if __name__ == "__main__":
    main()
