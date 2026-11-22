#!/usr/bin/env bash
set -euo pipefail
task_host=/tank/shared/projects/shenko/build/ui-corrected-ef2256e1
exec > >(tee "$task_host/host.log") 2>&1
trap 'status=$?; printf "host_exit=%s time=%s\n" "$status" "$(date -u +%FT%TZ)" > "$task_host/exit.txt"' EXIT
printf 'waiting_for_cargo_lock %s pid=%s\n' "$(date -u +%FT%TZ)" "$$"
exec 9>/tank/shared/projects/shenko/build/cargo.lock
flock -x 9
printf 'cargo_lock_acquired %s\n' "$(date -u +%FT%TZ)"
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer "docker inspect --format '{{.Image}}' shenko-build-r71" > "$task_host/container-image.txt"
test "$(cat "$task_host/container-image.txt")" = sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer docker exec shenko-build-r71 bash "$task_host/container-build-web.sh"
ssh -n -T -o BatchMode=yes -o StrictHostKeyChecking=yes doescontainer docker exec shenko-build-r71 cat /var/idw-target/ui-corrected-ef2256e1/delivery.tar.gz > "$task_host/delivery.tar.gz"
sha256sum "$task_host/delivery.tar.gz"
printf 'host_delivery_ready %s\n' "$(date -u +%FT%TZ)"
