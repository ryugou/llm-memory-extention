# GCE デプロイガイド (個人用、nip.io + Caddy 構成)

Phase 1 LLM Memory Extension を **GCE VM 1 台 + nip.io + Caddy 自動 TLS** で動かす最小構成。

このガイドは以降 `GCP_PROJECT_ID` を環境変数として参照する。手順実行前にローカルシェルで:

```bash
export GCP_PROJECT_ID=<your-gcp-project>
```

## 個人用構成の Accepted Risk

このガイドは **Phase 1 個人用** を想定。以下は意図的に受容しているリスクで、業務利用時は再評価が必要:

- **Firewall `0.0.0.0/0` で 80/443 を公開**: OAuth/MCP endpoint は public な前提。OAuth/DCR 側に in-memory session/code map の cap + expiry pruning (`crates/llm-memory-auth/src/authorization_server.rs`) は入っているが、token 発行や DCR 自体に per-IP rate-limit は無いため、悪意ある相手が DCR スパム / authorize spam を投げると CPU/log の負荷は受ける。MCP tool 呼び出しは認証後 per-user の rate-limit が効く。本格運用に出すなら Cloud Armor 等で前段制限を入れる。
- **nip.io + Let's Encrypt CT log**: 取得した cert が `crt.sh` 等で永続記録されるため VM の external IP が公開ログに残る。GCE IP は scan で見つかるので追加リスクは軽微。
- **secret は Secret Manager で集中管理 + container env 化**: OAuth client / JWT 鍵は `.env` には書かず、Secret Manager に保管 (§1-5)。VM 起動時に `deploy/gce/run.sh` が fetch して tmpfs (`/dev/shm/llm-memory-secrets.env`) 経由で compose に渡す。これにより `.env` / git repo / 通常ファイルシステムには secret 文字列を残さない。**ただし** compose の `env_file:` で注入した値は Docker container config として `/var/lib/docker/containers/<id>/config.v2.json` 等に書かれる。これは Docker daemon に到達できる主体 (root、`docker` group メンバ、`/var/run/docker.sock` にアクセス可能なユーザ) なら `docker inspect` で閲覧可能。本構成は `sudo docker compose` を踏める運用者 (= Docker daemon に到達できる = 実質 root 相当) に対しては protection しない (Phase 1 個人 / 自社運用の前提)。完全な secret 化が必要なら secret を env でなくファイル mount + entrypoint/app 側で読み込む方式に変更する必要がある (別 PR の値打ち)。
  同 secret 名 (例: `jwt-signing-key-v1`) の値だけを差し替える rotation は Console で新バージョン作成 → 再起動で完結。新しい secret 名を追加する rotation (例: `jwt-signing-key-v2`) は §1-5 / §2 に従い IAM accessor 付与 + `run.sh` の `SECRETS` 配列に 1 行追加が必要。
- **VM の Instance SA に `--scopes=cloud-platform`**: scope はあくまでメタデータ token 経由で叩ける API の上限を定めるだけ。実際の権限は SA に付与した IAM role (本ガイドでは `roles/storage.objectAdmin` (GCS バックアップ書込み) と `roles/aiplatform.user` (Vertex AI 呼び出し) の 2 つのみ) で制御。

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
gcloud config set project "${GCP_PROJECT_ID}"
```

### 1-2. API 有効化

新規 project では IAM / Vertex AI API も default 無効化されているので、後段で必要になる API をまとめて有効化する。

```bash
gcloud services enable \
  compute.googleapis.com \
  storage.googleapis.com \
  iam.googleapis.com \
  aiplatform.googleapis.com \
  secretmanager.googleapis.com
```

`aiplatform.googleapis.com` は Vertex AI (Gemini Flash / Pro 経由の概念抽出と wiki 合成) 用。
`secretmanager.googleapis.com` は OAuth client / JWT 鍵などの secret 保管用 (§1-5 で詳述)。

**確認** (5 つすべて enabled で返ること):

```bash
gcloud services list --enabled --project="${GCP_PROJECT_ID}" \
  --filter='config.name:(compute.googleapis.com OR storage.googleapis.com OR iam.googleapis.com OR aiplatform.googleapis.com OR secretmanager.googleapis.com)' \
  --format='value(config.name)'
```

`aiplatform.googleapis.com` だけが未 enable のまま気付かず起動すると server 自体は立ち上がるが、Gemini 呼び出し時に permission denied で raw 投入後の wiki 合成が静かに失敗する。

### 1-3. GCS バックアップバケット作成

```bash
gsutil mb -l asia-northeast1 -c standard "gs://${GCP_PROJECT_ID}-memory-backup"
```

### 1-4. Google OAuth クライアント作成 (Web 用)

Google Cloud Console の **API とサービス** → **認証情報** (UI 刷新後は **Google Auth Platform**) で:

1. **対象** (旧 OAuth consent screen / ユーザー設定):
   - **ユーザータイプ** が「外部」、**公開ステータス** が「テスト中」になっていること
   - **テストユーザー** に Claude Desktop で sign-in するアカウント (`<user>@example.com` 等) を追加
     — 外部 + テスト中アプリはテストユーザーに登録された address しか sign-in を許可しない
2. **クライアント** (旧 Credentials → OAuth 2.0 Client IDs):
   - 「**+ クライアントを作成**」 → アプリケーションの種類: **ウェブ アプリケーション**
   - 任意の名前 (例: `llm-memory`)
   - **承認済みのリダイレクト URI** に `https://<IP-with-hyphens>.nip.io/oauth/callback/google` を後で追加
     (VM 作成後に IP が確定するため、§7 で改めて登録する)
   - **クライアント ID** と **クライアント シークレット** をメモ (§1-5 で Secret Manager に登録する)

### 1-5. Secret Manager に secret を登録

OAuth client / JWT 鍵などの secret は Secret Manager で管理する。`.env` に直書きしない方針 (詳細は `.env.example` の `--- Secrets ---` セクション参照)。

3 つの secret を作成 (`secret-name` は run.sh の `SECRETS` 配列と一致させること):

```bash
# OAuth client_id (§1-4 でメモした値)
printf '%s' '<クライアント ID>' | \
  gcloud secrets create google-oauth-client-id \
    --project="${GCP_PROJECT_ID}" --data-file=- --replication-policy=automatic

# OAuth client_secret (§1-4 でメモした値)
printf '%s' '<クライアント シークレット>' | \
  gcloud secrets create google-oauth-client-secret \
    --project="${GCP_PROJECT_ID}" --data-file=- --replication-policy=automatic

# JWT 署名鍵 v1 (新規生成、base64 32 バイト)
openssl rand -base64 32 | tr -d '\n' | \
  gcloud secrets create jwt-signing-key-v1 \
    --project="${GCP_PROJECT_ID}" --data-file=- --replication-policy=automatic
```

`--project` 明示は active project 違いで別 project に secret を作ってしまう事故を防ぐため。

`printf '%s'` で末尾改行を抑制 (改行込みで保存すると compose env が壊れる)。

新しい JWT 鍵に rotation するときは:

```bash
openssl rand -base64 32 | tr -d '\n' | \
  gcloud secrets create jwt-signing-key-v2 \
    --project="${GCP_PROJECT_ID}" --data-file=- --replication-policy=automatic
```

その後 `deploy/gce/run.sh` の `SECRETS` 配列に `"jwt-signing-key-v2:JWT_SIGNING_KEY_v2"` を追加して再起動。

## 2. インスタンス用 Service Account 作成

```bash
SA_NAME=llm-memory-sa

gcloud iam service-accounts create "${SA_NAME}" \
  --display-name="LLM Memory VM service account"

# Litestream が GCS bucket に書き込む権限 (最小権限)
gsutil iam ch \
  "serviceAccount:${SA_NAME}@${GCP_PROJECT_ID}.iam.gserviceaccount.com:roles/storage.objectAdmin" \
  "gs://${GCP_PROJECT_ID}-memory-backup"

# Vertex AI (Gemini) を ADC 経由で呼び出すための権限
gcloud projects add-iam-policy-binding "${GCP_PROJECT_ID}" \
  --member="serviceAccount:${SA_NAME}@${GCP_PROJECT_ID}.iam.gserviceaccount.com" \
  --role="roles/aiplatform.user"

# Secret Manager から secret を読む権限 (§1-5 で作成した 3 つの secret 個別に付与)
for sec in google-oauth-client-id google-oauth-client-secret jwt-signing-key-v1; do
  gcloud secrets add-iam-policy-binding "${sec}" \
    --project="${GCP_PROJECT_ID}" \
    --member="serviceAccount:${SA_NAME}@${GCP_PROJECT_ID}.iam.gserviceaccount.com" \
    --role="roles/secretmanager.secretAccessor"
done
```

JWT 鍵を rotation して `jwt-signing-key-v2` を追加した場合は、同じ for-loop で v2 にも accessor を付与する。

**確認** (各コマンドが対象 SA まで検証するように `--filter` に SA member を含めている。別 SA / user に role が付いているだけでは通らない):

```bash
SA="llm-memory-sa@${GCP_PROJECT_ID}.iam.gserviceaccount.com"

# project レベル: roles/aiplatform.user が対象 SA に付与されているか
gcloud projects get-iam-policy "${GCP_PROJECT_ID}" \
  --flatten='bindings[].members' \
  --filter="bindings.role=roles/aiplatform.user AND bindings.members:serviceAccount:${SA}" \
  --format='value(bindings.members)'
# 期待値: serviceAccount:llm-memory-sa@<project-id>.iam.gserviceaccount.com (空なら未付与)

# bucket レベル: roles/storage.objectAdmin が対象 SA に付与されているか (jq で role+member を同時 match)
gsutil iam get "gs://${GCP_PROJECT_ID}-memory-backup" \
  | jq -er --arg sa "serviceAccount:${SA}" \
      '.bindings[] | select(.role=="roles/storage.objectAdmin") | .members[] | select(. == $sa)'
# 期待値: 上記 SA の文字列が 1 行返る (空 / exit code 非 0 なら未付与)

# secret レベル: 3 secret それぞれ roles/secretmanager.secretAccessor が対象 SA に付与されているか
# (filter の `=` と grep の `-x` で完全一致にして、似た prefix の別 SA を取り違えない)
for sec in google-oauth-client-id google-oauth-client-secret jwt-signing-key-v1; do
  echo -n "${sec}: "
  gcloud secrets get-iam-policy "${sec}" --project="${GCP_PROJECT_ID}" \
    --flatten='bindings[].members' \
    --filter="bindings.role=roles/secretmanager.secretAccessor AND bindings.members=serviceAccount:${SA}" \
    --format='value(bindings.members)' \
    | grep -qx "serviceAccount:${SA}" && echo OK || echo MISSING
done
# 期待値: 3 行すべて OK (MISSING があれば §2 の add-iam-policy-binding を再実行)
```

どれか欠けていると container 起動後に `secret access denied` / `Gemini permission denied` で raw 投入時の wiki 合成が静かに失敗する。`jq` は GCE Debian 12 default image に同梱されている。

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
SA="llm-memory-sa@${GCP_PROJECT_ID}.iam.gserviceaccount.com"
IP=$(gcloud compute addresses describe llm-memory-ip --region=asia-northeast1 --format='value(address)')

# e2-medium (4 GB RAM) を採用: VM 上で `cargo build --release` を実行するため、
# e2-small (2 GB) だと linker フェーズで OOM するリスクが高い。
gcloud compute instances create llm-memory \
  --zone=asia-northeast1-a \
  --machine-type=e2-medium \
  --image-family=debian-12 \
  --image-project=debian-cloud \
  --boot-disk-size=20GB \
  --boot-disk-type=pd-balanced \
  --tags=llm-memory \
  --service-account="${SA}" \
  --scopes=cloud-platform \
  --address="${IP}"
```

## 5. ファイアウォール (80/443 公開)

```bash
gcloud compute firewall-rules create allow-https-llm-memory \
  --direction=INGRESS \
  --action=ALLOW \
  --source-ranges=0.0.0.0/0 \
  --rules=tcp:80,tcp:443 \
  --target-tags=llm-memory
```

`tcp/80` は Let's Encrypt HTTP-01 challenge + http→https redirect 用。HTTP/3 (UDP/443) は Claude Desktop には不要なので開けない。

## 6. VM セットアップ (SSH)

```bash
gcloud compute ssh llm-memory --zone=asia-northeast1-a
```

VM 内で:

```bash
# git と curl のインストール
sudo apt-get update
sudo apt-get install -y git curl ca-certificates

# Docker と Compose v2 plugin を Docker 公式 repo から取得 (Debian 12 の
# default repo には docker-compose-plugin が無いため、`apt-get install
# docker-compose-plugin` では失敗する)。公式の get.docker.com スクリプトを
# 使うのが最小手数。
curl -fsSL https://get.docker.com | sudo sh

# (注意) ユーザを `docker` group に入れる慣例があるが、`docker` group メンバー
# は実質 root 権限になる (docker 経由でホストを bind-mount できる)。本ガイドは
# 個人 VM 想定のため、毎回 `sudo docker compose ...` で実行する方針にする。
```

インストール確認:

```bash
sudo docker compose version    # e.g. Docker Compose version v2.x.x
```

VM 内で続行:

```bash
# README 内で参照する project ID を VM 側でも export する
# (`${GCP_PROJECT_ID}` を含むコマンドを VM で実行するため)
export GCP_PROJECT_ID=<your-gcp-project>
# ログイン毎に有効化したいなら ~/.bashrc に追記:
echo "export GCP_PROJECT_ID=${GCP_PROJECT_ID}" >> ~/.bashrc

# リポジトリ取得 (public repo を前提)。private repo なら deploy key を別途設定。
git clone https://github.com/ryugou/llm-memory-extention.git
cd llm-memory-extention

# .env 作成 (下記内容を、ローカルでメモした値で埋める)
nano .env
```

`.env` の内容 (非 secret のみ。secret は §1-5 で Secret Manager に登録済みで、起動時に `run.sh` が fetch して container に注入する):

```
DATABASE_URL=sqlite:///data/db.sqlite
TRUSTED_PROXY_COUNT=1
VERTEX_PROJECT=<your-gcp-project>
VERTEX_LOCATION=us-central1
MODEL_EXTRACT=gemini-2.5-flash
MODEL_SYNTH=gemini-2.5-pro
RUST_LOG=info
PUBLIC_DOMAIN=34-146-12-34.nip.io
PUBLIC_URL=https://34-146-12-34.nip.io
LITESTREAM_BUCKET=<your-gcp-project>-memory-backup
```

`34-146-12-34` は §3 で予約した IP の `.` を `-` に置換した値。`<your-gcp-project>` はローカルで設定した `${GCP_PROJECT_ID}` の値で置換する。

書き込み後、念のためファイル権限を絞る (`.env` に secret はもう無いが、設定値の改竄防止):

```bash
chmod 600 ~/llm-memory-extention/.env
```

`.env` 展開の確認 (compose は env-file 内の `${...}` を展開しないので、文字列リテラル混入を検出する):

```bash
cd ~/llm-memory-extention/docker
sudo docker compose -p llm-memory-extention --env-file ../.env config \
  | grep -E '^[[:space:]]+PUBLIC_URL:'
# 期待値: PUBLIC_URL: https://34-146-12-34.nip.io (完全な URL)
# 失敗: PUBLIC_URL: https://${PUBLIC_DOMAIN} などのリテラルが残る → .env が誤り
```

**旧フォーマットが残っていないかの確認** (Secret Manager 設計に切り替わる前の構成で立てた VM を引き継ぐ場合、`.env` に下記の残骸が混入しているとそれらが container に注入されてしまい、Secret Manager 由来の値を上書きしないものの noise として残る):

```bash
grep -E '^(GOOGLE_OAUTH_CLIENT_ID|GOOGLE_OAUTH_CLIENT_SECRET|JWT_SIGNING_KEY_|ANTHROPIC_API_KEY|MODEL_HAIKU|MODEL_SONNET|__FILL_ME__)' ~/llm-memory-extention/.env
# 期待値: 何も出力されない
# 出てきたら .env から該当行を削除する (これらは Secret Manager 由来 or 廃止済み env)
```

以降、compose を直接叩く代わりに `deploy/gce/run.sh` 経由で起動する (§8)。`run.sh` が Secret Manager fetch + tmpfs (`/dev/shm/llm-memory-secrets.env`) への書き出し + compose に必要な `-p llm-memory-extention --env-file ../.env` の付与をまとめて行う。

### 6-1. 既存 VM での再デプロイ手順 (2 回目以降)

新 commit を取り込んで再起動する場合:

```bash
cd ~/llm-memory-extention
git pull --ff-only                           # run.sh / docker-compose.yml の更新を取り込む
./deploy/gce/run.sh down                     # 旧 container を停止 (data volume は保持)
./deploy/gce/run.sh up --build -d            # 再ビルド + 起動
```

`git pull` をせずに `run.sh` 経由で起動すると、古い `docker-compose.yml` で立ち上がって Secret Manager 由来の env が container に届かないことがあるので必ず pull する。

container env に Secret Manager の値が実際に届いているかは、起動後に下記 2 段で確認する:

**(a) キーが全て揃っているか (値は完全 redact)**

```bash
sudo docker inspect llm-memory-extention-server-1 \
  --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | grep -E '^(GOOGLE_OAUTH_CLIENT_ID|GOOGLE_OAUTH_CLIENT_SECRET|JWT_SIGNING_KEY_[^=]+|VERTEX_PROJECT|PUBLIC_URL)=' \
  | sed -E 's/^([^=]+)=.*/\1=<redacted>/'
# 期待値: 5 行 (GOOGLE_OAUTH_CLIENT_ID / GOOGLE_OAUTH_CLIENT_SECRET / JWT_SIGNING_KEY_v1 / VERTEX_PROJECT / PUBLIC_URL) が表示される
# JWT_SIGNING_KEY_<kid> の <kid> 部分は v1 / v2 ... と rotation で増える可能性があるため `[^=]+` で吸収する
```

**(b) `__FILL_ME__` や旧 secret の混入が無いか (= 古い `.env` が container に届いていないか)**

```bash
sudo docker inspect llm-memory-extention-server-1 \
  --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | grep -E '__FILL_ME__|MODEL_HAIKU|MODEL_SONNET|ANTHROPIC_API_KEY' \
  | sed -E 's/^([^=]+)=.*/\1=<旧値検出>/'
# 期待値: 何も出力されない
# 出力があった場合: `run.sh` 経由でなく直接 `docker compose up` した可能性 → §8 の手順で起動し直す
```

GOOGLE_OAUTH_CLIENT_ID の値そのもの (= Secret Manager 由来か `.env` 由来か) を確認したい場合は、Cloud Console の OAuth クライアント一覧と Secret Manager の値が一致していることを別途確認する。Console / Secret Manager 以外には secret を平文表示しない方針。

## 7. Google OAuth Console に redirect URI 追加

VM の IP が決まったので、§1-4 で作った OAuth クライアントに以下を追加:

- **承認済みのリダイレクト URI** (Authorized redirect URIs): `https://<IP-with-hyphens>.nip.io/oauth/callback/google`

Google Cloud Console の **Google Auth Platform** → **クライアント** からエントリを開いて編集 → **保存**。

## 8. 起動

VM 内で `deploy/gce/run.sh` を使う。これが §1-5 で登録した Secret Manager の値を fetch して `/dev/shm/llm-memory-secrets.env` (tmpfs) に書き、compose に渡す:

```bash
cd ~/llm-memory-extention
./deploy/gce/run.sh up --build -d
```

引数は `docker compose` にそのまま転送される (`up --build -d`、`logs -f --tail=200 server`、`down`、`ps` 等)。

初回ビルドは e2-medium で 5〜10 分かかる (Rust 全クレートを release プロファイルで compile)。`sudo` を毎回付けるのは Accepted Risk セクションの方針通り (`run.sh` 内部で `sudo docker compose ...` を呼ぶ)。

## 9. 動作確認

### 9-1. エンドポイントスモークテスト

ローカルマシンから (素の `curl` のみ。整形して読みたければ `| jq` を付けて OK):

```bash
DOMAIN=34-146-12-34.nip.io   # ← 実際の値に置換
curl https://${DOMAIN}/healthz                              # → ok
curl https://${DOMAIN}/.well-known/oauth-authorization-server
curl https://${DOMAIN}/metrics | head
curl -X POST https://${DOMAIN}/mcp -o /dev/null -w '%{http_code}\n'   # → 401 (認証ガードが効いている)
# 401 以外 (404 / 5xx / 3xx) が返る場合は Caddy / server / OAuth 設定のどこかに問題あり → §14 で切り分け
```

`/metrics` の `concept_rebuild_failed_total` / `rebuild_failed_total` は counter (累積値) のため、絶対値 0 だけで「成功」と判断してはいけない (raw が 1 件も投入されていない初期状態でも 0)。実際の抽出系の動作確認は §9-3 (raws / wikis 行の存在) と併用すること。

### 9-2. サーバーログ

VM 内で:

```bash
cd ~/llm-memory-extention
./deploy/gce/run.sh logs -f --tail=200 server caddy
```

### 9-3. データの実在確認 (raw 投入後)

Claude Desktop から `raw_append` を 1 件投げた直後、SQLite の中を見て raws / wikis に行が入っているかを確認する。

**初回のみ: VM ホストに sqlite3 を install** (Debian 12 default image には未同梱):

```bash
sudo apt-get install -y sqlite3
```

VM ホスト側で named volume の mountpoint を取って読む (read-only で安全):

```bash
DB_PATH=$(sudo docker volume inspect llm-memory-extention_data --format '{{.Mountpoint}}')/db.sqlite

sudo sqlite3 -header -column "${DB_PATH}" \
  "SELECT id, owner_id, substr(title,1,20) AS title, created_at FROM raws ORDER BY created_at DESC LIMIT 3;"

sudo sqlite3 -header -column "${DB_PATH}" \
  "SELECT scope, concept, substr(content,1,40) AS content, last_rebuilt_at FROM wikis ORDER BY last_rebuilt_at DESC LIMIT 3;"
```

ホストに sqlite3 を入れたくない場合の fallback として alpine の sqlite で attach する手もある (third-party image 依存を増やす点に注意):

```bash
sudo docker run --rm -v llm-memory-extention_data:/data:ro alpine:3 \
  sh -c 'apk add --no-cache sqlite >/dev/null && sqlite3 -header -column /data/db.sqlite \
    "SELECT id, owner_id, substr(title,1,20) AS title, created_at FROM raws ORDER BY created_at DESC LIMIT 3;"'
```

raws に行があるのに wikis が空のままで 30 秒以上経つ場合、Vertex AI 呼び出し失敗 (IAM 未付与 / API 未 enable / quota) を疑い、`./deploy/gce/run.sh logs server` で ERROR 行を探す。

## 10. Claude Desktop 連携

Claude Desktop の MCP 設定に追加 (詳細は `docs/superpowers/runbooks/e2e-checklist.md` セクション 2):

- URL: `https://${DOMAIN}/mcp`
- 初回接続時にブラウザで Google OAuth 認可

## 11. バックアップからの復元

**前提**: §8 の起動コマンドで `-p llm-memory-extention` を付けているため、compose project 名は `llm-memory-extention`、named volume は `llm-memory-extention_data` で作られている。下記 `docker run` はその volume 名にハードコードで attach する。`-p` を変えた場合は `sudo docker volume ls` で実名を確認して書き換えること。

新しい VM に移行する場合、初回起動前に GCS から `db.sqlite` を復元:

```bash
# VM 上で実行。docker compose up する前に。
cd ~/llm-memory-extention/docker
sudo docker run --rm \
  -v llm-memory-extention_data:/data \
  -v "$PWD/litestream.yml:/etc/litestream.yml:ro" \
  -e LITESTREAM_BUCKET="${GCP_PROJECT_ID}-memory-backup" \
  litestream/litestream:0.3.13 \
  restore -if-replica-exists /data/db.sqlite
```

## 11-1. Shutdown 時のデータ整合性 (note)

`server.depends_on: - litestream` で停止順は **server → litestream** に固定されている。これにより graceful shutdown (`docker compose down`、SIGTERM 等) では server の最後の DB 書き込み完了後に litestream が止まり、litestream は SIGTERM 受領時に終端 sync を試みる。

ただし `depends_on` は **順序保証のみ** で sync 完了は保証しない。具体的には:

- graceful shutdown: ほぼロスレス (server stop → litestream 最終 sync → litestream stop)
- ungraceful (kill -9 / OOM / VM クラッシュ): `sync-interval: 5m` 分まで replica に未反映の write が失われ得る (= RPO 5 分)

ロス窓を縮めたい場合、Litestream 0.3.x には外部から sync を強制する push trigger コマンドが無いので、`docker/litestream.yml` の `sync-interval` を短く (例: `30s`) するのが唯一の手段。間隔を絞ると GCS API quota の月次消費が増えるので運用適正値とのトレードオフ。

## 12. スケール上限

SQLite + シングルプロセスのため:

- 同時アクティブユーザ: 数十〜百
- データ量: 数十 GB

それを超えたら Phase 2 (Vegapunk 統合 + マルチインスタンス) へ。

## 13. アカウント削除

ユーザは `DELETE /v1/account` でデータをカスケード削除できる (raws / wikis / schemas / tokens / users)。発行済み JWT は middleware が user 存在チェックを行うため、削除直後から無効化される。

## 14. トラブルシューティング

以下のコマンドはすべて **VM 上で `~/llm-memory-extention` (repo root) で実行** することを前提とする (`deploy/gce/run.sh` を `./deploy/gce/run.sh ...` の形で叩くため):

```bash
cd ~/llm-memory-extention
```

直接 `docker compose` を叩く例 (`docker volume ls` 等) はそのまま VM のどこでも実行できる。

### `docker compose up --build` が OOM で落ちる
- `dmesg | grep -i kill` で OOM Kill を確認
- 解決策: VM を `e2-medium` 以上に変更 (`gcloud compute instances set-machine-type`)、または `--build` 中だけ swap 2 GB を一時追加

### `curl https://${DOMAIN}/healthz` が応答しない
- DNS 解決確認: `nslookup ${DOMAIN}` → VM の static IP が返るか
- ファイアウォール: `gcloud compute firewall-rules list --filter='name=allow-https-llm-memory'`
- Caddy ログ: `cd ~/llm-memory-extention && ./deploy/gce/run.sh logs caddy | tail -50`
- Let's Encrypt rate limit (週 50 cert/domain) に当たっていれば 5 日待つか staging endpoint を試す

### raw 投入はできるのに wiki が一向に生成されない (Gemini 抽出が静かに失敗)

raw_append 直前に `/metrics` で `concept_rebuild_failed_total` の値を控えておき、raw_append 後に再度 scrape したとき値が増えている場合 (counter は累積値なので絶対値でなく **差分** で判定する)、または server ログに `aiplatform` の permission denied / `INVALID_ARGUMENT` が出る場合、もしくは raws に行があるのに 30 秒以上経っても §9-3 の wikis に行が増えない場合:

- API が enabled か: `gcloud services list --enabled --project="${GCP_PROJECT_ID}" --filter='config.name:aiplatform.googleapis.com' --format='value(config.name)'` → 空なら `gcloud services enable aiplatform.googleapis.com` で有効化
- SA に role が付いているか: `gcloud projects get-iam-policy "${GCP_PROJECT_ID}" --flatten='bindings[].members' --filter="bindings.members:serviceAccount:llm-memory-sa@${GCP_PROJECT_ID}.iam.gserviceaccount.com" --format='value(bindings.role)' | grep aiplatform.user` → 空なら §2 の `add-iam-policy-binding` を実行
- VM 上で metadata server から SA email が取れるか: `curl -fsS -H "Metadata-Flavor: Google" 'http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/email'` → `llm-memory-sa@<project-id>.iam.gserviceaccount.com` が返ること (server container は scratch ベースで `curl` / `wget` が入っていないので VM ホスト側で叩く)
- 修正後は `./deploy/gce/run.sh down && ./deploy/gce/run.sh up -d` で再起動 (IAM 変更は token cache の関係で即時反映されないことがある)

### container の env に Secret Manager の値ではなく `__FILL_ME__` / `.env` 直書き値が入っている

旧構成 (Secret Manager 設計に切り替わる前) の VM を引き継いだ場合、`.env` に旧 secret が直書きされたまま放置されていることがある。`run.sh` 経由でなく直接 `docker compose up` で起動するとそれらが container に注入されてしまう (Secret Manager fetch が走らない)。

```bash
# 1. .env から旧 secret を排除 (§6 の grep で検出される行を全部削除)
nano ~/llm-memory-extention/.env

# 2. .env.bak / .env.old などのバックアップが残っていないか確認
ls -la ~/llm-memory-extention/.env*

# 3. run.sh 経由で再起動 (Secret Manager から fetch される)
cd ~/llm-memory-extention
./deploy/gce/run.sh down
./deploy/gce/run.sh up --build -d

# 4. 注入結果を確認 (§6-1 と同じ 2 段チェック)
#    キー揃いを確認 (値は完全 redact)
sudo docker inspect llm-memory-extention-server-1 \
  --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | grep -E '^(GOOGLE_OAUTH_CLIENT_ID|GOOGLE_OAUTH_CLIENT_SECRET|JWT_SIGNING_KEY_[^=]+)=' \
  | sed -E 's/^([^=]+)=.*/\1=<redacted>/'
#    旧値混入が無いか
sudo docker inspect llm-memory-extention-server-1 \
  --format '{{range .Config.Env}}{{println .}}{{end}}' \
  | grep -E '__FILL_ME__|MODEL_HAIKU|MODEL_SONNET|ANTHROPIC_API_KEY' \
  | sed -E 's/^([^=]+)=.*/\1=<旧値検出>/'
```

### サーバー起動時に「`no JWT_SIGNING_KEY_<kid> environment variable configured`」エラー
- Secret Manager に `jwt-signing-key-v1` が存在するか: `gcloud secrets list --filter='name~jwt-signing-key'`
- 値が空 / 改行入りでないか: `gcloud secrets versions access latest --secret=jwt-signing-key-v1 | wc -c` (base64 32 バイトなら 44 文字程度)
- container への env 注入を redact 付きで確認 (`config` は compose の最終解決後 yml を出力するため、値をそのまま grep すると端末に表示されてしまう):
  ```bash
  cd ~/llm-memory-extention
  ./deploy/gce/run.sh config | grep -E '^[[:space:]]+JWT_SIGNING_KEY' | sed -E 's/(:[[:space:]]*.{4}).+$/\1<redacted>/'
  ```
  (compose v2 の `config` 出力は yaml の `KEY: VALUE` 形式なので `:` 区切りで redact する。`=` 区切りで書くと secret が全表示されるので注意。)
- VM の SA に `roles/secretmanager.secretAccessor` が付いているか: `gcloud secrets get-iam-policy jwt-signing-key-v1`

### OAuth callback で `invalid_grant` / `redirect_uri mismatch`
- Google OAuth Console の Authorized redirect URIs と `${PUBLIC_URL}/oauth/callback/google` が一致するか確認
- nip.io domain は `.` を `-` に置換した形式 (例: `34.146.12.34` → `34-146-12-34.nip.io`)

### litestream が GCS に書き込めない
- VM の Instance SA が GCS bucket に `roles/storage.objectAdmin` を持つか: `gsutil iam get gs://${GCP_PROJECT_ID}-memory-backup`
- litestream ログ: `cd ~/llm-memory-extention && ./deploy/gce/run.sh logs litestream`

### Caddy が証明書を取得できない (`tls: no certificate found`)
- `tcp/80` が開放されているか (HTTP-01 challenge 用)
- DNS 確認 (nip.io が解決するか): `nslookup ${PUBLIC_DOMAIN}`
- caddy ログを確認: `cd ~/llm-memory-extention && ./deploy/gce/run.sh logs caddy | grep -E 'acme|tls'`

**最終手段** として caddy_data volume を削除すると ACME account + cert cache が消えて再発行が走る。ただし:

- Let's Encrypt の rate limit (`50 cert / domain / week` 等) に近い状況だと悪化する
- volume 名は compose project 名依存。本ガイドは `-p llm-memory-extention` で固定しているので `llm-memory-extention_caddy_data` のはずだが、念のため `sudo docker volume ls | grep caddy` で実名確認してから削除

```bash
cd ~/llm-memory-extention
./deploy/gce/run.sh down
sudo docker volume ls | grep caddy   # 実名確認 (`-p llm-memory-extention` 経由なら llm-memory-extention_caddy_data)
sudo docker volume rm <実名>          # 例: llm-memory-extention_caddy_data
./deploy/gce/run.sh up -d
```
