#!/bin/bash
# Monitor process CPU/memory usage during load test
# Usage: ./monitor-procs.sh <output_dir> <interval_sec> <pid1> <pid2> ...
# Runs until killed (SIGTERM/SIGINT)

OUTPUT_DIR="$1"
INTERVAL="${2:-1}"
shift 2
PIDS="$@"

mkdir -p "$OUTPUT_DIR"

# Header
echo "timestamp,pid,process,%cpu,%mem,rss_kb,vsz_kb,threads" > "$OUTPUT_DIR/process-stats.csv"

# Also log system-wide CPU
echo "timestamp,%usr,%sys,%iowait,%idle" > "$OUTPUT_DIR/system-cpu.csv"

cleanup() {
    exit 0
}
trap cleanup SIGTERM SIGINT

while true; do
    TS=$(date +%s.%N)

    # Per-process stats from /proc
    for PID in $PIDS; do
        if [ -d "/proc/$PID" ]; then
            # Get process name
            PNAME=$(cat /proc/$PID/comm 2>/dev/null || echo "?")

            # Get CPU and memory from ps
            STATS=$(ps -p $PID -o %cpu,%mem,rss,vsz,nlwp --no-headers 2>/dev/null)
            if [ -n "$STATS" ]; then
                CPU=$(echo $STATS | awk '{print $1}')
                MEM=$(echo $STATS | awk '{print $2}')
                RSS=$(echo $STATS | awk '{print $3}')
                VSZ=$(echo $STATS | awk '{print $4}')
                THR=$(echo $STATS | awk '{print $5}')
                echo "$TS,$PID,$PNAME,$CPU,$MEM,$RSS,$VSZ,$THR" >> "$OUTPUT_DIR/process-stats.csv"
            fi
        fi
    done

    # System-wide CPU from /proc/stat (compute delta)
    if [ -f /tmp/prev_stat ]; then
        read -r _ prev_user prev_nice prev_sys prev_idle prev_iowait _ < /tmp/prev_stat
        read -r _ curr_user curr_nice curr_sys curr_idle curr_iowait _ < <(head -1 /proc/stat)

        d_user=$((curr_user - prev_user + curr_nice - prev_nice))
        d_sys=$((curr_sys - prev_sys))
        d_idle=$((curr_idle - prev_idle))
        d_iowait=$((curr_iowait - prev_iowait))
        d_total=$((d_user + d_sys + d_idle + d_iowait))

        if [ $d_total -gt 0 ]; then
            pct_user=$(echo "scale=1; $d_user * 100 / $d_total" | bc)
            pct_sys=$(echo "scale=1; $d_sys * 100 / $d_total" | bc)
            pct_iowait=$(echo "scale=1; $d_iowait * 100 / $d_total" | bc)
            pct_idle=$(echo "scale=1; $d_idle * 100 / $d_total" | bc)
            echo "$TS,$pct_user,$pct_sys,$pct_iowait,$pct_idle" >> "$OUTPUT_DIR/system-cpu.csv"
        fi
    fi
    head -1 /proc/stat > /tmp/prev_stat

    sleep "$INTERVAL"
done
