#!/bin/sh
# A stand-in for the `tailscale` CLI in tests. Its state lives in the
# directory $0 sits in: status.json (what `status --json` prints), tcp/<port>
# (one Serve TCP handler as JSON each), deny (changes are refused like for
# a user who isn't the operator), nc-socket (`nc` relays to that Unix socket).
d=$(dirname "$0")
case "$1" in
status)
    cat "$d/status.json"
    ;;
serve)
    shift
    if [ "$1" = status ]; then
        printf '{"TCP":{'
        sep=''
        for f in "$d"/tcp/*; do
            [ -e "$f" ] || continue
            printf '%s"%s":%s' "$sep" "$(basename "$f")" "$(cat "$f")"
            sep=','
        done
        printf '}}\n'
        exit 0
    fi
    if [ -e "$d/deny" ]; then
        echo "must be root, or be an operator and able to run 'sudo tailscale' to serve a path or Unix socket" >&2
        exit 1
    fi
    [ "$1" = --bg ] && shift
    port=${1#--tcp=}
    if [ "$2" = off ]; then
        rm "$d/tcp/$port" 2>/dev/null || { echo "error: failed to remove TCP serve: serve config does not exist" >&2; exit 1; }
        exit 0
    fi
    mkdir -p "$d/tcp"
    printf '{"TCPForward":"%s"}' "$2" > "$d/tcp/$port"
    echo "Available within your tailnet: tcp://$port"
    ;;
nc)
    exec python3 -c '
import os, socket, sys, threading
s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
def up():
    while True:
        b = os.read(0, 65536)
        if not b:
            s.shutdown(socket.SHUT_WR)
            return
        s.sendall(b)
threading.Thread(target=up, daemon=True).start()
while True:
    b = s.recv(65536)
    if not b:
        break
    os.write(1, b)
' "$(cat "$d/nc-socket")"
    ;;
*)
    echo "fake tailscale: unsupported: $*" >&2
    exit 2
    ;;
esac
