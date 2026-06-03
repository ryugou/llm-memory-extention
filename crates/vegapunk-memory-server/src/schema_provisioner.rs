//! `SchemaProvisioner` の vegapunk gRPC 実装。
//!
//! `authorization_server::callback_google` は **OAuth callback ごとに**
//! (新規ユーザだけでなく既存ユーザの再 sign-in でも) 個人 schema と
//! shared schema に対して `ensure_schema` を呼ぶ。ここで個人 schema は
//! callback が DB から取り出した `users.vegapunk_schema` の **実値**
//! ─ 新規 provision 時の典型は `user-{google_subject}` だが、既存行が
//! 別命名で保存されていればそのまま使われる (`find_or_provision` は
//! cross-tenant guard で既存値を上書きしない)。gRPC リクエスト自体は
//! 毎回送るが、本 impl は `AlreadyExists` を `Ok(())` に丸めるので結果は
//! idempotent ― 同じユーザの再 sign-in や複数ユーザによる shared schema
//! 作成競合があっても OAuth フローは止まらない。
//!
//! 設定 (`ServerConfig` 経由で env から読む):
//! - shared schema 名:    `VEGAPUNK_SHARED_SCHEMA_NAME` (default `sivira-shared`)
//! - 新規 schema の template: `VEGAPUNK_DEFAULT_SCHEMA_TEMPLATE` (default `discussion`)
//!
//! template 名は vegapunk 側で既存の `SchemaTemplate` 一覧に存在する必要が
//! ある (= `ListSchemaTemplates` で確認可能)。

use async_trait::async_trait;
use tonic::Code;

use vegapunk_client::GraphRagClient;
use vegapunk_client::graphrag::{CreateSchemaRequest, create_schema_request::Source};
use vegapunk_memory_auth::authorization_server::SchemaProvisioner;

#[derive(Debug, Clone)]
pub struct VegapunkSchemaProvisioner {
    client: GraphRagClient,
    template: String,
}

impl VegapunkSchemaProvisioner {
    pub fn new(client: GraphRagClient, template: String) -> Self {
        Self { client, template }
    }
}

#[async_trait]
impl SchemaProvisioner for VegapunkSchemaProvisioner {
    async fn ensure_schema(&self, name: &str) -> Result<(), String> {
        let req = CreateSchemaRequest {
            name: name.to_string(),
            source: Some(Source::Template(self.template.clone())),
        };
        let mut client = self.client.clone();
        match client.create_schema(req).await {
            Ok(_) => Ok(()),
            // `AlreadyExists` は idempotent path: 同じ schema を 2 回目以降に
            // ensure する典型ケース (= 既存ユーザの再 sign-in、shared schema
            // が既に作成済) なので成功扱いに丸める。それ以外は失敗を string
            // にして返し、上位 (callback_google) で warn + continue 判断させる。
            Err(status) if status.code() == Code::AlreadyExists => Ok(()),
            Err(status) => Err(format!(
                "CreateSchema({name}) failed: gRPC {:?}: {}",
                status.code(),
                status.message()
            )),
        }
    }
}
