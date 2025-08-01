// Copyright 2025 OpenObserve Inc.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{
    cmp::max,
    collections::HashMap,
    net::SocketAddr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use actix_web::{App, HttpServer, dev::ServerHandle, http::KeepAlive, middleware, web};
use actix_web_lab::middleware::from_fn;
use actix_web_opentelemetry::RequestTracing;
use arrow_flight::flight_service_server::FlightServiceServer;
use config::{get_config, utils::size::bytes_to_human_readable};
use log::LevelFilter;
#[cfg(feature = "enterprise")]
use openobserve::handler::http::{
    auth::script_server::validator as script_server_validator, request::script_server,
};
use openobserve::{
    cli::basic::cli,
    common::{
        infra::{self as common_infra, cluster},
        meta,
        utils::zo_logger,
    },
    handler::{
        grpc::{
            auth::check_auth,
            flight::FlightServiceImpl,
            request::{
                event::Eventer,
                ingest::Ingester,
                logs::LogsServer,
                metrics::{ingester::MetricsIngester, querier::MetricsQuerier},
                query_cache::QueryCacheServerImpl,
                stream::StreamServiceImpl,
                traces::TraceServer,
            },
        },
        http::router::*,
    },
    job, migration, router,
    service::{
        cluster_info::ClusterInfoService, db, metadata, node::NodeService, search::SEARCH_SERVER,
        self_reporting, tls::http_tls_config,
    },
};
use opentelemetry::{KeyValue, global, trace::TracerProvider};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_proto::tonic::collector::{
    logs::v1::logs_service_server::LogsServiceServer,
    metrics::v1::metrics_service_server::MetricsServiceServer,
    trace::v1::trace_service_server::TraceServiceServer,
};
use opentelemetry_sdk::{Resource, propagation::TraceContextPropagator};
use proto::cluster_rpc::{
    cluster_info_service_server::ClusterInfoServiceServer, event_server::EventServer,
    ingest_server::IngestServer, metrics_server::MetricsServer,
    node_service_server::NodeServiceServer, query_cache_server::QueryCacheServer,
    search_server::SearchServer, streams_server::StreamsServer,
};
#[cfg(feature = "profiling")]
use pyroscope::PyroscopeAgent;
#[cfg(feature = "profiling")]
use pyroscope_pprofrs::{PprofConfig, pprof_backend};
use tokio::sync::oneshot;
use tonic::{
    codec::CompressionEncoding,
    metadata::{MetadataKey, MetadataMap, MetadataValue},
    transport::{Identity, ServerTlsConfig},
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Registry;
#[cfg(feature = "enterprise")]
use {config::Config, o2_enterprise::enterprise::common::config::O2Config};
#[cfg(feature = "pyroscope")]
use {
    pyroscope::PyroscopeAgent,
    pyroscope_pprofrs::{PprofConfig, pprof_backend},
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
use tracing_subscriber::{
    EnvFilter, filter::LevelFilter as TracingLevelFilter, fmt::Layer, prelude::*,
};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    #[cfg(feature = "tokio-console")]
    console_subscriber::ConsoleLayer::builder()
        .retention(Duration::from_secs(
            get_config().tokio_console.tokio_console_retention,
        ))
        .server_addr(
            format!(
                "{}:{}",
                get_config().tokio_console.tokio_console_server_addr,
                get_config().tokio_console.tokio_console_server_port
            )
            .as_str()
            .parse::<SocketAddr>()?,
        )
        .init();

    // setup profiling
    #[cfg(feature = "profiling")]
    let pprof_guard =
        if get_config().profiling.pprof_enabled || get_config().profiling.pprof_protobuf_enabled {
            let guard = pprof::ProfilerGuardBuilder::default()
                .frequency(1000)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
                .unwrap();
            Some(guard)
        } else {
            None
        };

    // setup pyroscope
    #[cfg(feature = "pyroscope")]
    let pyroscope_agent = if get_config().profiling.pyroscope_enabled {
        let agent = PyroscopeAgent::builder(
            &get_config().profiling.pyroscope_server_url,
            &get_config().profiling.pyroscope_project_name,
        )
        .tags(
            [
                ("role", get_config().common.node_role.as_str()),
                ("instance", get_config().common.instance_name.as_str()),
                ("version", config::VERSION),
            ]
            .to_vec(),
        )
        .backend(pprof_backend(PprofConfig::new().sample_rate(100)))
        .build()
        .expect("Failed to setup pyroscope agent");
        let agent_running = agent.start().expect("Failed to start pyroscope agent");
        Some(agent_running)
    } else {
        None
    };

    // cli mode
    if cli::cli().await? {
        return Ok(());
    }

    let cfg = get_config();

    // setup logs
    #[cfg(feature = "tokio-console")]
    let enable_tokio_console = true;
    #[cfg(not(feature = "tokio-console"))]
    let enable_tokio_console = false;
    let mut tracer_provider = None;
    let _guard: Option<WorkerGuard> = if enable_tokio_console {
        None
    } else if cfg.log.events_enabled {
        let logger = zo_logger::ZoLogger {
            sender: zo_logger::EVENT_SENDER.clone(),
        };
        log::set_boxed_logger(Box::new(logger)).map(|()| {
            log::set_max_level(LevelFilter::from_str(&cfg.log.level).unwrap_or(LevelFilter::Info))
        })?;
        None
    } else if cfg.common.tracing_enabled || cfg.common.tracing_search_enabled {
        tracer_provider = Some(enable_tracing()?);
        None
    } else {
        Some(setup_logs())
    };

    log::info!("Starting OpenObserve {}", config::VERSION);
    log::info!(
        "System info: CPU cores {}, MEM total {}, Disk total {}, free {}",
        cfg.limit.real_cpu_num,
        bytes_to_human_readable(cfg.limit.mem_total as f64),
        bytes_to_human_readable(cfg.limit.disk_total as f64),
        bytes_to_human_readable(cfg.limit.disk_free as f64),
    );
    log::info!(
        "Caches info: Disk max size {}, MEM max size {}, Datafusion pool size: {}",
        bytes_to_human_readable((cfg.disk_cache.max_size * cfg.disk_cache.bucket_num) as f64),
        bytes_to_human_readable((cfg.memory_cache.max_size * cfg.memory_cache.bucket_num) as f64),
        bytes_to_human_readable(cfg.memory_cache.datafusion_max_size as f64),
    );

    // init script server
    #[cfg(feature = "enterprise")]
    if config::cluster::LOCAL_NODE.is_script_server() && config::cluster::LOCAL_NODE.is_standalone()
    {
        log::info!("Starting script server");
        return init_script_server().await;
    }

    // init backend jobs
    let (job_init_tx, job_init_rx) = oneshot::channel();
    let (job_shutudown_tx, job_shutdown_rx) = oneshot::channel();
    let (job_stopped_tx, job_stopped_rx) = oneshot::channel();
    let job_rt_handle = std::thread::spawn(move || {
        let cfg = get_config();
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.limit.job_runtime_worker_num)
            .enable_all()
            .thread_name("job_runtime")
            .max_blocking_threads(cfg.limit.job_runtime_blocking_worker_num)
            .build()
        else {
            job_init_tx.send(false).ok();
            panic!("job runtime init failed")
        };
        let _guard = rt.enter();
        rt.block_on(async move {
            // it must be initialized before the server starts
            if let Err(e) = cluster::register_and_keep_alive().await {
                job_init_tx.send(false).ok();
                panic!("cluster init failed: {e}");
            }
            // init config
            if let Err(e) = config::init().await {
                job_init_tx.send(false).ok();
                panic!("config init failed: {e}");
            }

            // db related inits
            if let Err(e) = migration::init_db().await {
                job_init_tx.send(false).ok();
                panic!("db init failed: {e}");
            }

            // init infra
            if let Err(e) = infra::init().await {
                job_init_tx.send(false).ok();
                panic!("infra init failed: {e}");
            }

            if let Err(e) = common_infra::init().await {
                job_init_tx.send(false).ok();
                panic!("common infra init failed: {e}");
            }

            // init enterprise
            #[cfg(feature = "enterprise")]
            if let Err(e) = crate::init_enterprise().await {
                job_init_tx.send(false).ok();
                panic!("enterprise init failed: {e}");
            }

            // ingester init
            if let Err(e) = ingester::init().await {
                job_init_tx.send(false).ok();
                panic!("ingester init failed: {e}");
            }

            // init job
            if let Err(e) = job::init().await {
                job_init_tx.send(false).ok();
                panic!("job init failed: {e}");
            }

            // init meter provider
            let Ok(meter_provider) = job::metrics::init_meter_provider().await else {
                job_init_tx.send(false).ok();
                panic!("meter provider init failed");
            };

            job_init_tx.send(true).ok();
            job_shutdown_rx.await.ok();
            job_stopped_tx.send(()).ok();

            // shutdown meter provider
            let _ = meter_provider.shutdown();

            // flush distinct values
            _ = metadata::close().await;
            // flush WAL cache to disk
            _ = ingester::flush_all().await;
            common_infra::wal::flush_all_to_disk().await;
            // flush compact offset cache to disk disk
            _ = db::compact::files::sync_cache_to_db().await;
            // flush db
            let db = infra::db::get_db().await;
            _ = db.close().await;
        });
    });

    // wait for job init
    match job_init_rx.await {
        Ok(true) => log::info!("backend job init success"),
        Ok(false) => {
            return Err(anyhow::anyhow!("backend job init failed, exiting"));
        }
        Err(e) => {
            return Err(anyhow::anyhow!("backend job init failed: {}", e));
        }
    }

    // init gRPC server
    let (grpc_init_tx, grpc_init_rx) = oneshot::channel();
    let (grpc_shutudown_tx, grpc_shutdown_rx) = oneshot::channel();
    let (grpc_stopped_tx, grpc_stopped_rx) = oneshot::channel();
    let grpc_rt_handle = std::thread::spawn(move || {
        let cfg = get_config();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(cfg.limit.grpc_runtime_worker_num)
            .enable_all()
            .thread_name("grpc_runtime")
            .max_blocking_threads(cfg.limit.grpc_runtime_blocking_worker_num)
            .build()
            .expect("grpc runtime init failed");
        let _guard = rt.enter();
        rt.block_on(async move {
            let ret = if config::cluster::LOCAL_NODE.is_router() {
                init_router_grpc_server(grpc_init_tx, grpc_shutdown_rx, grpc_stopped_tx).await
            } else {
                init_common_grpc_server(grpc_init_tx, grpc_shutdown_rx, grpc_stopped_tx).await
            };
            if let Err(e) = ret {
                log::error!("gRPC server init failed: {e}");
                std::process::exit(1);
            }
        });
    });

    // wait for gRPC init
    grpc_init_rx.await.ok();

    // let node online
    let _ = cluster::set_online(false).await;

    // initialize the jobs are deferred until the gRPC service starts
    job::init_deferred()
        .await
        .expect("Deferred jobs failed to init");

    if cfg.log.events_enabled {
        tokio::task::spawn(async move { zo_logger::send_logs().await });
    }
    if cfg.common.telemetry_enabled {
        tokio::task::spawn(async move {
            meta::telemetry::Telemetry::new()
                .send_track_event("OpenObserve - Starting server", None, true, false)
                .await;
        });
    }

    // let node schedulable
    let mut start_ok = false;
    for _ in 0..10 {
        match cluster::set_schedulable().await {
            Ok(_) => {
                start_ok = true;
                break;
            }
            Err(e) => {
                log::error!("set node schedulable failed: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
    if !start_ok {
        return Err(anyhow::anyhow!("set node schedulable failed"));
    }

    // init http server
    if !cfg.common.tracing_enabled && cfg.common.tracing_search_enabled {
        if let Err(e) = init_http_server_without_tracing().await {
            log::error!("HTTP server runs failed: {e}");
        }
    } else if let Err(e) = init_http_server().await {
        log::error!("HTTP server runs failed: {e}");
    }
    log::info!("HTTP server stopped");

    // stop tracing
    if let Some(tracer_provider) = tracer_provider {
        let result = tracer_provider.shutdown();
        log::info!("Tracer provider shutdown result: {result:?}");
    }

    // flush usage report
    self_reporting::flush().await;

    // leave the cluster
    _ = cluster::leave().await;
    log::info!("Node left cluster");

    // stop gRPC server
    grpc_shutudown_tx.send(()).ok();
    grpc_stopped_rx.await.ok();
    grpc_rt_handle.join().ok();
    log::info!("gRPC server stopped");

    // stop backend jobs
    job_shutudown_tx.send(()).ok();
    job_stopped_rx.await.ok();
    job_rt_handle.join().ok();
    log::info!("backend job stopped");

    // stop telemetry
    if cfg.common.telemetry_enabled {
        meta::telemetry::Telemetry::new()
            .send_track_event("OpenObserve - Server stopped", None, true, true)
            .await;
    }

    // stop profiling
    #[cfg(feature = "profiling")]
    if let Some(guard) = pprof_guard {
        if let Ok(report) = guard.report().build() {
            if cfg.profiling.pprof_protobuf_enabled {
                let pb_file = format!("{}.pb", cfg.profiling.pprof_flamegraph_path);
                match std::fs::File::create(&pb_file) {
                    Ok(mut file) => {
                        use std::io::Write;

                        use pprof::protos::Message;

                        if let Ok(profile) = report.pprof() {
                            let mut content = Vec::new();
                            profile.encode(&mut content).unwrap();
                            if let Err(e) = file.write_all(&content) {
                                log::error!("Failed to write flamegraph: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create flamegraph file: {}", e);
                    }
                }
            } else {
                match std::fs::File::create(&cfg.profiling.pprof_flamegraph_path) {
                    Ok(file) => {
                        if let Err(e) = report.flamegraph(file) {
                            log::error!("Failed to write flamegraph: {}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to create flamegraph file: {}", e);
                    }
                }
            }
        };
    }

    // stop pyroscope
    #[cfg(feature = "pyroscope")]
    if let Some(agent) = pyroscope_agent {
        if let Ok(agent_ready) = agent.stop() {
            agent_ready.shutdown();
        }
    }

    log::info!("server stopped");

    Ok(())
}

async fn init_common_grpc_server(
    init_tx: oneshot::Sender<()>,
    shutdown_rx: oneshot::Receiver<()>,
    stopped_tx: oneshot::Sender<()>,
) -> Result<(), anyhow::Error> {
    let cfg = get_config();
    let ip = if !cfg.grpc.addr.is_empty() {
        cfg.grpc.addr.clone()
    } else {
        "0.0.0.0".to_string()
    };
    let gaddr: SocketAddr = format!("{}:{}", ip, cfg.grpc.port).parse()?;
    let event_svc = EventServer::new(Eventer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let search_svc = SearchServer::new(SEARCH_SERVER.clone())
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let metrics_svc = MetricsServer::new(MetricsQuerier)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let metrics_ingest_svc = MetricsServiceServer::new(MetricsIngester)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let logs_svc = LogsServiceServer::new(LogsServer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let trace_svc = TraceServiceServer::new(TraceServer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let query_cache_svc = QueryCacheServer::new(QueryCacheServerImpl)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let ingest_svc = IngestServer::new(Ingester)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let streams_svc = StreamsServer::new(StreamServiceImpl)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let flight_svc = FlightServiceServer::new(FlightServiceImpl)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let node_svc = NodeServiceServer::new(NodeService)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let cluster_info_svc = ClusterInfoServiceServer::new(ClusterInfoService)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);

    log::info!(
        "starting gRPC server {} at {}",
        if cfg.grpc.tls_enabled { "with TLS" } else { "" },
        gaddr
    );
    init_tx.send(()).ok();

    let builder = if cfg.grpc.tls_enabled {
        let cert = std::fs::read_to_string(&cfg.grpc.tls_cert_path)?;
        let key = std::fs::read_to_string(&cfg.grpc.tls_key_path)?;
        let identity = Identity::from_pem(cert, key);
        tonic::transport::Server::builder().tls_config(ServerTlsConfig::new().identity(identity))?
    } else {
        tonic::transport::Server::builder()
    };
    let ret = builder
        .layer(tonic::service::interceptor(check_auth))
        .add_service(event_svc)
        .add_service(search_svc)
        .add_service(metrics_svc)
        .add_service(metrics_ingest_svc)
        .add_service(trace_svc)
        .add_service(logs_svc)
        .add_service(query_cache_svc)
        .add_service(ingest_svc)
        .add_service(streams_svc)
        .add_service(flight_svc)
        .add_service(node_svc)
        .add_service(cluster_info_svc)
        .serve_with_shutdown(gaddr, async {
            shutdown_rx.await.ok();
            log::info!("gRPC server starts shutting down");
        })
        .await;
    if let Err(e) = ret {
        return Err(anyhow::anyhow!("{e}"));
    }

    stopped_tx.send(()).ok();
    Ok(())
}

async fn init_router_grpc_server(
    init_tx: oneshot::Sender<()>,
    shutdown_rx: oneshot::Receiver<()>,
    stopped_tx: oneshot::Sender<()>,
) -> Result<(), anyhow::Error> {
    let cfg = get_config();
    let gaddr: SocketAddr = format!("0.0.0.0:{}", cfg.grpc.port).parse()?;
    let logs_svc = LogsServiceServer::new(router::grpc::ingest::logs::LogsServer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let metrics_svc = MetricsServiceServer::new(router::grpc::ingest::metrics::MetricsServer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);
    let traces_svc = TraceServiceServer::new(router::grpc::ingest::traces::TraceServer)
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(cfg.grpc.max_message_size * 1024 * 1024)
        .max_encoding_message_size(cfg.grpc.max_message_size * 1024 * 1024);

    log::info!(
        "starting gRPC server {} at {}",
        if cfg.grpc.tls_enabled { "with TLS" } else { "" },
        gaddr
    );
    init_tx.send(()).ok();

    let builder = if cfg.grpc.tls_enabled {
        let cert = std::fs::read_to_string(&cfg.grpc.tls_cert_path)?;
        let key = std::fs::read_to_string(&cfg.grpc.tls_key_path)?;
        let identity = Identity::from_pem(cert, key);
        tonic::transport::Server::builder().tls_config(ServerTlsConfig::new().identity(identity))?
    } else {
        tonic::transport::Server::builder()
    };
    let ret = builder
        .layer(tonic::service::interceptor(check_auth))
        .add_service(logs_svc)
        .add_service(metrics_svc)
        .add_service(traces_svc)
        .serve_with_shutdown(gaddr, async {
            shutdown_rx.await.ok();
            log::info!("gRPC server starts shutting down");
        })
        .await;
    if let Err(e) = ret {
        return Err(anyhow::anyhow!("{e}"));
    }

    stopped_tx.send(()).ok();
    Ok(())
}

async fn init_http_server() -> Result<(), anyhow::Error> {
    let cfg = get_config();

    let thread_id = Arc::new(AtomicU16::new(0));
    let haddr: SocketAddr = if cfg.http.ipv6_enabled {
        format!("[::]:{}", cfg.http.port).parse()?
    } else {
        let ip = if !cfg.http.addr.is_empty() {
            cfg.http.addr.clone()
        } else {
            "0.0.0.0".to_string()
        };
        format!("{}:{}", ip, cfg.http.port).parse()?
    };

    let server = HttpServer::new(move || {
        let cfg = get_config();
        let local_id = thread_id.load(Ordering::SeqCst) as usize;
        if cfg.common.feature_per_thread_lock {
            thread_id.fetch_add(1, Ordering::SeqCst);
        }
        let scheme = if cfg.http.tls_enabled {
            "HTTPS"
        } else {
            "HTTP"
        };
        log::info!("Starting {scheme} server at: {haddr}, thread_id: {local_id}");
        let mut app = App::new();
        if config::cluster::LOCAL_NODE.is_router() {
            let http_client =
                router::http::create_http_client().expect("Failed to create http tls client");
            let factory = web::scope(&cfg.common.base_uri);
            #[cfg(feature = "enterprise")]
            let factory = factory.wrap(
                o2_ratelimit::middleware::RateLimitController::new_with_extractor(Some(
                    router::ratelimit::resource_extractor::default_extractor,
                )),
            );

            app = app
                .service(
                    // if `cfg.common.base_uri` is empty, scope("") still works as expected.
                    factory
                        .wrap(middlewares::SlowLog::new(cfg.limit.http_slow_log_threshold))
                        .wrap(from_fn(middlewares::check_keep_alive))
                        .service(get_metrics)
                        .service(router::http::config)
                        .service(router::http::config_paths)
                        .service(router::http::api)
                        .service(router::http::aws)
                        .service(router::http::gcp)
                        .service(router::http::rum)
                        .configure(get_basic_routes)
                        .configure(get_proxy_routes),
                )
                .app_data(web::Data::new(http_client))
        } else {
            app = app.service({
                let scope = web::scope(&cfg.common.base_uri)
                    .wrap(middlewares::SlowLog::new(cfg.limit.http_slow_log_threshold))
                    .wrap(from_fn(middlewares::check_keep_alive))
                    .service(get_metrics)
                    .configure(get_config_routes)
                    .configure(get_service_routes)
                    .configure(get_other_service_routes)
                    .configure(get_basic_routes)
                    .configure(get_proxy_routes);
                #[cfg(feature = "enterprise")]
                let scope = scope.configure(get_script_server_routes);
                scope
            })
        }
        app.app_data(web::JsonConfig::default().limit(cfg.limit.req_json_limit))
            .app_data(web::PayloadConfig::new(cfg.limit.req_payload_limit)) // size is in bytes
            .app_data(web::Data::new(local_id))
            .wrap(middlewares::Compress::default())
            .wrap(middleware::Logger::new(
                r#"%a "%r" %s %b "%{Content-Length}i" "%{Referer}i" "%{User-Agent}i" %T"#,
            ))
            .wrap(RequestTracing::new())
    })
    .keep_alive(if cfg.limit.http_keep_alive_disabled {
        KeepAlive::Disabled
    } else {
        KeepAlive::Timeout(Duration::from_secs(max(1, cfg.limit.http_keep_alive)))
    })
    .client_request_timeout(Duration::from_secs(max(1, cfg.limit.http_request_timeout)))
    .shutdown_timeout(max(1, cfg.limit.http_shutdown_timeout));
    let server = if cfg.http.tls_enabled {
        let sc = http_tls_config()?;
        server.bind_rustls_0_23(haddr, sc)?
    } else {
        server.bind(haddr)?
    };

    let server = server
        .workers(cfg.limit.http_worker_num)
        .worker_max_blocking_threads(cfg.limit.http_worker_max_blocking)
        .disable_signals()
        .run();
    let handle = server.handle();
    tokio::task::spawn(async move {
        graceful_shutdown(handle).await;
    });
    server.await?;
    Ok(())
}

async fn init_http_server_without_tracing() -> Result<(), anyhow::Error> {
    let cfg = get_config();
    let thread_id = Arc::new(AtomicU16::new(0));
    let haddr: SocketAddr = if cfg.http.ipv6_enabled {
        format!("[::]:{}", cfg.http.port).parse()?
    } else {
        let ip = if !cfg.http.addr.is_empty() {
            cfg.http.addr.clone()
        } else {
            "0.0.0.0".to_string()
        };
        format!("{}:{}", ip, cfg.http.port).parse()?
    };

    let server = HttpServer::new(move || {
        let cfg = get_config();
        let local_id = thread_id.load(Ordering::SeqCst) as usize;
        if cfg.common.feature_per_thread_lock {
            thread_id.fetch_add(1, Ordering::SeqCst);
        }

        let scheme = if cfg.http.tls_enabled {
            "HTTPS"
        } else {
            "HTTP"
        };
        log::info!("Starting {scheme} server at: {haddr}, thread_id: {local_id}");

        let mut app = App::new();
        if config::cluster::LOCAL_NODE.is_router() {
            let http_client =
                router::http::create_http_client().expect("Failed to create http tls client");
            let factory = web::scope(&cfg.common.base_uri);
            #[cfg(feature = "enterprise")]
            let factory = factory.wrap(
                o2_ratelimit::middleware::RateLimitController::new_with_extractor(Some(
                    router::ratelimit::resource_extractor::default_extractor,
                )),
            );

            app = app
                .service(
                    // if `cfg.common.base_uri` is empty, scope("") still works as expected.
                    factory
                        .wrap(middlewares::SlowLog::new(cfg.limit.http_slow_log_threshold))
                        .wrap(from_fn(middlewares::check_keep_alive))
                        .service(get_metrics)
                        .service(router::http::config)
                        .service(router::http::config_paths)
                        .service(router::http::api)
                        .service(router::http::aws)
                        .service(router::http::gcp)
                        .service(router::http::rum)
                        .configure(get_basic_routes)
                        .configure(get_proxy_routes),
                )
                .app_data(web::Data::new(http_client))
        } else {
            app = app.service({
                let scope = web::scope(&cfg.common.base_uri)
                    .wrap(middlewares::SlowLog::new(cfg.limit.http_slow_log_threshold))
                    .wrap(from_fn(middlewares::check_keep_alive))
                    .service(get_metrics)
                    .configure(get_config_routes)
                    .configure(get_service_routes)
                    .configure(get_other_service_routes)
                    .configure(get_basic_routes)
                    .configure(get_proxy_routes);
                #[cfg(feature = "enterprise")]
                let scope = scope.configure(get_script_server_routes);
                scope
            })
        }
        app.app_data(web::JsonConfig::default().limit(cfg.limit.req_json_limit))
            .app_data(web::PayloadConfig::new(cfg.limit.req_payload_limit)) // size is in bytes
            .app_data(web::Data::new(local_id))
            .wrap(middlewares::Compress::default())
            .wrap(middleware::Logger::new(
                r#"%a "%r" %s %b "%{Content-Length}i" "%{Referer}i" "%{User-Agent}i" %T"#,
            ))
    })
    .keep_alive(if cfg.limit.http_keep_alive_disabled {
        KeepAlive::Disabled
    } else {
        KeepAlive::Timeout(Duration::from_secs(max(1, cfg.limit.http_keep_alive)))
    })
    .client_request_timeout(Duration::from_secs(max(1, cfg.limit.http_request_timeout)))
    .shutdown_timeout(max(1, cfg.limit.http_shutdown_timeout));
    let server = if cfg.http.tls_enabled {
        let sc = http_tls_config()?;
        server.bind_rustls_0_23(haddr, sc)?
    } else {
        server.bind(haddr)?
    };

    let server = server
        .workers(cfg.limit.http_worker_num)
        .worker_max_blocking_threads(cfg.limit.http_worker_max_blocking)
        .disable_signals()
        .run();
    let handle = server.handle();
    tokio::task::spawn(async move {
        graceful_shutdown(handle).await;
    });
    server.await?;
    Ok(())
}

async fn graceful_shutdown(handle: ServerHandle) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigquit = signal(SignalKind::quit()).unwrap();
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigint = signal(SignalKind::interrupt()).unwrap();

        tokio::select! {
            _ = sigquit.recv() =>  log::info!("SIGQUIT received"),
            _ = sigterm.recv() =>  log::info!("SIGTERM received"),
            _ = sigint.recv() =>   log::info!("SIGINT received"),
        }
    }

    #[cfg(not(unix))]
    {
        use tokio::signal::windows::*;

        let mut sigbreak = ctrl_break().unwrap();
        let mut sigint = ctrl_c().unwrap();
        let mut sigquit = ctrl_close().unwrap();
        let mut sigterm = ctrl_shutdown().unwrap();

        tokio::select! {
            _ = sigbreak.recv() =>  log::info!("ctrl-break received"),
            _ = sigquit.recv() =>  log::info!("ctrl-c received"),
            _ = sigterm.recv() =>  log::info!("ctrl-close received"),
            _ = sigint.recv() =>   log::info!("ctrl-shutdown received"),
        }
    }
    // tokio::signal::ctrl_c().await.unwrap();
    // println!("ctrl-c received!");

    // offline the node
    if let Err(e) = cluster::set_offline(true).await {
        log::error!("set offline failed: {e}");
    }
    log::info!("Node is offline");

    handle.stop(true).await;
}

/// Setup the tracing related components
pub(crate) fn setup_logs() -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    let cfg = get_config();
    let (writer, guard) = if cfg.log.file_dir.is_empty() {
        let (non_blocking, _guard) = tracing_appender::non_blocking(std::io::stdout());
        (BoxMakeWriter::new(non_blocking), _guard)
    } else {
        let file_name_prefix = if cfg.log.file_name_prefix.is_empty() {
            format!("o2.{}.log", cfg.common.instance_name.as_str())
        } else {
            cfg.log.file_name_prefix.to_string()
        };
        let file_appender = tracing_appender::rolling::daily(&cfg.log.file_dir, file_name_prefix);
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
        (BoxMakeWriter::new(non_blocking), _guard)
    };
    let layer = if cfg.log.json_format {
        Layer::default()
            .with_writer(writer)
            .with_timer(config::meta::logger::CustomTimeFormat)
            .with_ansi(false)
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .boxed()
    } else {
        Layer::default()
            .with_writer(writer)
            .with_ansi(false)
            .with_target(true)
            .event_format(config::meta::logger::O2Formatter::default())
            .boxed()
    };

    tracing_subscriber::registry()
        .with(
            EnvFilter::builder()
                .with_default_directive(TracingLevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .with(layer)
        .init();
    guard
}

fn enable_tracing() -> Result<opentelemetry_sdk::trace::TracerProvider, anyhow::Error> {
    let cfg = get_config();
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    let tracer = opentelemetry_otlp::new_pipeline().tracing();
    let tracer = if cfg.common.otel_otlp_grpc_url.is_empty() {
        tracer.with_exporter({
            let mut headers = HashMap::new();
            headers.insert(
                cfg.common.tracing_header_key.clone(),
                cfg.common.tracing_header_value.clone(),
            );
            opentelemetry_otlp::new_exporter()
                .http()
                .with_http_client(
                    reqwest::Client::builder()
                        .danger_accept_invalid_certs(true)
                        .timeout(Duration::from_secs(10))           // Overall request timeout
                        .connect_timeout(Duration::from_secs(5))    // Connection establishment timeout
                        .pool_idle_timeout(Duration::from_secs(60)) // How long to keep idle connections
                        .pool_max_idle_per_host(10) // How many idle connections to keep per host
                        .build()?,
                )
                .with_endpoint(&cfg.common.otel_otlp_url)
                .with_headers(headers)
        })
    } else {
        tracer.with_exporter({
            let mut metadata = MetadataMap::new();
            metadata.insert(
                MetadataKey::from_str(&cfg.common.tracing_header_key).unwrap(),
                MetadataValue::from_str(&cfg.common.tracing_header_value).unwrap(),
            );
            metadata.insert(
                MetadataKey::from_str(&cfg.grpc.org_header_key).unwrap(),
                MetadataValue::from_str(&cfg.common.tracing_grpc_header_org).unwrap(),
            );
            metadata.insert(
                MetadataKey::from_str(&cfg.grpc.stream_header_key).unwrap(),
                MetadataValue::from_str(&cfg.common.tracing_grpc_header_stream_name).unwrap(),
            );
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(&cfg.common.otel_otlp_grpc_url)
                .with_metadata(metadata)
                .with_protocol(opentelemetry_otlp::Protocol::Grpc)
        })
    };

    // Store the tracer provider before installing batch processor
    let tracer = tracer.with_trace_config(
        opentelemetry_sdk::trace::Config::default().with_resource(Resource::new(vec![
            KeyValue::new("service.name", cfg.common.node_role.to_string()),
            KeyValue::new("service.instance", cfg.common.instance_name.to_string()),
            KeyValue::new("service.version", config::VERSION),
        ])),
    );

    // Install batch processor
    let tracer = tracer.install_batch(opentelemetry_sdk::runtime::Tokio)?;

    let layer = if cfg.log.json_format {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .json()
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().with_ansi(false).boxed()
    };

    global::set_tracer_provider(tracer.clone());
    Registry::default()
        .with(tracing_subscriber::EnvFilter::new(&cfg.log.level))
        .with(layer)
        .with(OpenTelemetryLayer::new(
            tracer.tracer("tracing-otel-subscriber"),
        ))
        .init();

    // Return the tracer provider
    Ok(tracer)
}

#[cfg(feature = "enterprise")]
async fn init_script_server() -> Result<(), anyhow::Error> {
    let cfg = get_config();

    let thread_id = Arc::new(AtomicU16::new(0));
    let haddr: SocketAddr = if cfg.http.ipv6_enabled {
        format!("[::]:{}", cfg.http.port).parse()?
    } else {
        let ip = if !cfg.http.addr.is_empty() {
            cfg.http.addr.clone()
        } else {
            "0.0.0.0".to_string()
        };
        format!("{}:{}", ip, cfg.http.port).parse()?
    };

    // following command will setup the namespace
    #[cfg(feature = "enterprise")]
    o2_enterprise::enterprise::actions::action_deployer::init().await?;

    let server = HttpServer::new(move || {
        let cfg = get_config();
        let local_id = thread_id.load(Ordering::SeqCst) as usize;
        if cfg.common.feature_per_thread_lock {
            thread_id.fetch_add(1, Ordering::SeqCst);
        }
        let scheme = if cfg.http.tls_enabled {
            "HTTPS"
        } else {
            "HTTP"
        };
        log::info!("Starting Script Server {scheme} server at: {haddr}, thread_id: {local_id}");
        let mut app = App::new();
        app = app.service(web::scope(&cfg.common.base_uri).configure(get_script_server_routes));
        app.app_data(web::JsonConfig::default().limit(cfg.limit.req_json_limit))
            .app_data(web::PayloadConfig::new(cfg.limit.req_payload_limit)) // size is in bytes
            .app_data(web::Data::new(local_id))
            .wrap(middlewares::Compress::default())
            .wrap(middleware::Logger::new(
                r#"%a "%r" %s %b "%{Content-Length}i" "%{Referer}i" "%{User-Agent}i" %T"#,
            ))
            .wrap(RequestTracing::new())
    })
    .keep_alive(if cfg.limit.http_keep_alive_disabled {
        KeepAlive::Disabled
    } else {
        KeepAlive::Timeout(Duration::from_secs(max(1, cfg.limit.http_keep_alive)))
    })
    .client_request_timeout(Duration::from_secs(max(1, cfg.limit.http_request_timeout)))
    .shutdown_timeout(max(1, cfg.limit.http_shutdown_timeout));
    let server = if cfg.http.tls_enabled {
        let sc = http_tls_config()?;
        server.bind_rustls_0_23(haddr, sc)?
    } else {
        server.bind(haddr)?
    };

    let server = server
        .workers(cfg.limit.http_worker_num)
        .worker_max_blocking_threads(cfg.limit.http_worker_num * cfg.limit.http_worker_max_blocking)
        .disable_signals()
        .run();
    let handle = server.handle();
    tokio::task::spawn(async move {
        graceful_shutdown(handle).await;
    });
    server.await?;

    log::info!("HTTP server stopped");

    // flush usage report
    self_reporting::flush().await;

    // stop telemetry
    if cfg.common.telemetry_enabled {
        meta::telemetry::Telemetry::new()
            .send_track_event("OpenObserve - Server stopped", None, true, true)
            .await;
    }

    #[cfg(feature = "profiling")]
    if let Some(agent) = agent {
        let agent_ready = agent.stop().unwrap();
        agent_ready.shutdown();
    }

    log::info!("server stopped");

    Ok(())
}

#[cfg(feature = "enterprise")]
pub fn get_script_server_routes(cfg: &mut web::ServiceConfig) {
    let cors = get_cors();
    cfg.service(
        web::scope("/api")
            .wrap(actix_web_httpauth::middleware::HttpAuthentication::with_fn(
                script_server_validator,
            ))
            .wrap(cors)
            .service(script_server::create_job)
            .service(script_server::delete_job)
            .service(script_server::get_app_details)
            .service(script_server::list_deployed_apps)
            .service(script_server::patch_action),
    );
}

/// Initializes enterprise features.
#[cfg(feature = "enterprise")]
async fn init_enterprise() -> Result<(), anyhow::Error> {
    o2_enterprise::enterprise::search::init().await?;

    if let Err(e) = o2_enterprise::enterprise::actions::action_manager::init_client() {
        log::warn!("Failed to init action manager client: {e}");
    }

    if o2_enterprise::enterprise::common::config::get_config()
        .super_cluster
        .enabled
    {
        log::info!("init super cluster");
        o2_enterprise::enterprise::super_cluster::kv::init().await?;
        openobserve::super_cluster_queue::init().await?;
    }

    // check ratelimit config
    let cfg = config::get_config();
    let o2cfg = o2_enterprise::enterprise::common::config::get_config();
    if let Err(e) = check_ratelimit_config(&cfg, &o2cfg) {
        panic!("ratelimit config error: {e}");
    }

    o2_enterprise::enterprise::pipeline::pipeline_file_server::PipelineFileServer::run().await?;
    if o2cfg.rate_limit.rate_limit_enabled && o2_openfga::config::get_config().enabled {
        o2_ratelimit::init(openobserve::handler::http::router::openapi::openapi_info().await)
            .await?;
    }

    Ok(())
}

#[cfg(feature = "enterprise")]
fn check_ratelimit_config(cfg: &Config, o2cfg: &O2Config) -> Result<(), anyhow::Error> {
    if o2cfg.rate_limit.rate_limit_enabled {
        let meta_store: config::meta::meta_store::MetaStore =
            cfg.common.queue_store.as_str().into();
        if meta_store != config::meta::meta_store::MetaStore::Nats {
            return Err(anyhow::anyhow!(
                "ZO_QUEUE_STORE must be nats when ratelimit is enabled"
            ));
        }
    }

    if o2cfg.rate_limit.rate_limit_rule_refresh_interval < 2 {
        return Err(anyhow::anyhow!(
            "ratelimit rules refresh interval must be greater than or equal to 2 seconds"
        ));
    }
    Ok(())
}
