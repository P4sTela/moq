#!/bin/bash
# Run load test with process monitoring
# Usage: ./run-with-monitor.sh <results_dir> <loadtest_args...>

RESULTS_DIR="$1"
shift

echo "=== Starting load test with monitoring ==="
echo "Results: $RESULTS_DIR"

# Start the load test in background, capture its PID
cargo run --release -p moq-loadtest -- "$@" --results-dir "$RESULTS_DIR" &
LOADTEST_PID=$!

# Wait a moment for relay processes to start
sleep 3

# Find all relevant PIDs: moq-relay and moq-loadtest processes
RELAY_PIDS=$(pgrep -f "moq-relay" | tr '\n' ' ')
echo "Loadtest PID: $LOADTEST_PID"
echo "Relay PIDs: $RELAY_PIDS"

ALL_PIDS="$LOADTEST_PID $RELAY_PIDS"

# Start monitor in background
scripts/monitor-procs.sh "$RESULTS_DIR" 1 $ALL_PIDS &
MONITOR_PID=$!

# Also run pidstat for detailed per-second CPU breakdown
pidstat -p ALL -u -r 1 > "$RESULTS_DIR/pidstat.log" 2>&1 &
PIDSTAT_PID=$!

# Wait for load test to finish
wait $LOADTEST_PID
LOADTEST_EXIT=$?

# Stop monitor
kill $MONITOR_PID 2>/dev/null
kill $PIDSTAT_PID 2>/dev/null
wait $MONITOR_PID 2>/dev/null
wait $PIDSTAT_PID 2>/dev/null

# Generate summary
echo ""
echo "=== Process Resource Summary ==="
python3 << 'PYEOF'
import csv, sys, os
from collections import defaultdict

results_dir = sys.argv[1] if len(sys.argv) > 1 else "."
csv_path = os.path.join(results_dir, "process-stats.csv")
sys_path = os.path.join(results_dir, "system-cpu.csv")

if not os.path.exists(csv_path):
    print("No process-stats.csv found")
    sys.exit(0)

# Process stats
proc_data = defaultdict(lambda: {"cpu": [], "mem": [], "rss": [], "threads": []})

with open(csv_path) as f:
    reader = csv.DictReader(f)
    for row in reader:
        key = f"{row['process']}(pid={row['pid']})"
        try:
            proc_data[key]["cpu"].append(float(row["%cpu"]))
            proc_data[key]["mem"].append(float(row["%mem"]))
            proc_data[key]["rss"].append(int(row["rss_kb"]))
            proc_data[key]["threads"].append(int(row["threads"]))
        except (ValueError, KeyError):
            pass

print(f"\n{'Process':<35s} | {'Avg CPU%':>9s} | {'Max CPU%':>9s} | {'Avg RSS MB':>10s} | {'Max RSS MB':>10s} | {'Threads':>7s}")
print("-" * 35 + "-|-" + "-" * 9 + "-|-" + "-" * 9 + "-|-" + "-" * 10 + "-|-" + "-" * 10 + "-|-" + "-" * 7)

for name in sorted(proc_data.keys()):
    d = proc_data[name]
    if len(d["cpu"]) < 3:
        continue
    # Skip first 2 samples (startup)
    cpu = d["cpu"][2:]
    rss = d["rss"][2:]
    threads = d["threads"][2:]
    if not cpu:
        continue
    avg_cpu = sum(cpu) / len(cpu)
    max_cpu = max(cpu)
    avg_rss = sum(rss) / len(rss) / 1024
    max_rss = max(rss) / 1024
    avg_thr = sum(threads) / len(threads)
    print(f"{name:<35s} | {avg_cpu:9.1f} | {max_cpu:9.1f} | {avg_rss:10.1f} | {max_rss:10.1f} | {avg_thr:7.0f}")

# System CPU
if os.path.exists(sys_path):
    print(f"\nSystem-wide CPU (32 cores):")
    with open(sys_path) as f:
        reader = csv.DictReader(f)
        rows = list(reader)
    if len(rows) > 2:
        rows = rows[2:]  # skip startup
        usr = [float(r["%usr"]) for r in rows]
        sys_ = [float(r["%sys"]) for r in rows]
        idle = [float(r["%idle"]) for r in rows]
        print(f"  Avg user: {sum(usr)/len(usr):.1f}%  sys: {sum(sys_)/len(sys_):.1f}%  idle: {sum(idle)/len(idle):.1f}%")
        print(f"  Max user: {max(usr):.1f}%  sys: {max(sys_):.1f}%")
PYEOF
"$RESULTS_DIR"

exit $LOADTEST_EXIT
