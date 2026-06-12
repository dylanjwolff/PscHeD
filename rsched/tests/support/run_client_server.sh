#!/usr/bin/env bash
set -u

runner=$1
server=$2
client=$3
ipc=$4
out=$5

"$runner" "$client" "$ipc" &
client_pid=$!
"$runner" "$server" "$ipc" >"$out" &
server_pid=$!

wait "$client_pid"
client_status=$?
wait "$server_pid"
server_status=$?
cat "$out"
exit $((client_status != 0 ? client_status : server_status))
