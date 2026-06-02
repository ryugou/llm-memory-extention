//! JWT 認証 middleware (vegapunk-memory-server 用)。
//!
//! llm-memory-auth::middleware と同じく:
//! - Authorization: Bearer ヘッダの JWT を verify
//! - users テーブルに該当 sub が存在することを毎リクエスト確認 (= account 削除済の
//!   token を弾く)
//!
//! 唯一の差分は、参照する DB が `vegapunk-memory-storage` 配下になっていること。

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use axum_extra::{
    TypedHeader,
    headers::{Authorization, authorization::Bearer},
};
use sqlx::SqlitePool;

use crate::jwt::{self, JwtKeys};

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub client_id: String,
    /// users 行から取り出した vegapunk graph の tenant key。
    /// tool handler は backend gRPC 呼び出し時にこの値を必ず注入し、
    /// client が `schema` 引数で別 tenant を指定しても上書きできないようにする
    /// (cross-tenant guard)。
    pub vegapunk_schema: String,
}

/// 認証 middleware が必要とする state: JWT 鍵と、user 存在チェック用の DB pool。
/// JWT の signature/exp 検証だけでは account 削除後のトークンを弾けないため、
/// users 表に該当 id があることを毎リクエスト確認する。
#[derive(Clone)]
pub struct AuthState {
    pub jwt_keys: JwtKeys,
    pub pool: SqlitePool,
}

impl AuthState {
    pub fn new(jwt_keys: JwtKeys, pool: SqlitePool) -> Self {
        Self { jwt_keys, pool }
    }
}

/// axum middleware: requires a valid Bearer token AND that the user row still
/// exists. The user existence check ensures account 削除直後の bearer 利用を
/// 弾く (JWT signature/exp verification alone cannot do).
pub async fn require_auth(
    State(auth): State<AuthState>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = bearer.ok_or(StatusCode::UNAUTHORIZED)?.0;
    let claims =
        jwt::verify(&auth.jwt_keys, token.0.token()).map_err(|_| StatusCode::UNAUTHORIZED)?;
    // 削除済み user の token を弾く: vegapunk-memory-server で account 削除を
    // 実装した際、users 行が消えると次の API 呼び出しでこの query が None を
    // 返して 401 になる。
    //
    // 行が見つかったら vegapunk_schema を AuthenticatedUser に詰める — tool
    // handler の cross-tenant guard はこの値だけを信用する。
    let user = vegapunk_memory_storage::users::find_by_id(&auth.pool, &claims.sub)
        .await
        .map_err(|e| {
            tracing::error!(user_id = %claims.sub, error = ?e, "users::find_by_id failed in auth middleware");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    req.extensions_mut().insert(AuthenticatedUser {
        user_id: user.id,
        client_id: claims.client_id,
        vegapunk_schema: user.vegapunk_schema,
    });
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::{JwtKeys, issue};
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as AxumRequest;
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use std::collections::HashMap;
    use tower::ServiceExt;
    use vegapunk_memory_storage::pool::init_pool;
    use vegapunk_memory_storage::users;

    fn keys() -> JwtKeys {
        let mut m = HashMap::new();
        m.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        JwtKeys {
            current_kid: "v1".into(),
            keys: m,
        }
    }

    async fn auth_state_with_user(user_id: &str) -> AuthState {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        // vegapunk-memory-storage::users::insert は vegapunk_schema が必須。
        // middleware の test は schema 値そのものを使わないので固定値を渡す。
        users::insert(&pool, user_id, "google", "sub", None, "test-schema")
            .await
            .unwrap();
        AuthState::new(keys(), pool)
    }

    async fn auth_state_no_user() -> AuthState {
        let pool = init_pool("sqlite::memory:").await.unwrap();
        AuthState::new(keys(), pool)
    }

    async fn protected() -> &'static str {
        "ok"
    }

    /// Test handler that echoes the AuthenticatedUser's vegapunk_schema so
    /// tests can verify the middleware loaded the right tenant key into the
    /// request extensions.
    async fn protected_schema_echo(
        axum::Extension(user): axum::Extension<AuthenticatedUser>,
    ) -> String {
        user.vegapunk_schema
    }

    fn app(auth: AuthState) -> Router {
        Router::new()
            .route("/", get(protected))
            .route("/schema", get(protected_schema_echo))
            .route_layer(from_fn_with_state(auth, require_auth))
            .with_state(())
    }

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        let auth = auth_state_no_user().await;
        let res = app(auth)
            .oneshot(AxumRequest::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_token_passes_through() {
        let user_id = "01HJAUTHUSER000000000000001";
        let auth = auth_state_with_user(user_id).await;
        let token = issue(&auth.jwt_keys, user_id, "c1", 3600).unwrap();
        let res = app(auth)
            .oneshot(
                AxumRequest::get("/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_returns_401() {
        let auth = auth_state_no_user().await;
        let res = app(auth)
            .oneshot(
                AxumRequest::get("/")
                    .header("authorization", "Bearer not-a-jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_user_carries_user_row_schema() {
        // require_auth は users 行から vegapunk_schema を引いて
        // AuthenticatedUser.vegapunk_schema にセットする。tool handler の
        // cross-tenant guard はこの値だけを信用するので、列が正しく届くこと
        // をハンドラ経由で確認する。
        let user_id = "01HJAUTHUSER000000000000002";
        let pool = init_pool("sqlite::memory:").await.unwrap();
        users::insert(&pool, user_id, "google", "sub2", None, "tenant-xyz")
            .await
            .unwrap();
        let auth = AuthState::new(keys(), pool);
        let token = issue(&auth.jwt_keys, user_id, "c1", 3600).unwrap();
        let res = app(auth)
            .oneshot(
                AxumRequest::get("/schema")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"tenant-xyz");
    }

    #[tokio::test]
    async fn token_for_deleted_user_is_401() {
        // user 行が存在しないとき、JWT が正規でも middleware は 401 を返す
        // (account 削除直後の bearer 利用を弾く回帰)。
        let user_id = "01HJAUTHGHOST00000000000001";
        let auth = auth_state_no_user().await; // pool に user を作らない
        let token = issue(&auth.jwt_keys, user_id, "c1", 3600).unwrap();
        let res = app(auth)
            .oneshot(
                AxumRequest::get("/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "JWT for deleted user must be rejected"
        );
    }
}
