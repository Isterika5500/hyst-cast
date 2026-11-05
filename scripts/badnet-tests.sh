#!/usr/bin/env bash
# Самопроверки через «плохую сеть». Нужен собранный release.
set -u
S=${TMPDIR:-/tmp}/hyst-cast-badnet; mkdir -p $S/srv
B=./target/release
HYST_CAST_TOKEN=test HYST_CAST_PORT=4597 HYST_CAST_DATA=$S/srv $B/hyst-cast-server > $S/srv.log 2>&1 & SRVPID=$!
trap 'kill $SRVPID 2>/dev/null' EXIT
sleep 1
run() { # имя, сервер_стримера, сервер_зрителя, пресет, аргументы badnet...
  local name=$1 srv=$2 vsrv=$3 preset=$4; shift 4
  echo "=== $name"
  python3 scripts/badnet.py 5597 127.0.0.1:4597 "$@" > $S/badnet.log 2>&1 & local BP=$!
  sleep 0.7
  timeout 70 $B/hyst-cast --selftest --server $srv --viewer-server $vsrv --cert $S/srv/cert.der --token test --seconds 20 --preset $preset 2>$S/st.err | grep -E "стример:|зритель:|САМО"
  grep -E "НЕ ПРОЙДЕНА|не дождались" $S/st.err | tail -1
  echo "   снижений битрейта: $(grep -c 'канал не тянет' $S/st.err)"
  kill $BP 2>/dev/null; wait $BP 2>/dev/null
}
D=127.0.0.1:4597 P=127.0.0.1:5597
run "канал стримера 4 Мбит/с, 40 мс, 0.5% потерь (цель 6 Мбит/с)" $P $D 1080p30 --up-kbps 4000 --delay-ms 40 --loss 0.5
run "канал зрителя 3 Мбит/с, 60 мс (стрим 6 Мбит/с)" $D $P 1080p30 --down-kbps 3000 --delay-ms 60
run "обоим 2% потерь, 100 мс, скорость без ограничений" $P $P 1080p30 --delay-ms 100 --loss 2
