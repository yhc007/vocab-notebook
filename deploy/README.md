# 배포 런북 — Google VM

CoreDB는 로컬 디스크에 상태를 저장하는 상시 서버라 Cloud Run 대신 **영구 디스크가 붙은 e2-small VM**에 CoreDB와 앱을 함께 올린다.

## 구성 파일

| 파일 | 역할 | 실행 위치 |
| --- | --- | --- |
| `provision-vm.sh` | VM + 영구 디스크 + 방화벽 생성 | 로컬(gcloud) |
| `setup-vm.sh` | 디스크 마운트, Rust/CoreDB/앱 빌드, 서비스 등록 | VM 내부(sudo) |
| `coredb.service` | CoreDB systemd 유닛 (127.0.0.1:9042) | VM |
| `vocab-notebook.service` | 앱 systemd 유닛 (127.0.0.1:8080) | VM |
| `Caddyfile` | HTTPS 리버스 프록시 | VM |
| `backup-coredb.sh` | 데이터 디스크 내 일일 백업 | VM(cron) |

## 사전 준비물

- GCP 프로젝트(결제 활성화) + `gcloud` CLI 로그인
- Anthropic API 키
- (HTTPS 원하면) 도메인 1개 — 없으면 HTTP+IP로 테스트 가능

## 절차

### 1. VM 생성 (로컬에서)

```bash
gcloud auth login
gcloud config set project YOUR_PROJECT_ID
cd deploy
PROJECT=YOUR_PROJECT_ID ZONE=us-central1-a ./provision-vm.sh
# 끝에 출력되는 외부 IP를 메모
```

기본값: e2-small, 부팅 20GB, 데이터 디스크 30GB(pd-balanced), 방화벽 80/443.
CoreDB 9042 포트는 외부에 열지 않는다(앱이 localhost로만 접근).

### 2. (도메인 사용 시) DNS 설정

도메인의 A 레코드를 위 외부 IP로 지정. `deploy/Caddyfile`의 `vocab.example.com`을
실제 도메인으로 수정. 도메인이 없으면 `Caddyfile`에서 `:80` 블록을 대신 사용.

### 3. 코드 업로드 + 셋업 (VM 내부)

```bash
# 로컬에서 레포 전송
gcloud compute scp --zone us-central1-a --recurse ../../vocab-notebook \
  vocab-notebook:~/vocab-notebook

# VM 접속
gcloud compute ssh --zone us-central1-a vocab-notebook

# VM 안에서
cd ~/vocab-notebook/deploy
sudo ./setup-vm.sh
```

### 4. 비밀값 입력

```bash
sudo nano /etc/vocab-notebook.env     # ANTHROPIC_API_KEY 등 채우기
sudo systemctl restart vocab-notebook
```

### 5. 확인

```bash
systemctl status coredb vocab-notebook caddy
journalctl -u vocab-notebook -f
# 브라우저에서 https://<도메인>  (또는 http://<외부IP>)
```

## 주의 / 알려진 변수

- **CoreDB CLI 옵션·바이너리 이름**: `coredb.service`는 `coredb start --host --port
  --data-dir --commitlog-dir`를 가정한다. 실제 옵션이 다르면 `coredb --help`로 확인 후
  유닛 파일을 수정한다. 빌드 산출물 경로도 `/opt/coredb/target/release/`에서 확인.
- **scylla ↔ CoreDB 호환**: 앱이 9042에 연결되지 않으면 `src/db.rs`를 HTTP API
  (`POST /query`) 방식으로 교체한다(메인 README 참고).
- **인증**: 현재 앱에는 Google OAuth 게이트가 아직 없다. 인터넷에 노출하기 전에 스펙 5번의
  OAuth 미들웨어를 먼저 붙이거나, 그 전까지는 방화벽 source-range를 본인 IP로 제한할 것.
- **비용**: e2-small + 디스크 60GB 합쳐 대략 월 $15 내외 + Claude API 사용량.

## 정리(삭제)

```bash
gcloud compute instances delete vocab-notebook --zone us-central1-a
gcloud compute disks delete vocab-data --zone us-central1-a   # 데이터까지 삭제
gcloud compute firewall-rules delete vocab-allow-web
```
