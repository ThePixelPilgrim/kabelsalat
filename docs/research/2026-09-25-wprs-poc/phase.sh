#!/usr/bin/env bash
# phase.sh LABEL RATE_BPS MODE SECONDS PROBES
# Sets relay rate + remote animbox mode, lets it run, samples ssh-channel exec latency
# over the forward's own ControlMaster connection, then runs latency probes.
# Writes everything to logs/phase-LABEL.txt.
set -u
P=/home/christoph/Projects/kabelsalat/.poc
LABEL=$1 RATE=$2 MODE=$3 SECS=$4 PROBES=$5
OUT=$P/logs/phase-$LABEL.txt
CTL=$(grep control= $P/run/kst-info-dev | cut -d= -f2)
R=/home/christoph/.local/share/kabelsalat-poc
SSHCM="ssh -o BatchMode=yes -S $P/run/cm.sock dev"
echo "$RATE" > $P/run/rate-dev
$SSHCM "echo $MODE > $R/run/mode"
T0=$(date +%s.%N)
echo "phase $LABEL rate=$RATE mode=$MODE start=$T0" > $OUT
end=$(( $(date +%s) + SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  s=$(date +%s.%N); timeout 60 $SSHCM true; e=$(date +%s.%N)
  echo "sshexec $(echo "$e - $s" | bc)" >> $OUT
  sleep 2
done
python3 $P/lat.py "$CTL" "$PROBES" 60 >> $OUT 2>&1
T1=$(date +%s.%N)
echo "end=$T1" >> $OUT
$SSHCM "awk -v a=$T0 -v b=$T1 '\$1>=a && \$1<=b' $R/logs/rss.log | sed -n '1p;\$p'; grep STAT $R/logs/animbox.log | awk -v a=$T0 -v b=$T1 '\$2/1000>=a && \$2/1000<=b {print \$3}' | tr '\n' ' '" >> $OUT
awk -F, -v a=$T0 -v b=$T1 '$1>=a && $1<=b {d+=$2; u+=$3; n++; if ($2>m) m=$2} END {printf "relay: %d s, down avg %.0f B/s (%.2f Mbit/s), peak %d B/s, up avg %.0f B/s\n", n, d/n, d/n*8/1e6, m, u/n}' $P/logs/relay-dev.csv >> $OUT
cat $OUT
