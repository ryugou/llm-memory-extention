# GCE デプロイガイド

LLM Memory Extension (Phase 1) を GCE 上で動かす手順です。前提:
- GCP プロジェクトに `compute.instances.create` 権限がある
- 認証されたユーザの Google OAuth Client ID/Secret が用意されている
- Anthropic API キーが用意されている
- ドメインと SSL 証明書を運用できる

## 1. インスタンス作成

```bash
gcloud compute instances create llm-memory \
  --machine-type=e2-small \
  --zone=asia-northeast1-a \
  --image-family=cos-stable \
  --image-project=cos-cloud \
  --boot-disk-size=20GB \
  --boot-disk-type=pd-balanced \
  --tags=http-server,https-server \
  --service-account=<svc>@<project>.iam.gserviceaccount.com \
  --scopes=cloud-platform
```

無料枠の `e2-micro` でも動作するが、`cargo build --release` は VM 上では行わないため `e2-small` (2 GB RAM) で十分。

## 2. GCS バケット作成 (litestream バックアップ用)

```bash
gsutil mb -l asia-northeast1 -c standard gs://<your-project>-memory-backup
gsutil iam ch serviceAccount:<svc>@<project>.iam.gserviceaccount.com:roles/storage.objectAdmin \
  gs://<your-project>-memory-backup
```

## 3. Secret Manager にシークレットを格納

以下のシークレットを Secret Manager に作成:
- `ANTHROPIC_API_KEY`
- `GOOGLE_OAUTH_CLIENT_ID`
- `GOOGLE_OAUTH_CLIENT_SECRET`
- `JWT_SIGNING_KEY_v1` (32 バイト base64)

例:
```bash
echo -n "$(openssl rand -base64 32)" | gcloud secrets create JWT_SIGNING_KEY_v1 --data-file=-
```

VM の startup script でこれらを `.env` に展開する (省略形):
```bash
mkdir -p /opt/llm-memory
gcloud secrets versions access latest --secret=ANTHROPIC_API_KEY > /opt/llm-memory/.env
echo "ANTHROPIC_API_KEY=$(cat /opt/llm-memory/.env)" > /opt/llm-memory/.env
# ... 同様に他の secret も追記
```

## 4. ファイアウォール / Cloud Load Balancer

VM への直接アクセスはブロックし、HTTPS は Cloud Load Balancer 経由で終端する:

```bash
gcloud compute firewall-rules create allow-lb-to-server \
  --direction=INGRESS \
  --action=ALLOW \
  --source-ranges=130.211.0.0/22,35.191.0.0/16 \
  --rules=tcp:8080 \
  --target-tags=llm-memory

# LB の作成は gcloud compute backend-services / url-maps / target-https-proxies で行う (省略)
```

LB は X-Forwarded-For を末尾に LB IP として付与するため、サーバー側の `TRUSTED_PROXY_COUNT=1` (デフォルト) で OK。

## 5. デプロイ手順

ローカルから:

```bash
gcloud compute scp --zone=asia-northeast1-a \
  docker/docker-compose.yml docker/litestream.yml \
  llm-memory:/opt/llm-memory/

# .env を生成 (上記 Secret Manager 連携を含む)
# サービスアカウントキー (gcp-sa.json) も配置
gcloud compute scp --zone=asia-northeast1-a \
  gcp-sa.json llm-memory:/opt/llm-memory/

# 起動 (初回)
gcloud compute ssh --zone=asia-northeast1-a llm-memory -- \
  'cd /opt/llm-memory && docker compose up -d'
```

定期 pull は `deploy/gce/startup.sh` を cron に登録するか、`docker compose pull && docker compose up -d` を手動で。

## 6. 動作確認

- `curl https://<your-domain>/healthz` → "ok"
- `curl https://<your-domain>/.well-known/oauth-authorization-server` → JSON
- `curl https://<your-domain>/metrics` → Prometheus テキスト

## 7. バックアップからのリストア

litestream は SQLite を GCS にレプリケートしているため、新規 VM で起動する前に:

```bash
docker run --rm -v $(pwd)/data:/data \
  -v $(pwd)/litestream.yml:/etc/litestream.yml \
  -e GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp-sa.json \
  -v $(pwd)/gcp-sa.json:/etc/gcp-sa.json \
  litestream/litestream:0.3.13 \
  restore -if-replica-exists /data/db.sqlite
```

## 8. スケール上限

SQLite + シングルプロセスのため:
- 同時アクティブユーザ: 数十～百程度
- データ量: 数十 GB 程度

それを超えたら Phase 2 (Vegapunk 統合 + マルチインスタンス) へ移行。

## 9. アカウント削除

ユーザは `DELETE /v1/account` でデータをカスケード削除できる (raws / wikis / schemas / tokens / users)。
