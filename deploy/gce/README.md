# GCE デプロイガイド (個人用、nip.io + Caddy 構成)

Phase 1 LLM Memory Extension を **GCE VM 1 台 + nip.io + Caddy 自動 TLS** で動かす最小構成。

## アーキテクチャ

```
[Claude Desktop] --HTTPS--> [GCE VM :443 Caddy] --HTTP--> [Docker: server:8080]
                                                       \-> [Docker: litestream (GCS replicate)]
```

- Caddy が `<IP>.nip.io` 用に Let's Encrypt 証明書を自動取得
- VM の instance service account が GCS への書き込み権限を持ち、litestream は ADC 経由でアクセス (鍵ファイルをディスクに置かない)
- HTTPS は VM 上で終端 (Cloud Load Balancer 不要)

## 1. 事前準備 (ローカル)

### 1-1. GCP 認証 + プロジェクト設定

```bash
gcloud auth login
gcloud config set project gen-lang-client-0184763777
```

### 1-2. API 有効化

```bash
gcloud services enable compute.googleapis.com storage.googleapis.com
```

### 1-3. GCS バックアップバケット作成

```bash
gsutil mb -l asia-northeast1 -c standard gs://gen-lang-client-0184763777-memory-backup
```

### 1-4. JWT 鍵生成 (ローカルで)

```bash
openssl rand -base64 32
# 出力をメモ → .env の JWT_SIGNING_KEY_v1 に
```

### 1-5. Google OAuth クライアント作成 (Web 用)

`https://console.cloud.google.com/apis/credentials` で:

1. OAuth consent screen 設定 (User Type: External, Test mode)
2. Credentials → Create credentials → OAuth client ID → Web application
3. Authorized redirect URIs に **後で** `https://<IP>.nip.io/oauth/callback/google` を追加 (VM 作成後に IP が確定するため)
4. client_id と client_secret をメモ

## 2. インスタンス用 Service Account 作成

```bash
PROJECT=gen-lang-client-0184763777
SA_NAME=llm-memory-sa

gcloud iam service-accounts create $SA_NAME \
  --display-name="LLM Memory VM service account"

# Litestream が GCS bucket に書き込む権限
gsutil iam ch \
  serviceAccount:${SA_NAME}@${PROJECT}.iam.gserviceaccount.com:roles/storage.objectAdmin \
  gs://${PROJECT}-memory-backup
```

## 3. 静的 IP 予約

```bash
gcloud compute addresses create llm-memory-ip \
  --region=asia-northeast1

# 取得した IP を確認
gcloud compute addresses describe llm-memory-ip \
  --region=asia-northeast1 --format='value(address)'
# 例: 34.146.12.34 → PUBLIC_DOMAIN = 34-146-12-34.nip.io
```

## 4. VM 作成

```bash
PROJECT=gen-lang-client-0184763777
SA=llm-memory-sa@${PROJECT}.iam.gserviceaccount.com
IP=$(gcloud compute addresses describe llm-memory-ip --region=asia-northeast1 --format='value(address)')

gcloud compute instances create llm-memory \
  --zone=asia-northeast1-a \
  --machine-type=e2-small \
  --image-family=debian-12 \
  --image-project=debian-cloud \
  --boot-disk-size=20GB \
  --boot-disk-type=pd-balanced \
  --tags=llm-memory \
  --service-account=${SA} \
  --scopes=cloud-platform \
  --address=${IP}
```

## 5. ファイアウォール (80/443 公開)

```bash
gcloud compute firewall-rules create allow-https-llm-memory \
  --direction=INGRESS \
  --action=ALLOW \
  --source-ranges=0.0.0.0/0 \
  --rules=tcp:80,tcp:443,udp:443 \
  --target-tags=llm-memory
```

## 6. VM セットアップ (SSH)

```bash
gcloud compute ssh llm-memory --zone=asia-northeast1-a
```

VM 内で:

```bash
# Docker と git
sudo apt-get update
sudo apt-get install -y docker.io docker-compose-plugin git
sudo usermod -aG docker $USER
exit   # 一度抜けて再ログイン
```

再 SSH:

```bash
gcloud compute ssh llm-memory --zone=asia-northeast1-a
```

VM 内で:

```bash
# リポジトリ取得
git clone https://github.com/ryugou/llm-memory-extention.git
cd llm-memory-extention

# .env 作成 (下記内容を、ローカルでメモした値で埋める)
nano .env
```

`.env` の内容:

```
DATABASE_URL=sqlite:///data/db.sqlite
ANTHROPIC_API_KEY=sk-ant-...
GOOGLE_OAUTH_CLIENT_ID=...
GOOGLE_OAUTH_CLIENT_SECRET=...
JWT_SIGNING_KEY_v1=<openssl rand -base64 32 の出力>
TRUSTED_PROXY_COUNT=1
MODEL_HAIKU=claude-haiku-4-5-20251001
MODEL_SONNET=claude-sonnet-4-6
RUST_LOG=info
PUBLIC_DOMAIN=<IP の `.` を `-` に置換>.nip.io
PUBLIC_URL=https://${PUBLIC_DOMAIN}
LITESTREAM_BUCKET=gen-lang-client-0184763777-memory-backup
```

## 7. Google OAuth Console に redirect URI 追加

VM の IP が決まったので、§1-5 で作った OAuth クライアントに以下を追加:

- Authorized redirect URIs: `https://<IP-with-hyphens>.nip.io/oauth/callback/google`

## 8. 起動

VM 内で:

```bash
cd ~/llm-memory-extention/docker
docker compose --env-file ../.env up --build -d
```

初回ビルドは数分かかる (Rust 全クレートを compile)。

## 9. 動作確認

ローカルマシンから:

```bash
DOMAIN=34-146-12-34.nip.io   # ← 実際の値に置換
curl https://${DOMAIN}/healthz                              # → ok
curl https://${DOMAIN}/.well-known/oauth-authorization-server | jq
curl https://${DOMAIN}/metrics | head
```

VM 内のサーバーログ:

```bash
cd ~/llm-memory-extention/docker
docker compose logs -f --tail=200 server caddy
```

## 10. Claude Desktop 連携

Claude Desktop の MCP 設定に追加 (詳細は `docs/superpowers/runbooks/e2e-checklist.md` セクション 2):

- URL: `https://${DOMAIN}/mcp`
- 初回接続時にブラウザで Google OAuth 認可

## 11. バックアップからの復元

新しい VM に移行する場合、初回起動前に GCS から `db.sqlite` を復元:

```bash
# VM 内、docker compose up する前に
docker run --rm \
  -v llm-memory-extention_data:/data \
  -v "$PWD/litestream.yml:/etc/litestream.yml:ro" \
  -e LITESTREAM_BUCKET=gen-lang-client-0184763777-memory-backup \
  litestream/litestream:0.3.13 \
  restore -if-replica-exists /data/db.sqlite
```

`llm-memory-extention_data` は docker-compose の named volume 名 (`<project>_<volume>`)。

## 12. スケール上限

SQLite + シングルプロセスのため:

- 同時アクティブユーザ: 数十〜百
- データ量: 数十 GB

それを超えたら Phase 2 (Vegapunk 統合 + マルチインスタンス) へ。

## 13. アカウント削除

ユーザは `DELETE /v1/account` でデータをカスケード削除できる (raws / wikis / schemas / tokens / users)。発行済み JWT は middleware が user 存在チェックを行うため、削除直後から無効化される。
