//! OpenShell gateway admission hook.
//!
//! * `launch -- <gateway> [args]`: container entrypoint. Starts the hook, then
//!   the gateway (see `launcher.rs`).
//! * `serve`: run only the hook (useful for local testing).

mod hook;
mod launcher;
mod paths;

/// Generated from the OpenShell protos pinned in `openshell.version`.
/// The module tree mirrors the protobuf packages because generated code
/// refers to other packages through relative `super::` paths.
#[allow(clippy::all, clippy::pedantic, dead_code)]
pub mod proto {
    pub mod v1 {
        tonic::include_proto!("openshell.v1");
    }
    pub mod datamodel {
        pub mod v1 {
            tonic::include_proto!("openshell.datamodel.v1");
        }
    }
    pub mod sandbox {
        pub mod v1 {
            tonic::include_proto!("openshell.sandbox.v1");
        }
    }
    pub mod options {
        pub mod v1 {
            tonic::include_proto!("openshell.options.v1");
        }
    }
    pub mod extension {
        pub mod v1 {
            tonic::include_proto!("openshell.extension.v1");
        }
    }
    pub mod gateway_interceptor {
        pub mod v1 {
            tonic::include_proto!("openshell.gateway_interceptor.v1");
        }
    }
}

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use tokio_stream::wrappers::TcpListenerStream;
use tracing_subscriber::EnvFilter;

use hook::{AdmissionHook, PathPrefixes};
use proto::gateway_interceptor::v1::gateway_interceptor_server::GatewayInterceptorServer;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args, Clone)]
struct HookArgs {
    /// Address the admission hook listens on.
    #[arg(long, env = "ADMISSION_HOOK_LISTEN", default_value = "127.0.0.1:12777")]
    listen: SocketAddr,

    /// Machine path prefix for Windows drives with the Docker driver
    /// (Docker Desktop): C:\x -> <prefix>/c/x
    #[arg(
        long,
        env = "ADMISSION_HOOK_DOCKER_PREFIX",
        default_value = "/run/desktop/mnt/host"
    )]
    docker_prefix: String,

    /// Machine path prefix for Windows drives with the Podman driver
    /// (Podman machine): C:\x -> <prefix>/c/x
    #[arg(long, env = "ADMISSION_HOOK_PODMAN_PREFIX", default_value = "/mnt")]
    podman_prefix: String,
}

impl HookArgs {
    fn hook(&self) -> AdmissionHook {
        AdmissionHook::new(PathPrefixes {
            docker: self.docker_prefix.clone(),
            podman: self.podman_prefix.clone(),
        })
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the admission hook, then the gateway command after `--`.
    Launch {
        #[command(flatten)]
        hook: HookArgs,

        /// Interceptor endpoint written into the gateway config
        /// [default: http://<listen address>]
        #[arg(long, env = "ADMISSION_HOOK_ENDPOINT")]
        endpoint: Option<String>,

        /// Do not touch the gateway config (register the interceptor yourself).
        #[arg(long, env = "ADMISSION_HOOK_SKIP_CONFIG_INJECTION")]
        skip_config_injection: bool,

        /// Directory for the generated gateway config.
        #[arg(
            long,
            env = "ADMISSION_HOOK_RUNTIME_DIR",
            default_value = "/tmp/openshell-admission-hook"
        )]
        runtime_dir: PathBuf,

        /// Interceptor RPC timeout written into the gateway config (10ms..30s).
        #[arg(long, env = "ADMISSION_HOOK_TIMEOUT", default_value = "2s")]
        timeout: String,

        /// Gateway program and arguments.
        #[arg(last = true, required = true, value_name = "GATEWAY_CMD")]
        command: Vec<OsString>,
    },
    /// Run only the admission hook.
    Serve {
        #[command(flatten)]
        hook: HookArgs,
    },
}

/// Serves the interceptor on an already-bound listener until it fails.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    hook: AdmissionHook,
) -> Result<(), tonic::transport::Error> {
    tonic::transport::Server::builder()
        .add_service(GatewayInterceptorServer::new(hook))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await
}

/// The gateway runs in the same container: a wildcard listen address is
/// reached through loopback.
fn default_endpoint(listen: SocketAddr) -> String {
    let mut addr = listen;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    format!("http://{addr}")
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("ADMISSION_HOOK_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let result = rt.block_on(async move {
        match cli.command {
            Cmd::Launch {
                hook,
                endpoint,
                skip_config_injection,
                runtime_dir,
                timeout,
                command,
            } => {
                let opts = launcher::LaunchOptions {
                    listen: hook.listen,
                    endpoint: endpoint.unwrap_or_else(|| default_endpoint(hook.listen)),
                    inject_config: !skip_config_injection,
                    runtime_dir,
                    timeout,
                    command,
                };
                launcher::run(opts, hook.hook()).await
            }
            Cmd::Serve { hook } => {
                let listener = tokio::net::TcpListener::bind(hook.listen).await?;
                tracing::info!(listen = %hook.listen, "admission hook listening");
                serve_listener(listener, hook.hook()).await?;
                Ok(0)
            }
        }
    });

    match result {
        Ok(code) => std::process::ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::ExitCode::from(1)
        }
    }
}
