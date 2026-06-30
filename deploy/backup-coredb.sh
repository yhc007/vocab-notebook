#!/usr/bin/env bash
# CoreDB 데이터 디렉토리를 영구 디스크 내 backups/ 로 스냅샷(tar.gz). 14일 보관.
# CoreDB 자체 백업 API가 안정화되면 그쪽으로 교체 가능.
set -euo pipefail

SRC="/var/lib/vocab/coredb"
DEST="/var/lib/vocab/backups"
STAMP="$(date +%Y%m%d-%H%M%S)"
mkdir -p "$DEST"

tar -czf "$DEST/coredb-$STAMP.tar.gz" -C "$SRC" data commitlog
echo "backup created: $DEST/coredb-$STAMP.tar.gz"

# 14일 지난 백업 삭제
find "$DEST" -name 'coredb-*.tar.gz' -mtime +14 -delete
