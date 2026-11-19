//! `GatewayInterceptor` gRPC service.
//!
//! Bound to `openshell.v1.OpenShell/CreateSandbox` in the `modify_operation`
//! phase. The gateway hands us the proposed `CreateSandboxRequest` as
//! protobuf-JSON (camelCase field names). Caller supplied driver config lives
//! in `spec.template.driverConfig.<driver>` as a free-form Struct; for the
//! `docker` and `podman` drivers we look at `mounts[*]` entries of
//! `type = "bind"` and rewrite Windows `source` paths with RFC 6902 `replace`
//! patches.

use serde_json::Value as Json;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::paths::convert_windows_path;
use crate::proto::extension::v1::{PeerMetadata, ProtocolVersion};
use crate::proto::gateway_interceptor::v1::{
    DescribeRequest, GatewayInterceptorPhase, InterceptorBinding, InterceptorEvaluation,
    InterceptorManifest, InterceptorResult, InterceptorSelector, JsonPatch,
    ProviderProfileSnapshot, ProviderProfileSnapshotRequest,
    gateway_interceptor_server::GatewayInterceptor, interceptor_evaluation::Phase,
};

pub const INTERCEPTOR_NAME: &str = "windows-mount-admission";
pub const CREATE_SANDBOX_RPC: &str = "openshell.v1.OpenShell/CreateSandbox";
const OPENSHELL_SERVICE: &str = "openshell.v1.OpenShell";
const CREATE_SANDBOX_METHOD: &str = "CreateSandbox";
const BINDING_ID: &str = "create-sandbox-windows-mounts";
const PROTOCOL_MAJOR: u32 = 1;
const PROTOCOL_MINOR: u32 = 0;
const CONTRACT_CAPABILITY: &str = "openshell.gateway-interceptor.contract";
const DRIVER_CONFIG_POINTER: &str = "/spec/template/driverConfig";

/// Machine path prefix for each supported compute driver.
#[derive(Debug, Clone)]
pub struct PathPrefixes {
    pub docker: String,
    pub podman: String,
}

impl PathPrefixes {
    fn drivers(&self) -> [(&'static str, &str); 2] {
        [("docker", &self.docker), ("podman", &self.podman)]
    }
}

#[derive(Debug, Clone)]
pub struct AdmissionHook {
    prefixes: PathPrefixes,
}

impl AdmissionHook {
    pub fn new(prefixes: PathPrefixes) -> Self {
        Self { prefixes }
    }

    /// Computes the patches for one proposed CreateSandbox operation.
    /// Returns `Err(reason)` when a Windows path cannot be mapped.
    pub fn mount_patches(&self, operation: &Json) -> Result<Vec<(String, String, String)>, String> {
        let mut out = Vec::new();
        for (driver, prefix) in self.prefixes.drivers() {
            let base = format!("{DRIVER_CONFIG_POINTER}/{driver}/mounts");
            let Some(mounts) = operation.pointer(&base).and_then(Json::as_array) else {
                continue;
            };
            for (index, mount) in mounts.iter().enumerate() {
                if mount.get("type").and_then(Json::as_str) != Some("bind") {
                    continue;
                }
                let Some(source) = mount.get("source").and_then(Json::as_str) else {
                    continue;
                };
                match convert_windows_path(source, prefix) {
                    Ok(Some(converted)) if converted != source => {
                        out.push((
                            format!("{base}/{index}/source"),
                            source.to_string(),
                            converted,
                        ));
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("driverConfig.{driver}.mounts[{index}]: {e}")),
                }
            }
        }
        Ok(out)
    }

    fn evaluate_create_sandbox(&self, operation: &Json, mutate: bool) -> InterceptorResult {
        let rewrites = match self.mount_patches(operation) {
            Ok(r) => r,
            Err(reason) => {
                warn!(%reason, "denying CreateSandbox");
                return deny("INVALID_ARGUMENT", reason);
            }
        };
        if rewrites.is_empty() {
            return allow();
        }
        if !mutate {
            // validate phase: a Windows path survived modify_operation.
            let (path, source, _) = &rewrites[0];
            return deny(
                "INVALID_ARGUMENT",
                format!("bind mount source {source:?} at {path} is still a Windows path"),
            );
        }
        let mut result = allow();
        for (path, source, converted) in &rewrites {
            info!(%path, from = %source, to = %converted, "rewriting bind mount source");
            result.patches.push(JsonPatch {
                op: "replace".into(),
                path: path.clone(),
                value: Some(prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(converted.clone())),
                }),
                from: String::new(),
            });
        }
        result
            .log_annotations
            .insert("rewritten_bind_mounts".into(), rewrites.len().to_string());
        result
    }
}

fn allow() -> InterceptorResult {
    InterceptorResult {
        allowed: true,
        ..Default::default()
    }
}

fn deny(code: &str, reason: String) -> InterceptorResult {
    InterceptorResult {
        allowed: false,
        reason,
        status_code: code.into(),
        ..Default::default()
    }
}

pub fn extension_metadata() -> PeerMetadata {
    PeerMetadata {
        protocol_version: Some(ProtocolVersion {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
        }),
        implementation_name: "openkaiden/openshell-admission-hook".into(),
        implementation_version: env!("CARGO_PKG_VERSION").into(),
        supported_capabilities: vec![CONTRACT_CAPABILITY.into()],
        required_capabilities: vec![CONTRACT_CAPABILITY.into()],
    }
}

fn validate_gateway_metadata(gateway: Option<&PeerMetadata>) -> Result<(), Status> {
    let gateway = gateway.ok_or_else(|| {
        Status::failed_precondition("gateway did not send protocol metadata; upgrade the gateway")
    })?;
    let version = gateway
        .protocol_version
        .as_ref()
        .ok_or_else(|| Status::failed_precondition("gateway metadata has no protocol version"))?;
    if version.major != PROTOCOL_MAJOR {
        return Err(Status::failed_precondition(format!(
            "gateway interceptor protocol {}.{} is incompatible with {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}",
            version.major, version.minor
        )));
    }
    let ours = extension_metadata();
    let missing: Vec<_> = gateway
        .required_capabilities
        .iter()
        .filter(|c| !ours.supported_capabilities.contains(c))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(Status::failed_precondition(format!(
            "gateway requires unsupported capabilities: {}",
            missing.join(", ")
        )));
    }
    if !gateway
        .supported_capabilities
        .iter()
        .any(|c| c == CONTRACT_CAPABILITY)
    {
        return Err(Status::failed_precondition(format!(
            "gateway does not advertise {CONTRACT_CAPABILITY}"
        )));
    }
    Ok(())
}

#[tonic::async_trait]
impl GatewayInterceptor for AdmissionHook {
    async fn describe(
        &self,
        request: Request<DescribeRequest>,
    ) -> Result<Response<InterceptorManifest>, Status> {
        let gateway = request.into_inner().gateway;
        validate_gateway_metadata(gateway.as_ref())?;
        if let Some(g) = &gateway {
            info!(
                gateway = %g.implementation_name,
                version = %g.implementation_version,
                "gateway connected (Describe)"
            );
        }
        Ok(Response::new(InterceptorManifest {
            name: INTERCEPTOR_NAME.into(),
            bindings: vec![InterceptorBinding {
                id: BINDING_ID.into(),
                selector: Some(InterceptorSelector {
                    rpc: CREATE_SANDBOX_RPC.into(),
                    ..Default::default()
                }),
                phases: vec![GatewayInterceptorPhase::ModifyOperation as i32],
                failure_policy: "fail_closed".into(),
            }],
            failure_policy: "fail_closed".into(),
            provider_profiles: false,
            expected_audience: String::new(),
            extension: Some(extension_metadata()),
        }))
    }

    /// Only called when the manifest sets `provider_profiles = true`, which
    /// this hook never does.
    async fn snapshot_provider_profiles(
        &self,
        _request: Request<ProviderProfileSnapshotRequest>,
    ) -> Result<Response<ProviderProfileSnapshot>, Status> {
        Err(Status::unimplemented(
            "this interceptor does not provide provider profiles",
        ))
    }

    async fn evaluate(
        &self,
        request: Request<InterceptorEvaluation>,
    ) -> Result<Response<InterceptorResult>, Status> {
        let eval = request.into_inner();
        debug!(service = %eval.service, method = %eval.method, binding = %eval.binding_id, "Evaluate");
        if eval.service != OPENSHELL_SERVICE || eval.method != CREATE_SANDBOX_METHOD {
            return Ok(Response::new(allow()));
        }
        let result = match eval.phase {
            Some(Phase::ModifyOperation(p)) => {
                let op = p
                    .proposed_operation
                    .map(struct_to_json)
                    .unwrap_or(Json::Null);
                self.evaluate_create_sandbox(&op, true)
            }
            Some(Phase::Validate(p)) => {
                let op = p
                    .proposed_operation
                    .map(struct_to_json)
                    .unwrap_or(Json::Null);
                self.evaluate_create_sandbox(&op, false)
            }
            Some(Phase::PostCommit(_)) | None => allow(),
        };
        Ok(Response::new(result))
    }
}

// ---- google.protobuf.Struct <-> serde_json ----------------------------------

pub fn struct_to_json(s: prost_types::Struct) -> Json {
    Json::Object(
        s.fields
            .into_iter()
            .map(|(k, v)| (k, value_to_json(v)))
            .collect(),
    )
}

fn value_to_json(v: prost_types::Value) -> Json {
    use prost_types::value::Kind;
    match v.kind {
        None | Some(Kind::NullValue(_)) => Json::Null,
        Some(Kind::BoolValue(b)) => Json::Bool(b),
        Some(Kind::NumberValue(n)) => {
            serde_json::Number::from_f64(n).map_or(Json::Null, Json::Number)
        }
        Some(Kind::StringValue(s)) => Json::String(s),
        Some(Kind::ListValue(l)) => Json::Array(l.values.into_iter().map(value_to_json).collect()),
        Some(Kind::StructValue(s)) => struct_to_json(s),
    }
}

#[cfg(test)]
pub fn json_to_struct(j: Json) -> prost_types::Struct {
    match json_to_value(j).kind {
        Some(prost_types::value::Kind::StructValue(s)) => s,
        _ => prost_types::Struct {
            fields: std::collections::BTreeMap::new(),
        },
    }
}

#[cfg(test)]
fn json_to_value(j: Json) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match j {
        Json::Null => Kind::NullValue(0),
        Json::Bool(b) => Kind::BoolValue(b),
        Json::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or_default()),
        Json::String(s) => Kind::StringValue(s),
        Json::Array(a) => Kind::ListValue(prost_types::ListValue {
            values: a.into_iter().map(json_to_value).collect(),
        }),
        Json::Object(o) => Kind::StructValue(prost_types::Struct {
            fields: o.into_iter().map(|(k, v)| (k, json_to_value(v))).collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gateway_interceptor::v1::ModifyOperationEvaluation;
    use serde_json::json;

    fn hook() -> AdmissionHook {
        AdmissionHook::new(PathPrefixes {
            docker: "/run/desktop/mnt/host".into(),
            podman: "/mnt".into(),
        })
    }

    fn eval(op: Json) -> InterceptorEvaluation {
        InterceptorEvaluation {
            interceptor_name: INTERCEPTOR_NAME.into(),
            binding_id: BINDING_ID.into(),
            service: OPENSHELL_SERVICE.into(),
            method: CREATE_SANDBOX_METHOD.into(),
            principal: Default::default(),
            phase: Some(Phase::ModifyOperation(ModifyOperationEvaluation {
                proposed_operation: Some(json_to_struct(op)),
            })),
        }
    }

    async fn run(op: Json) -> InterceptorResult {
        hook()
            .evaluate(Request::new(eval(op)))
            .await
            .unwrap()
            .into_inner()
    }

    fn patch_value(p: &JsonPatch) -> &str {
        match p.value.as_ref().and_then(|v| v.kind.as_ref()) {
            Some(prost_types::value::Kind::StringValue(s)) => s,
            other => panic!("unexpected patch value {other:?}"),
        }
    }

    #[tokio::test]
    async fn rewrites_docker_and_podman_bind_sources() {
        let op = json!({
            "name": "demo",
            "spec": { "template": { "image": "ubuntu", "driverConfig": {
                "docker": { "mounts": [
                    { "type": "volume", "source": "work", "target": "/sandbox/work" },
                    { "type": "bind", "source": "C:\\Users\\me\\proj", "target": "/sandbox/proj" }
                ]},
                "podman": { "mounts": [
                    { "type": "bind", "source": "D:/data", "target": "/data", "read_only": false },
                    { "type": "bind", "source": "/already/linux", "target": "/x" }
                ]}
            }}}
        });
        let r = run(op).await;
        assert!(r.allowed);
        assert_eq!(r.patches.len(), 2);
        assert_eq!(r.patches[0].op, "replace");
        assert_eq!(
            r.patches[0].path,
            "/spec/template/driverConfig/docker/mounts/1/source"
        );
        assert_eq!(
            patch_value(&r.patches[0]),
            "/run/desktop/mnt/host/c/Users/me/proj"
        );
        assert_eq!(
            r.patches[1].path,
            "/spec/template/driverConfig/podman/mounts/0/source"
        );
        assert_eq!(patch_value(&r.patches[1]), "/mnt/d/data");
        assert_eq!(
            r.log_annotations
                .get("rewritten_bind_mounts")
                .map(String::as_str),
            Some("2")
        );
    }

    #[tokio::test]
    async fn allows_requests_without_driver_config() {
        let r = run(json!({ "spec": { "template": { "image": "ubuntu" } } })).await;
        assert!(r.allowed);
        assert!(r.patches.is_empty());
        let r = run(json!({})).await;
        assert!(r.allowed);
    }

    #[tokio::test]
    async fn denies_unmappable_paths() {
        let op = json!({ "spec": { "template": { "driverConfig": { "podman": { "mounts": [
            { "type": "bind", "source": "\\\\server\\share", "target": "/x" }
        ]}}}}});
        let r = run(op).await;
        assert!(!r.allowed);
        assert_eq!(r.status_code, "INVALID_ARGUMENT");
        assert!(r.reason.contains("podman.mounts[0]"), "{}", r.reason);
    }

    #[tokio::test]
    async fn ignores_other_rpcs() {
        let mut e = eval(
            json!({ "spec": { "template": { "driverConfig": { "podman": { "mounts": [
                { "type": "bind", "source": "\\\\server\\share", "target": "/x" }
            ]}}}}}),
        );
        e.method = "UpdateConfig".into();
        let r = hook().evaluate(Request::new(e)).await.unwrap().into_inner();
        assert!(r.allowed);
    }

    #[tokio::test]
    async fn describe_negotiates_protocol() {
        let gateway = PeerMetadata {
            protocol_version: Some(ProtocolVersion { major: 1, minor: 3 }),
            implementation_name: "openshell/gateway".into(),
            implementation_version: "test".into(),
            supported_capabilities: vec![CONTRACT_CAPABILITY.into()],
            required_capabilities: vec![CONTRACT_CAPABILITY.into()],
        };
        let m = hook()
            .describe(Request::new(DescribeRequest {
                gateway: Some(gateway.clone()),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(m.bindings.len(), 1);
        assert_eq!(
            m.bindings[0].selector.as_ref().unwrap().rpc,
            CREATE_SANDBOX_RPC
        );
        assert_eq!(
            m.bindings[0].phases,
            vec![GatewayInterceptorPhase::ModifyOperation as i32]
        );
        assert!(
            m.extension
                .unwrap()
                .supported_capabilities
                .contains(&CONTRACT_CAPABILITY.to_string())
        );

        let mut bad = gateway.clone();
        bad.protocol_version = Some(ProtocolVersion { major: 2, minor: 0 });
        assert!(
            hook()
                .describe(Request::new(DescribeRequest { gateway: Some(bad) }))
                .await
                .is_err()
        );
        assert!(
            hook()
                .describe(Request::new(DescribeRequest { gateway: None }))
                .await
                .is_err()
        );
        let mut needs_more = gateway;
        needs_more
            .required_capabilities
            .push("openshell.future.thing".into());
        assert!(
            hook()
                .describe(Request::new(DescribeRequest {
                    gateway: Some(needs_more)
                }))
                .await
                .is_err()
        );
    }
}
