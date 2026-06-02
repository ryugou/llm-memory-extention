//! `SchemaProvisioner` の vegapunk gRPC 実装。
//!
//! 初回 OAuth callback で `authorization_server::callback_google` から呼ばれ、
//! 個人 schema (`user-{sub}`) と shared schema (`sivira-shared`) を idempotent
//! に作成する。`AlreadyExists` は成功扱いに丸めるので、同じユーザの再 sign-in
//! や複数ユーザによる shared schema 作成競合があっても OAuth フローは止まらない。
//!
//! template 名 (= 個人 schema を新規作成する際の vegapunk SchemaTemplate)
//! は `ServerConfig.default_schema_template` で env から指定する
//! (`VEGAPUNK_DEFAULT_SCHEMA_TEMPLATE`、default `discussion`)。

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
