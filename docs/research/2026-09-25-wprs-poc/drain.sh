#!/usr/bin/env bash
# drain.sh LABEL: set remote mode idle, unthrottle, wait until downstream < 50 KB/s, report backlog.
P=/home/christoph/Projects/kabelsalat/.poc
OUT=$P/logs/phase-$1.txt
ssh -o BatchMode=yes -S $P/run/cm.sock dev 'echo idle > ~/.local/share/kabelsalat-poc/run/mode'
echo 0 > $P/run/rate-dev
T0=$(date +%s.%N); sleep 3; q=0
while :; do sleep 1; b=$(tail -1 $P/logs/relay-dev.csv | cut -d, -f2); if [ "$b" -lt 50000 ]; then q=$((q+1)); else q=0; fi; [ $q -ge 2 ] && break; done
T1=$(date +%s.%N)
awk -F, -v a=$T0 -v b=$T1 '$1>=a && $1<=b {d+=$2} END {printf "drain: %.1f MB backlog delivered in %.0f s after unthrottle\n", d/1e6, b-a}' $P/logs/relay-dev.csv | tee -a $OUT
ssh -o BatchMode=yes -S $P/run/cm.sock dev 'tail -1 ~/.local/share/kabelsalat-poc/logs/rss.log' | tee -a $OUT
