#!/usr/bin/env bash
# GCP에 vocab-notebook용 VM과 영구 디스크, 방화벽을 생성한다.
# 로컬 머신에서 실행 (gcloud CLI + 로그인 필요). VM 내부에서 실행하지 말 것.
#
#   gcloud auth login
#   gcloud config set project YOUR_PROJECT_ID
#   ./provision-vm.sh
set -euo pipefail

# ── 설정값 (필요에 맞게 수정) ─────────────────────────────
PROJECT="${PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
ZONE="${ZONE:-us-central1-a}"
VM_NAME="${VM_NAME:-vocab-notebook}"
MACHINE="${MACHINE:-e2-small}"          # vCPU 2 / 2GB. 메모리 더 필요시 e2-medium
IMAGE_FAMILY="${IMAGE_FAMILY:-debian-12}"
IMAGE_PROJECT="${IMAGE_PROJECT:-debian-cloud}"
BOOT_DISK_GB="${BOOT_DISK_GB:-20}"
DATA_DISK_NAME="${DATA_DISK_NAME:-vocab-data}"
DATA_DISK_GB="${DATA_DISK_GB:-30}"       # CoreDB SSTable/WAL/백업용 영구 디스크
DATA_DISK_TYPE="${DATA_DISK_TYPE:-pd-balanced}"
TAG="vocab-notebook"
# ─────────────────────────────────────────────────────────

echo ">> project=$PROJECT zone=$ZONE vm=$VM_NAME"
[ -n "$PROJECT" ] || { echo "PROJECT가 비어있음. gcloud config set project ... 먼저 실행"; exit 1; }

# 1) 데이터용 영구 디스크 생성 (부팅 디스크와 분리 → 재생성에도 데이터 보존)
if ! gcloud compute disks describe "$DATA_DISK_NAME" --zone "$ZONE" >/dev/null 2>&1; then
  gcloud compute disks create "$DATA_DISK_NAME" \
    --zone "$ZONE" --size "${DATA_DISK_GB}GB" --type "$DATA_DISK_TYPE"
else
  echo ">> data disk already exists: $DATA_DISK_NAME"
fi

# 2) VM 생성 + 데이터 디스크 부착
gcloud compute instances create "$VM_NAME" \
  --zone "$ZONE" \
  --machine-type "$MACHINE" \
  --image-family "$IMAGE_FAMILY" --image-project "$IMAGE_PROJECT" \
  --boot-disk-size "${BOOT_DISK_GB}GB" --boot-disk-type pd-balanced \
  --disk "name=$DATA_DISK_NAME,device-name=vocabdata,mode=rw,auto-delete=no" \
  --tags "$TAG"

# 3) 방화벽: HTTP(80)/HTTPS(443)만 허용. CoreDB 9042는 외부에 열지 않음(localhost 전용).
if ! gcloud compute firewall-rules describe vocab-allow-web >/dev/null 2>&1; then
  gcloud compute firewall-rules create vocab-allow-web \
    --direction INGRESS --action ALLOW \
    --rules tcp:80,tcp:443 \
    --target-tags "$TAG" \
    --source-ranges 0.0.0.0/0
fi

EXT_IP=$(gcloud compute instances describe "$VM_NAME" --zone "$ZONE" \
  --format='get(networkInterfaces[0].accessConfigs[0].natIP)')

echo
echo ">> VM 생성 완료. 외부 IP: $EXT_IP"
echo ">> 다음 단계:"
echo "   1) (도메인 사용 시) DNS A 레코드를 $EXT_IP 로 설정"
echo "   2) gcloud compute scp --zone $ZONE --recurse <repo> $VM_NAME:~/vocab-notebook"
echo "   3) gcloud compute ssh --zone $ZONE $VM_NAME"
echo "   4) VM 안에서: cd ~/vocab-notebook/deploy && sudo ./setup-vm.sh"
