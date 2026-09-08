use wild_agent_os_core::api::grpc::server::seapp::se_kernel_service_server::SeKernelServiceServer;
use wild_agent_os_core::api::grpc::server::AgentOSService;
use wild_agent_os_core::config::settings::Settings;
use wild_agent_os_core::utils::data_paths::migrate_legacy_home_data;
use wild_agent_os_core::utils::init_logging;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const SHUTDOWN_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut interrupt = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        let mut terminate = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = interrupt.recv() => tracing::info!("received SIGINT"),
            _ = terminate.recv() => tracing::info!("received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
        tracing::info!("received Ctrl-C");
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Err(error) = wild_agent_os_core::api::http::iam::validate_startup_auth_configuration() {
        eprintln!("Authentication configuration error: {}", error);
        std::process::exit(1);
    }

    if let Err(e) = migrate_legacy_home_data() {
        eprintln!("Warning: legacy data directory migration skipped: {}", e);
    }

    let settings = match Settings::load() {
        Ok(settings) => settings,
        Err(error) if Settings::development_config_fallback_enabled() => {
            eprintln!(
                "Development configuration fallback enabled; failed to load configuration ({error}), using defaults"
            );
            Settings::default()
        }
        Err(error) => {
            eprintln!("Configuration load error: {error}");
            eprintln!(
                "Refusing to start with defaults. Fix the configuration, or explicitly set \
                 AGENT_OS_CONFIG_PROFILE=development or AGENT_OS_ALLOW_DEFAULT_CONFIG=true \
                 for local development."
            );
            std::process::exit(1);
        }
    };

    let _logging_guard = init_logging(&settings.logging);

    if let Err(e) = settings.validate() {
        eprintln!("Configuration error: {}", e);
        eprintln!("Please set AGENT_OS_GATEWAY_API_KEY or configure config.yaml");
        std::process::exit(1);
    }

    std::fs::create_dir_all(&settings.output.directory)?;
    std::fs::create_dir_all(&settings.memory.l0.path)?;

    let addr = settings
        .api
        .grpc_addr
        .parse()
        .unwrap_or_else(|_| "[::1]:50051".parse().expect("default addr parse"));
    let shutdown = CancellationToken::new();
    let agent_os_service = AgentOSService::new_with_shutdown(settings, shutdown.clone())
        .map_err(Box::<dyn std::error::Error>::from)?;

    // async initialize BatchAgent system (register agents, start triggers)
    agent_os_service.init_batch_system().await;

    // mount existing axum HTTP/SSE routes (build_router) alongside gRPC, sharing runtime state
    let http_router = agent_os_service.build_http_router();
    let http_port: u16 = std::env::var("AGENT_OS_HTTP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);
    let http_addr = std::net::SocketAddr::from(([0, 0, 0, 0], http_port));
    let mut servers = JoinSet::new();
    let http_shutdown = shutdown.clone();
    servers.spawn(async move {
        match tokio::net::TcpListener::bind(http_addr).await {
            Ok(listener) => {
                tracing::info!("Agent OS HTTP/SSE server starting on {}", http_addr);
                if let Err(e) = axum::serve(listener, http_router)
                    .with_graceful_shutdown(http_shutdown.cancelled_owned())
                    .await
                {
                    tracing::error!("HTTP server error: {}", e);
                    Err(e.to_string())
                } else {
                    Ok(())
                }
            }
            Err(e) => {
                tracing::error!("Failed to bind HTTP server on {}: {}", http_addr, e);
                Err(e.to_string())
            }
        }
    });

    tracing::info!("Agent OS gRPC server starting on {}", addr);

    let grpc_shutdown = shutdown.clone();
    servers.spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SeKernelServiceServer::new(agent_os_service))
            .serve_with_shutdown(addr, grpc_shutdown.cancelled_owned())
            .await
            .map_err(|error| error.to_string())
    });

    tokio::select! {
        _ = wait_for_shutdown_signal() => {}
        result = servers.join_next() => {
            tracing::error!(?result, "server exited before a shutdown signal");
        }
    }

    tracing::info!(
        timeout_secs = SHUTDOWN_DRAIN_TIMEOUT.as_secs(),
        "starting graceful shutdown"
    );
    shutdown.cancel();
    if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, async {
        while let Some(result) = servers.join_next().await {
            if let Err(error) = result {
                tracing::warn!(?error, "server task failed during shutdown");
            }
        }
    })
    .await
    .is_err()
    {
        tracing::warn!("shutdown drain timed out; aborting remaining server tasks");
        servers.abort_all();
        while servers.join_next().await.is_some() {}
    }

    Ok(())
}
