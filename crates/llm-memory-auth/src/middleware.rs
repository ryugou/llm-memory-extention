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
/// exists. The user existence check ensures `DELETE /v1/account` immediately
/// invalidates outstanding access tokens (which JWT signature/exp verification
/// alone cannot do).
pub async fn require_auth(
    State(auth): State<AuthState>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = bearer.ok_or(StatusCode::UNAUTHORIZED)?.0;
    let claims =
        jwt::verify(&auth.jwt_keys, token.0.token()).map_err(|_| StatusCode::UNAUTHORIZED)?;
    // 削除済み user の token を弾く: account.delete_cascade で users 行が
    // 消えると、次の API 呼び出しでこの query が None を返して 401 になる。
    let user_exists = llm_memory_storage::users::find_by_id(&auth.pool, &claims.sub)
        .await
        .map_err(|e| {
            tracing::error!(user_id = %claims.sub, error = ?e, "users::find_by_id failed in auth middleware");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .is_some();
    if !user_exists {
        return Err(StatusCode::UNAUTHORIZED);
    }
    req.extensions_mut().insert(AuthenticatedUser {
        user_id: claims.sub,
        client_id: claims.client_id,
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
    use llm_memory_storage::pool::init_pool;
    use llm_memory_storage::users;
    use std::collections::HashMap;
    use tower::ServiceExt;

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
        users::upsert(&pool, user_id, "google", "sub", None)
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

    fn app(auth: AuthState) -> Router {
        Router::new()
            .route("/", get(protected))
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
    async fn token_for_deleted_user_is_401() {
        // user 行が存在しないとき、JWT が正規でも middleware は 401 を返す
        // (DELETE /v1/account でアカウントを消した直後の bearer 利用を弾く回帰)。
        let user_id = "01HJAUTHGHOST00000000000001";
        let auth = auth_state_no_user().await; // pool に user を作らない
        // でも JWT は valid なものを発行する
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
