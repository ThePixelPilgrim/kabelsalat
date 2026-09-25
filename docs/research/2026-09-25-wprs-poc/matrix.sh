#!/usr/bin/env bash
P=/home/christoph/Projects/kabelsalat/.poc
run() { $P/phase.sh "$@" > /dev/null; $P/drain.sh "$1" > /dev/null; }
run t10-idle 10000000 idle 30 3
run t10-plasma 10000000 plasma 30 3
run t10-scroll 10000000 scroll 30 3
run t10-noise 10000000 noise 20 2
run t1-idle 1000000 idle 30 3
run t1-plasma 1000000 plasma 30 3
run t1-scroll 1000000 scroll 30 3
echo MATRIX DONE
