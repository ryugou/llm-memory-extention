#!/usr/bin/env bash
# GCE 上で docker compose を起動するラッパー。
#
# 役割:
# 1. Secret Manager から secret を fetch して、固定パス
#    `/dev/shm/llm-memory-secrets.env` (tmpfs、永続層には残らない) に書き出す。
# 2. `docker compose` を起動する。secret を container に渡す経路はこの script
#    ではなく `docker/docker-compose.yml` 側の `env_file:` ディレクティブ
#    (`/dev/shm/llm-memory-secrets.env` を 2 つめの env_file として参照) で完結する。
#    この script が compose に渡す `--env-file ../.env` は compose ファイル内の
#    `${LITESTREAM_BUCKET}` 等の variable interpolation 用で、container には渡らない。
# 3. 終了時 (シェル exit 時) に tmp ファイルを削除する (EXIT trap)。
#
# Usage:
#   ./run.sh up --build -d
#   ./run.sh logs -f --tail=200 server caddy
#   ./run.sh down
#   ./run.sh ps
#
# 引数はそのまま `docker compose` に転送する。compose project 名と base env_file
# はこのスクリプト内で固定する。
#
# 前提:
# - VM の instance service account に各 secret の Secret Manager のシークレット
#   アクセサー (`roles/secretmanager.secretAccessor`) が付与されていること。
# - `gcloud` CLI が VM 上で利用可能で、instance SA で認証されていること
#   (gcloud sdk + Debian default 構成で OK)。
# - docker compose v2 が `sudo docker compose` で叩けること。

set -euo pipefail

# GCP project ID。環境変数で上書き可能、無ければ gcloud config から取得。
PROJECT="${GCP_PROJECT_ID:-$(gcloud config get-value project 2>/dev/null)}"
if [[ -z "${PROJECT}" || "${PROJECT}" == "(unset)" ]]; then
  echo "ERROR: GCP project が決まらない。GCP_PROJECT_ID を export するか" \
       "gcloud config set project <id> を実行してください。" >&2
  exit 1
fi

# secret-name : env-var-name のマッピング。
# JWT 鍵を rotation するときは `jwt-signing-key-v2:JWT_SIGNING_KEY_v2` を追加する。
SECRETS=(
  "google-oauth-client-id:GOOGLE_OAUTH_CLIENT_ID"
  "google-oauth-client-secret:GOOGLE_OAUTH_CLIENT_SECRET"
  "jwt-signing-key-v1:JWT_SIGNING_KEY_v1"
)

# tmpfs (/dev/shm) 上に env ファイルを作る。リブートで自動消滅。
# パスは固定: docker-compose.yml が `env_file:` で同じパスを参照しているため。
TMPENV="/dev/shm/llm-memory-secrets.env"
# 旧 run があれば消す (前回 trap 漏れ等)
rm -f "${TMPENV}"
# 作成 + 自分のみ読み書き
install -m 600 /dev/null "${TMPENV}"
trap 'rm -f "${TMPENV}"' EXIT

echo "Fetching secrets from Secret Manager (project=${PROJECT}) → ${TMPENV} ..."
for entry in "${SECRETS[@]}"; do
  name="${entry%%:*}"
  envvar="${entry##*:}"
  val=$(gcloud secrets versions access latest --secret="${name}" --project="${PROJECT}")
  # `=` を含む値も正しく扱うため printf '%s=%s' を使う。
  # 末尾改行 1 個だけ付ける (gcloud secrets versions access は値の末尾改行を
  # 削除して返すので 1 行)。
  printf '%s=%s\n' "${envvar}" "${val}" >>"${TMPENV}"
  echo "  ${envvar} (from ${name})"
done

# compose 起動。
# - container への env 注入はこの script ではなく docker-compose.yml の
#   `env_file:` セクションで完結する (`../.env` + `/dev/shm/llm-memory-secrets.env`
#   の 2 つを参照)。後者はこの script が直前に書いた tmp ファイル。
# - 下記 `--env-file ../.env` は compose ファイル内の `${LITESTREAM_BUCKET}` 等の
#   variable interpolation 用で、container には渡らない。tmp env file はここでは
#   渡さない (interpolation で参照されるのは非 secret の値だけのため不要)。
# - `exec` を使うと bash プロセスが置き換わって EXIT trap が発火しなくなるため、
#   fork して exit を待つ。`up -d` の場合は container 起動後すぐ戻ってきて
#   trap で tmp ファイルを削除。container は起動時に env を読み込み済みなので、
#   その後の削除は安全。`up` (foreground) や `logs -f` でも Ctrl-C で
#   compose 終了 → script 終了 → trap → 削除の順で確実に消える。
cd "$(dirname "$0")/../../docker"
sudo docker compose \
  -p llm-memory-extention \
  --env-file ../.env \
  "$@"
