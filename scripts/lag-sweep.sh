#!/bin/zsh
# Mirror with several settings; screenshot the lag wheel next to a webcam view of the TV (e.g. Photo Booth).
# Usage: scripts/lag-sweep.sh DEVICE [OUTDIR [CONFIG...]]   (~25 s each; CONFIG = "name --flags...")
set -u
dev=${1:?device, e.g. 192.168.177.56}
out=${2:-lag-sweep-$(date +%Y%m%d-%H%M%S)}
mkdir -p $out
bin=${0:A:h:h}/target/release/cast-mirror

configs=("${@:3}")
(( $#configs )) || configs=(
  "d200       --delay 200"
  "d150       --delay 150"
  "d100       --delay 100"
  "d100-fps60 --delay 100 --fps 60"
  "d100-main  --delay 100 --codecs h264"
  "d100-1280  --delay 100 --width 1280"
)

for c in $configs; do
  name=${c%% *}; args=(${=c#* })
  echo "== $name: $args"
  say -r 200 "${name//-/ }" &
  $bin $dev --stats $args > $out/$name.log 2>&1 &
  pid=$!
  sleep 10
  for i in 1 2 3 4 5 6; do
    screencapture -x -m $out/$name-$i.png
    ps -o %cpu= -p $pid >> $out/$name.cpu
    sleep 1
  done
  kill -INT $pid
  for i in {1..20}; do kill -0 $pid 2>/dev/null || break; sleep 0.5; done
  if kill -0 $pid 2>/dev/null; then kill -KILL $pid; $bin stop $dev; fi
  grep "^fps" $out/$name.log | tail -1
  sleep 3
done
echo "Screenshots in $out"
say -r 200 "Sweep finished"
