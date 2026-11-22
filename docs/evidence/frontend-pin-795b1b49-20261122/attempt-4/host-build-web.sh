#!/usr/bin/env bash
set -euo pipefail
task_host=/tank/shared/projects/shenko/build/daily-web-pin-795b1b49-r4
task_source=/tank/shared/projects/shenko/worktrees/shenko-web-pin-795b1b49
task_upstream=/tank/shared/projects/shenko/worktrees/proto-ui-web-pin-795b1b49
exec > >(tee "$task_host/host.log") 2>&1
trap 'status=$?; printf "host_exit=%s time=%s\n" "$status" "$(date -u +%FT%TZ)" > "$task_host/exit.txt"' EXIT
printf 'waiting_for_cargo_lock %s pid=%s\n' "$(date -u +%FT%TZ)" "$$"
exec 9>/tank/shared/projects/shenko/build/cargo.lock
flock -x 9
printf 'cargo_lock_acquired %s\n' "$(date -u +%FT%TZ)"
test -d "$task_source"
test -d "$task_upstream"
# Sources stay read-only to the container; it creates its own persistent Git
# checkouts in the private writable volume under the same resource admission.
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer "docker inspect --format '{{.Image}}' shenko-build-r71" > "$task_host/container-image.txt"
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer docker exec shenko-build-r71 bash "$task_host/container-build-web.sh"
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer docker exec shenko-build-r71 cat /var/idw-target/daily-web-pin-795b1b49-r4/delivery.tar.gz > "$task_host/delivery.tar.gz"
sha256sum "$task_host/delivery.tar.gz"
printf 'host_delivery_ready %s\n' "$(date -u +%FT%TZ)"
