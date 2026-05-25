#!/usr/bin/env bash
# GCE 上で docker compose を起動するラッパー。
#
# 役割:
# 1. Secret Manager から secret を fetch して、固定パス
#    `/dev/shm/llm-memory-secrets.env` (tmpfs、ホストの永続ディスクには書かれない)
#    に書き出す。これにより `.env` / git repo / VM の通常ファイルシステムに
#    secret 文字列を残さない。
# 2. `docker compose` を起動する。secret を container に渡す経路はこの script
#    ではなく `docker/docker-compose.yml` 側の `env_file:` ディレクティブ
#    (`/dev/shm/llm-memory-secrets.env` を 2 つめの env_file として参照) で完結する。
#    この script が compose に渡す `--env-file ../.env` は compose ファイル内の
#    `${LITESTREAM_BUCKET}` 等の variable interpolation 用で、container には渡らない。
# 3. 終了時 (シェル exit 時) に tmp ファイルを削除する (EXIT trap)。
#
# **secret の到達範囲 (Accepted Risk)**:
# `env_file:` で注入された値は Docker の container config として
# `/var/lib/docker/containers/<id>/config.v2.json` 等に永続化される。これは
# Docker daemon に到達できる主体 (root、`docker` グループメンバ、`/var/run/docker.sock`
# にアクセス可能なユーザ) なら `docker inspect` 等で参照できる。本 script は
# `.env` / repo / 通常ユーザのホームに secret を書かないことを保証するだけで、
# Docker daemon にアクセスできる主体 (= 実質 root 相当) からは依然見える。
# 完全 secret 化が必要なら別途 secret をファイル mount + entrypoint/app 側で
# 読み込む方式に切り替えること。Phase 1 (個人 / 自社運用、`sudo docker compose`
# を踏める = Docker daemon に到達できる人 = 実質 root) ではこのリスクを受容している
# (deploy/gce/README.md 「Accepted Risk」参照)。
#
# Usage (repo root `~/llm-memory-extention` から実行する想定):
#   ./deploy/gce/run.sh up --build -d
#   ./deploy/gce/run.sh logs -f --tail=200 server caddy
#   ./deploy/gce/run.sh down
#   ./deploy/gce/run.sh ps
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

# secret 取得が必要な compose subcommand を判定する。`down` / `logs` / `ps` 等の
# 既存 container を触るだけの操作で gcloud / Secret Manager に依存させると、
# SM が一時的に不可達のときに container を止められない等の運用事故になる。
# 新規 container を作る系 (`up`, `run`, `create`) と、env の最終解決後 yml を
# 出力する系 (`config`, `convert`) は secret が無いと結果が壊れる / 不完全になるので
# fetch 対象に含める。それ以外 (`down`, `logs`, `ps`, `top`, `kill`, `restart` 等) は skip。
NEED_SECRETS=false
case "${1:-}" in
  up|run|create|config|convert)
    NEED_SECRETS=true
    ;;
esac

if [[ "${NEED_SECRETS}" == "true" ]]; then
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
    # secret 値に改行 / CR が含まれていると tmp env ファイルが複数行に化けて
    # compose の env パースが壊れる (意図せず別 var を注入できる shape にもなる)。
    # fail-fast し、Secret 側を 1 行に直すよう促す。
    if [[ "${val}" == *$'\n'* || "${val}" == *$'\r'* ]]; then
      echo "ERROR: secret '${name}' contains a newline/CR. Rewrite it as a single line, e.g.:" >&2
      echo "  printf '%s' '<value-without-newline>' | gcloud secrets versions add ${name} --project=${PROJECT} --data-file=-" >&2
      exit 1
    fi
    # `=` を含む値も正しく扱うため printf '%s=%s' を使う。
    # 末尾改行 1 個だけ付ける (gcloud secrets versions access は値の末尾改行を
    # 削除して返すので 1 行)。
    printf '%s=%s\n' "${envvar}" "${val}" >>"${TMPENV}"
    echo "  ${envvar} (from ${name})"
  done
fi

# `config` / `convert` は解決後の compose 設定を stdout に出力するため、
# Secret Manager 由来の値もそのまま端末 / リダイレクト先 / CI ログ等に出る。
# うっかり叩いて漏らさないよう、subcommand を見て STDERR に警告を出す。
# (READ FILTER で値を redact しても良いが、grep/sed の組み合わせ次第になるので
# まずは「叩いた人に意識させる」運用ガードを入れる。README は redact 付きの
# grep を案内している。)
case "${1:-}" in
  config|convert)
    echo "WARNING: '$1' は env を解決した結果を stdout に出力します。Secret Manager 由来の値もそのまま表示されるため、redirect / CI ログ等に流さないよう注意してください。" >&2
    ;;
esac

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
