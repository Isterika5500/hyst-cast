#!/usr/bin/env bash
# Открыть UDP 4433 для hyst-cast: Security List Oracle + iptables на сервере. С бэкапами.
# Запускать самому: ! bash ~/Projects/hyst-cast/deploy/open-udp-4433.sh
set -euo pipefail
export PYTHONWARNINGS=ignore SUPPRESS_LABEL_WARNING=True
SL=ocid1.securitylist.oc1.me-dubai-1.aaaaaaaa3n6iy63wuz4r7pjhjlnm6kacyvex56x5zopjsbbmtyocwlxjnsua
BK=~/.oci/backups/Default-seclist-$(date +%F-%H%M)-before-hyst-cast.json
mkdir -p ~/.oci/backups
oci network security-list get --security-list-id "$SL" > "$BK"
echo "бэкап Security List: $BK"
ETAG=$(jq -r '.etag' "$BK")
if jq -e '.data["ingress-security-rules"][] | select(.["udp-options"]["destination-port-range"].min == 4433)' "$BK" >/dev/null; then
  echo "UDP 4433 в Security List уже открыт"
else
  jq '.data["ingress-security-rules"] + [{"protocol":"17","source":"0.0.0.0/0","source-type":"CIDR_BLOCK","is-stateless":false,
        "description":"hyst-cast (QUIC)","udp-options":{"destination-port-range":{"min":4433,"max":4433}}}]
      | walk(if type == "object" then with_entries(.key |= gsub("-(?<c>[a-z])"; .c | ascii_upcase)) else . end)' "$BK" > /tmp/ingress-new.json
  echo "правил было: $(jq '.data["ingress-security-rules"] | length' "$BK"), станет: $(jq length /tmp/ingress-new.json)"
  oci network security-list update --security-list-id "$SL" --ingress-security-rules file:///tmp/ingress-new.json \
      --if-match "$ETAG" --force --query 'data."ingress-security-rules"[].{proto:protocol,tcp:"tcp-options"."destination-port-range".min,udp:"udp-options"."destination-port-range".min}' --output table
  rm -f /tmp/ingress-new.json
fi
[ -S /tmp/ssh-oracle.sock ] || ssh-agent -a /tmp/ssh-oracle.sock >/dev/null
export SSH_AUTH_SOCK=/tmp/ssh-oracle.sock
ssh-add -l >/dev/null 2>&1 || SSH_ASKPASS=$HOME/.local/bin/zenity-askpass SSH_ASKPASS_REQUIRE=force ssh-add ~/.ssh/id_ed25519_oracle </dev/null
ssh oracle 'set -e
sudo cp -n /etc/iptables/rules.v4 /etc/iptables/rules.v4.bak-$(date +%F)-before-hyst-cast
if sudo iptables -C INPUT -p udp --dport 4433 -j ACCEPT 2>/dev/null; then echo "iptables: уже открыт"; else
  N=$(sudo iptables -L INPUT --line-numbers -n | awk "/REJECT/{print \$1; exit}")
  sudo iptables -I INPUT ${N:-5} -p udp --dport 4433 -m comment --comment hyst-cast -j ACCEPT
  sudo netfilter-persistent save >/dev/null 2>&1 && echo "iptables: UDP 4433 открыт (позиция ${N:-5}) и сохранён"
fi
sudo iptables -L INPUT -n --line-numbers | grep -E "4433|REJECT"'
ssh-add -D >/dev/null
echo "готово"
