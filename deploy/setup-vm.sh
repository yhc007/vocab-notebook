#!/usr/bin/env bash
# VM 내부에서 1회 실행 (sudo). 데이터 디스크 마운트, Rust/CoreDB/앱 빌드, 서비스 등록.
#   cd ~/vocab-notebook/deploy && sudo ./setup-vm.sh
set -euo pipefail

DATA_DEV="/dev/disk/by-id/google-vocabdata"   # provision 시 device-name=vocabdata
DATA_MNT="/var/lib/vocab"
APP_USER="vocab"
REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"   # ~/vocab-notebook

echo ">> 1. 패키지 설치"
apt-get update
apt-get install -y build-essential pkg-config libssl-dev git curl debian-keyring \
  debian-archive-keyring apt-transport-https

echo ">> 2. 데이터 디스크 마운트 (최초 1회 포맷)"
if ! blkid "$DATA_DEV" >/dev/null 2>&1; then
  mkfs.ext4 -m 0 -F "$DATA_DEV"
fi
mkdir -p "$DATA_MNT"
grep -q "$DATA_MNT" /etc/fstab || \
  echo "$DATA_DEV $DATA_MNT ext4 discard,defaults,nofail 0 2" >> /etc/fstab
mount -a
mkdir -p "$DATA_MNT/coredb/data" "$DATA_MNT/coredb/commitlog" "$DATA_MNT/backups"

echo ">> 3. 앱 전용 사용자 생성"
id "$APP_USER" >/dev/null 2>&1 || useradd --system --shell /usr/sbin/nologin "$APP_USER"
chown -R "$APP_USER":"$APP_USER" "$DATA_MNT"

echo ">> 4. Rust 설치 (rustup, 시스템 전역)"
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable --profile minimal
  source "$HOME/.cargo/env"
fi
export PATH="$HOME/.cargo/bin:$PATH"

echo ">> 5. CoreDB 빌드"
if [ ! -d /opt/coredb ]; then
  git clone https://github.com/yhc007/coredb /opt/coredb
fi
( cd /opt/coredb && cargo build --release )
install -m755 /opt/coredb/target/release/coredb /usr/local/bin/coredb 2>/dev/null || \
  echo "   (바이너리 이름이 다르면 /opt/coredb/target/release/ 확인 후 경로 조정)"

echo ">> 6. 앱 빌드"
( cd "$REPO_DIR" && cargo build --release )
install -m755 "$REPO_DIR/target/release/vocab-notebook" /usr/local/bin/vocab-notebook
mkdir -p /opt/vocab-notebook
cp -r "$REPO_DIR/static" /opt/vocab-notebook/

echo ">> 7. systemd 유닛 설치"
cp "$REPO_DIR/deploy/coredb.service" /etc/systemd/system/
cp "$REPO_DIR/deploy/vocab-notebook.service" /etc/systemd/system/

echo ">> 8. 환경변수 파일 (비밀값은 여기서 직접 채울 것)"
if [ ! -f /etc/vocab-notebook.env ]; then
  cp "$REPO_DIR/.env.example" /etc/vocab-notebook.env
  chmod 600 /etc/vocab-notebook.env
  echo "   !! /etc/vocab-notebook.env 를 편집해 ANTHROPIC_API_KEY 등을 채우세요."
fi

echo ">> 9. 백업 cron (매일 03:00)"
cp "$REPO_DIR/deploy/backup-coredb.sh" /usr/local/bin/backup-coredb.sh
chmod +x /usr/local/bin/backup-coredb.sh
echo "0 3 * * * $APP_USER /usr/local/bin/backup-coredb.sh >> /var/log/vocab-backup.log 2>&1" \
  > /etc/cron.d/vocab-backup

echo ">> 10. Caddy(HTTPS 리버스 프록시) 설치"
if ! command -v caddy >/dev/null 2>&1; then
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | \
    gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update && apt-get install -y caddy
fi
cp "$REPO_DIR/deploy/Caddyfile" /etc/caddy/Caddyfile
echo "   !! /etc/caddy/Caddyfile 의 도메인을 실제 값으로 수정하세요 (또는 :80 사용)."

echo ">> 11. 서비스 기동"
systemctl daemon-reload
systemctl enable --now coredb.service
sleep 3
systemctl enable --now vocab-notebook.service
systemctl reload caddy || systemctl restart caddy

echo
echo ">> 완료. 상태 확인:"
echo "   systemctl status coredb vocab-notebook caddy"
echo "   journalctl -u vocab-notebook -f"
