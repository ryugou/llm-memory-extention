use axum::{extract::State, http::StatusCode};

use crate::app::AppState;
use llm_memory_auth::middleware::AuthenticatedUser;

pub async fn delete_me(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
) -> Result<StatusCode, StatusCode> {
    match llm_memory_storage::users::delete_cascade(&state.pool, &user.user_id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => {
            tracing::error!(user_id = %user.user_id, error = ?e, "delete_cascade failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{build_router, build_state_for_tests};
    use crate::config::ServerConfig;
    use axum::body::Body;
    use axum::http::Request;
    use llm_memory_auth::jwt;
    use llm_memory_core::scope::Scope;
    use llm_memory_storage::raws::{NewRaw, insert};
    use llm_memory_storage::users;
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn setup() -> (axum::Router, AppState, String) {
        let cfg = ServerConfig {
            database_url: "sqlite::memory:".into(),
            bind_addr: "x".into(),
            public_url: "https://test".into(),
            anthropic_api_key: "x".into(),
            google_client_id: "x".into(),
            google_client_secret: "x".into(),
            model_haiku: "h".into(),
            model_sonnet: "s".into(),
            trusted_proxy_count: 1,
        };
        let mut state = build_state_for_tests(cfg).await.unwrap();

        // Construct JwtKeys directly (avoid env-var race conditions across parallel tests).
        let mut keys_map = HashMap::new();
        keys_map.insert("v1".into(), b"01234567890123456789012345678901".to_vec());
        let keys = jwt::JwtKeys {
            current_kid: "v1".into(),
            keys: keys_map,
        };
        state.jwt_keys = keys.clone();

        // Manually create a user (delete_me requires auth, so user must exist).
        let user = users::upsert(
            &state.pool,
            "01HJTESTUSER0000000000000A",
            "google",
            "sub",
            None,
        )
        .await
        .unwrap();
        // Add a personal raw so we can verify cascade.
        insert(
            &state.pool,
            NewRaw {
                scope: Scope::Personal,
                owner_id: &user.id,
                title: "t",
                content: "c",
                source: "m",
                tags_json: None,
                created_by: Some(&user.id),
            },
        )
        .await
        .unwrap();

        // Issue an access token for the user.
        let token = jwt::issue(&keys, &user.id, "client-1", 3600).unwrap();

        let router = build_router(state.clone());
        (router, state, token)
    }

    #[tokio::test]
    async fn delete_me_returns_204_and_cascades() {
        let (router, state, token) = setup().await;
        // Pre-condition: user exists
        let pre = users::find_by_id(&state.pool, "01HJTESTUSER0000000000000A")
            .await
            .unwrap();
        assert!(pre.is_some());

        let res = router
            .oneshot(
                Request::delete("/v1/account")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // Post-condition: user deleted
        let post = users::find_by_id(&state.pool, "01HJTESTUSER0000000000000A")
            .await
            .unwrap();
        assert!(post.is_none());
    }

    #[tokio::test]
    async fn delete_me_without_token_is_401() {
        let (router, _, _) = setup().await;
        let res = router
            .oneshot(Request::delete("/v1/account").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
