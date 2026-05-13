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

use crate::jwt::{self, JwtKeys};

#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    pub user_id: String,
    pub client_id: String,
}

/// axum middleware: requires a valid Bearer token. Extracts user_id/client_id
/// into request extensions for downstream handlers.
pub async fn require_auth(
    State(keys): State<JwtKeys>,
    bearer: Option<TypedHeader<Authorization<Bearer>>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token = bearer.ok_or(StatusCode::UNAUTHORIZED)?.0;
    let claims = jwt::verify(&keys, token.0.token()).map_err(|_| StatusCode::UNAUTHORIZED)?;
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

    async fn protected() -> &'static str {
        "ok"
    }

    fn app(k: JwtKeys) -> Router {
        Router::new()
            .route("/", get(protected))
            .route_layer(from_fn_with_state(k.clone(), require_auth))
            .with_state(())
    }

    #[tokio::test]
    async fn missing_bearer_returns_401() {
        let k = keys();
        let res = app(k)
            .oneshot(AxumRequest::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_token_passes_through() {
        let k = keys();
        let token = issue(&k, "u1", "c1", 3600).unwrap();
        let res = app(k)
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
        let k = keys();
        let res = app(k)
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
}
