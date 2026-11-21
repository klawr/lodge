#!/bin/bash
# A kubectl for a "cluster" that is one docker engine: a pod is a running
# container of that name. Only what lodge uses: version, get, exec and port-forward.
echo "$*" >> "$(dirname "$0")/kubectl.log"

while [ $# -gt 0 ]; do
    case "$1" in
        --context | --namespace) shift 2 ;;
        *) break ;;
    esac
done
cmd=$1
shift

case "$cmd" in
    version) # version --client
        echo "Client Version: v0.0.0-fake"
        ;;
    get)
        if [ "$1" = pods ]; then # get pods -l SELECTOR --field-selector=… -o jsonpath=…
            docker ps --filter "label=$3" --filter status=running --format '{{.Names}}' | tr '\n' ' '
            exit 0
        fi
        # get pod NAME -o jsonpath=…
        name=$2
        state=$(docker inspect -f '{{.State.Status}}' "$name" 2>/dev/null) || {
            echo "Error from server (NotFound): pods \"$name\" not found" >&2
            exit 1
        }
        if [ "$state" = running ]; then printf Running; else printf Pending; fi
        ;;
    exec) # exec --quiet [-i|-it] POD [-c NAME] -- COMMAND…
        flags=()
        while [ $# -gt 0 ]; do
            case "$1" in
                --quiet) shift ;;
                -i) flags=(-i); shift ;;
                -it) if [ -t 0 ]; then flags=(-it); else flags=(-i); fi; shift ;;
                *) break ;;
            esac
        done
        pod=$1
        shift
        if [ "$1" = -c ]; then shift 2; fi
        [ "$1" = -- ] && shift
        exec docker exec "${flags[@]}" "$pod" "$@"
        ;;
    port-forward) # port-forward --address ADDR pod/NAME LOCAL:REMOTE
        [ "$1" = --address ] && shift 2
        pod=${1#pod/}
        local_port=${2%%:*}
        remote_port=${2##*:}
        ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$pod")
        exec python3 -c '
import socket, sys, threading
ip, local, remote = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", local))
srv.listen(8)

def pipe(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    for s in (a, b):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

while True:
    client, _ = srv.accept()
    upstream = socket.create_connection((ip, remote))
    threading.Thread(target=pipe, args=(client, upstream), daemon=True).start()
    threading.Thread(target=pipe, args=(upstream, client), daemon=True).start()
' "$ip" "$local_port" "$remote_port"
        ;;
    *)
        echo "fake kubectl: unsupported: $cmd $*" >&2
        exit 1
        ;;
esac
