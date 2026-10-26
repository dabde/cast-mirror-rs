#!/bin/zsh
# Record a Chrome "Cast screen" session to a receiver and summarise it.
# Once: brew install --cask wireshark-chmodbpf  (admin password; gives non-root capture access)
# Usage: scripts/capture-chrome.sh RECEIVER_IP [SECONDS]  (your normal Chrome can stay open)
set -eu
ip=${1:?receiver IP, e.g. 192.168.1.50}
secs=${2:-60}
out=${OUT:-capture-$(date +%Y%m%d-%H%M%S)}
mkdir -p $out
out=${out:A}
keys=$out/sslkeys.log

# A separate profile starts a fresh Chrome process that writes TLS keys, so 8009 can be decrypted.
open -na "Google Chrome" --env SSLKEYLOGFILE=$keys --args --user-data-dir=$out/profile --no-first-run
echo "Chrome started. Within ${secs}s: ⋮ → Cast… → Sources → Cast screen → pick the receiver."

tshark -i en0 -f "host $ip" -a duration:$secs -w $out/session.pcapng
echo "Saved $out/session.pcapng"

# Wireshark guesses NVMe/TCP for port 8009; force TLS so the keys get used.
ts=(tshark -r $out/session.pcapng -o tls.keylog_file:$keys -d tcp.port==8009,tls --disable-protocol nvme-tcp)
echo "\n== UDP conversations (RTP + RTCP share one port pair)"
$ts -q -z conv,udp | grep -v ":5353\|:1900\|:57621" || true
echo "== Cast channel messages, decrypted"
$ts -Y "tcp.port==8009" -x 2>/dev/null \
  | awk '/^Decrypted TLS/{d=1;next} /^[A-Z]/{d=0} /^$/{d=0} d' \
  | sed -E 's/^[0-9a-f]{4}  //; s/   .*$//' | tr -d ' \n' | xxd -r -p > $out/cast.bin
LC_ALL=C grep -aoE '"type": ?"[A-Z_]+"' $out/cast.bin | sort | uniq -c || echo "(nothing decrypted: is $keys empty?)"
echo "Decrypted channel bytes: $out/cast.bin (OFFER/ANSWER are plain JSON inside)"
