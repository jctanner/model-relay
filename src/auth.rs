use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::ApiError;

#[derive(Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(skip)]
    pub project_epoch: String,
    pub client_id: String,
    pub key_id: String,
    pub sha256: String,
    pub models: Vec<String>,
    pub backends: Vec<String>,
    /// Allows Messages models present in the upstream catalog, not arbitrary IDs.
    #[serde(default)]
    pub catalog_models: bool,
    /// Allows Vertex routing to models present in the upstream catalog.
    #[serde(default)]
    pub vertex_catalog_models: bool,
    /// Explicitly permits this identity to acquire synthetic metadata tokens.
    #[serde(default)]
    pub mock_adc: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub keys: Vec<Key>,
}

#[derive(Clone, serde::Serialize)]
pub struct Identity {
    pub upstream_profile: String,
    pub project_id: String,
    pub client_id: String,
    pub key_id: String,
    pub scheme: &'static str,
}

pub fn fingerprint(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

pub fn matches(key: &str, verifier: &str) -> bool {
    fingerprint(key)
        .as_bytes()
        .ct_eq(verifier.as_bytes())
        .into()
}

fn credential(headers: &HeaderMap) -> Result<(&str, &'static str), ApiError> {
    for name in ["authorization", "x-api-key"] {
        if headers.get_all(name).iter().count() > 1 {
            return Err(ApiError::bad("Duplicate credential headers"));
        }
    }
    let bearer = headers
        .get("authorization")
        .map(|v| {
            let value = v.to_str().map_err(|_| ApiError::unauthorized())?;
            let (scheme, key) = value.split_once(' ').ok_or_else(ApiError::unauthorized)?;
            if !scheme.eq_ignore_ascii_case("bearer") || key.is_empty() {
                return Err(ApiError::unauthorized());
            }
            Ok(key)
        })
        .transpose()?;
    let api_key = headers
        .get("x-api-key")
        .map(|v| v.to_str().map_err(|_| ApiError::unauthorized()))
        .transpose()?;
    match (bearer, api_key) {
        (Some(a), Some(b)) if a != b => Err(ApiError::bad("Conflicting credential headers")),
        (Some(key), _) => Ok((key, "bearer")),
        (_, Some(key)) if !key.is_empty() => Ok((key, "x-api-key")),
        _ => Err(ApiError::unauthorized()),
    }
}

pub fn authenticate(
    headers: &HeaderMap,
    registry: &Registry,
    model: &str,
    backend: &str,
    adc: Option<&crate::mock_adc::MockAdc>,
) -> Result<Identity, ApiError> {
    let (key, identity) = authenticate_key(headers, registry, backend, adc)?;
    if !key.models.iter().any(|m| m == model) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model/backend access denied",
        ));
    }
    Ok(identity)
}

pub fn authenticate_key<'a>(
    headers: &HeaderMap,
    registry: &'a Registry,
    backend: &str,
    adc: Option<&crate::mock_adc::MockAdc>,
) -> Result<(&'a Key, Identity), ApiError> {
    let (value, scheme) = credential(headers)?;
    let issued = adc.map(|a| a.resolve(value)).transpose()?.flatten();
    if issued.as_ref().is_some_and(|binding| {
        registry
            .keys
            .iter()
            .filter(|key| key.key_id == binding.key_id)
            .count()
            != 1
    }) {
        return Err(ApiError::unauthorized());
    }
    let key = registry
        .keys
        .iter()
        .find(|key| match &issued {
            Some(binding) => {
                scheme == "bearer"
                    && key.mock_adc
                    && key.client_id == binding.client_id
                    && key.key_id == binding.key_id
                    && key.project_id == binding.project_id
                    && key.project_epoch == binding.project_epoch
                    && key
                        .sha256
                        .as_bytes()
                        .ct_eq(binding.verifier.as_bytes())
                        .into()
            }
            None => matches(value, &key.sha256),
        })
        .ok_or_else(ApiError::unauthorized)?;
    if !backend.is_empty() && !key.backends.iter().any(|b| b == backend) {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "permission_error",
            "Model/backend access denied",
        ));
    }
    Ok((
        key,
        Identity {
            upstream_profile: "default".into(),
            project_id: key
                .project_id
                .clone()
                .unwrap_or_else(|| crate::projects::DEFAULT_PROJECT.into()),
            client_id: key.client_id.clone(),
            key_id: key.key_id.clone(),
            scheme: if issued.is_some() { "mock-adc" } else { scheme },
        },
    ))
}

pub fn admin(headers: &HeaderMap, verifier: &str) -> Result<(), ApiError> {
    let (key, scheme) = credential(headers)?;
    if scheme != "bearer" || !matches(key, verifier) {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_keys_conflicts_and_grants() {
        let registry = Registry {
            keys: vec![Key {
                project_id: None,
                project_epoch: String::new(),
                client_id: "client".into(),
                key_id: "key".into(),
                sha256: fingerprint("short"),
                models: vec!["model".into()],
                backends: vec!["mock".into()],
                catalog_models: false,
                vertex_catalog_models: false,
                mock_adc: false,
            }],
        };
        let mut headers = HeaderMap::new();
        assert_eq!(
            authenticate(&headers, &registry, "model", "mock", None)
                .err()
                .unwrap()
                .0,
            StatusCode::UNAUTHORIZED
        );
        headers.insert("x-api-key", "short".parse().unwrap());
        assert_eq!(
            authenticate(&headers, &registry, "model", "mock", None)
                .unwrap()
                .scheme,
            "x-api-key"
        );
        assert_eq!(
            authenticate(&headers, &registry, "other", "mock", None)
                .err()
                .unwrap()
                .0,
            StatusCode::FORBIDDEN
        );
        headers.insert("authorization", "Bearer different".parse().unwrap());
        assert_eq!(
            authenticate(&headers, &registry, "model", "mock", None)
                .err()
                .unwrap()
                .0,
            StatusCode::BAD_REQUEST
        );
        headers.insert("authorization", "Bearer short".parse().unwrap());
        assert!(authenticate(&headers, &registry, "model", "mock", None).is_ok());
        assert!(admin(&headers, &fingerprint("separate-admin")).is_err());
        assert!(authenticate(&headers, &Registry { keys: vec![] }, "model", "mock", None).is_err());
    }
}
