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
    io::Cursor,
    sync::Arc,
    task::{Context, Poll},
};

use arrow::{
    array::RecordBatch,
    ipc::{CompressionType, writer::IpcWriteOptions},
};
use arrow_flight::{
    Action, ActionType, Criteria, Empty, FlightData, FlightDescriptor, FlightInfo,
    HandshakeRequest, HandshakeResponse, PollInfo, PutResult, SchemaResult, Ticket,
    encode::FlightDataEncoderBuilder, error::FlightError, flight_service_server::FlightService,
};
use arrow_schema::Schema;
use config::{cluster::LOCAL_NODE, meta::search::ScanStats, metrics};
use datafusion::{
    common::{DataFusionError, Result},
    execution::SendableRecordBatchStream,
    physical_plan::{displayable, execute_stream},
};
use futures::{Stream, StreamExt, TryStreamExt, stream::BoxStream};
use prost::Message;
use tonic::{Request, Response, Status, Streaming};
use tracing_opentelemetry::OpenTelemetrySpanExt;
#[cfg(feature = "enterprise")]
use {
    crate::service::search::SEARCH_SERVER,
    o2_enterprise::enterprise::common::config::get_config as get_o2_config,
    o2_enterprise::enterprise::search::TaskStatus,
};

use crate::{
    handler::grpc::MetadataMap,
    service::search::{
        grpc::flight as grpcFlight,
        inspector::{SearchInspectorFieldsBuilder, search_inspector_fields},
        request::FlightSearchRequest,
        utils::AsyncDefer,
    },
};

#[derive(Default)]
pub struct FlightServiceImpl;

#[tonic::async_trait]
impl FlightService for FlightServiceImpl {
    type HandshakeStream = BoxStream<'static, Result<HandshakeResponse, Status>>;
    type ListFlightsStream = BoxStream<'static, Result<FlightInfo, Status>>;
    type DoGetStream = BoxStream<'static, Result<FlightData, Status>>;
    type DoPutStream = BoxStream<'static, Result<PutResult, Status>>;
    type DoActionStream = BoxStream<'static, Result<arrow_flight::Result, Status>>;
    type ListActionsStream = BoxStream<'static, Result<ActionType, Status>>;
    type DoExchangeStream = BoxStream<'static, Result<FlightData, Status>>;

    #[tracing::instrument(name = "grpc:search:flight:do_get", skip_all)]
    async fn do_get(
        &self,
        request: Request<Ticket>,
    ) -> Result<Response<Self::DoGetStream>, Status> {
        let _start = std::time::Instant::now();
        let cfg = config::get_config();

        let parent_cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&MetadataMap(request.metadata()))
        });
        tracing::Span::current().set_parent(parent_cx.clone());

        // 1. decode ticket to RemoteExecNode
        let ticket = request.into_inner();
        let mut buf = Cursor::new(ticket.ticket);
        let req = proto::cluster_rpc::FlightSearchRequest::decode(&mut buf)
            .map_err(|e| DataFusionError::Internal(format!("{e:?}")))
            .map_err(|e| Status::internal(e.to_string()))?;

        let req: FlightSearchRequest = req.into();
        let trace_id = format!(
            "{}-{}",
            req.query_identifier.trace_id, req.query_identifier.job_id
        );
        let is_super_cluster = req.super_cluster_info.is_super_cluster;
        let timeout = req.search_info.timeout as u64;
        log::info!(
            "[trace_id {}] flight->search: do_get, timeout: {}s",
            trace_id,
            timeout
        );

        #[cfg(feature = "enterprise")]
        if is_super_cluster && !SEARCH_SERVER.contain_key(&trace_id).await {
            SEARCH_SERVER
                .insert(trace_id.clone(), TaskStatus::new_follower(vec![], false))
                .await;
        }

        let result = get_ctx_and_physical_plan(&trace_id, &req).await;
        log::info!(
            "{}",
            search_inspector_fields(
                format!(
                    "[trace_id {trace_id}] flight->do_get: get_ctx_and_physical_plan took: {} ms",
                    _start.elapsed().as_millis(),
                ),
                SearchInspectorFieldsBuilder::new()
                    .node_name(LOCAL_NODE.name.clone())
                    .component("flight::do_get get_ctx_and_physical_plan".to_string())
                    .search_role("follower".to_string())
                    .duration(_start.elapsed().as_millis() as usize)
                    .build()
            )
        );

        #[cfg(feature = "enterprise")]
        if is_super_cluster && !SEARCH_SERVER.is_leader(&trace_id).await {
            SEARCH_SERVER.remove(&trace_id, false).await;
        }

        // 2. prepare dataufion context
        let (ctx, physical_plan, defer, scan_stats) = match result {
            Ok(v) => v,
            Err(e) => {
                // clear session data
                clear_session_data(&trace_id);
                log::error!(
                    "[trace_id {trace_id}] flight->search: do_get physical plan generate error: {e:?}",
                );
                return Err(Status::internal(e.to_string()));
            }
        };

        log::info!(
            "[trace_id {}] flight->search: executing stream, is super cluster: {}",
            trace_id,
            is_super_cluster
        );

        let mut schema = physical_plan.schema();

        if cfg.common.print_key_sql {
            let plan = displayable(physical_plan.as_ref())
                .indent(false)
                .to_string();
            println!("+---------------------------+--------------------------+");
            println!("follow physical plan, is_super_cluster_follower_leader: {is_super_cluster}");
            println!("+---------------------------+--------------------------+");
            println!("{plan}");
        }

        schema = add_scan_stats_to_schema(schema, scan_stats);
        #[cfg(feature = "enterprise")]
        if get_o2_config().super_cluster.enabled && !req.super_cluster_info.is_super_cluster {
            // we only set for non-follow leaders
            // split will always have atleast one element even if the string is empty
            // or the split char is not in string, so we can safely unwrap here
            let main_trace_id = trace_id.split("-").next().unwrap();
            SEARCH_SERVER
                .set_scan_stats(main_trace_id, (&scan_stats).into())
                .await;
        }

        let start = std::time::Instant::now();
        let write_options: IpcWriteOptions = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::ZSTD))
            .map_err(|e| {
                // clear session data
                clear_session_data(&trace_id);
                log::error!(
                    "[trace_id {trace_id}] flight->search: do_get create IPC write options error: {e:?}",
                );
                Status::internal(e.to_string())
            })?;
        let stream = execute_stream(physical_plan, ctx.task_ctx().clone()).map_err(|e| {
            // clear session data
            clear_session_data(&trace_id);
            log::error!(
                "[trace_id {trace_id}] flight->search: do_get physical plan execution error: {e:?}",
            );
            Status::internal(e.to_string())
        })?;
        let flight_data_stream = FlightDataEncoderBuilder::new()
            .with_schema(schema)
            .with_max_flight_data_size(33554432) // 32MB
            .with_options(write_options)
            .build(FlightSenderStream::new(
                trace_id.to_string(),
                stream,
                defer,
                start,
                timeout,
            ))
            .map_err(|err| Status::from_error(Box::new(err)));

        Ok(Response::new(
            Box::pin(flight_data_stream) as Self::DoGetStream
        ))
    }

    async fn handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<Response<Self::HandshakeStream>, Status> {
        Err(Status::unimplemented("Implement handshake"))
    }

    async fn list_flights(
        &self,
        _request: Request<Criteria>,
    ) -> Result<Response<Self::ListFlightsStream>, Status> {
        Err(Status::unimplemented("Implement list_flights"))
    }

    async fn get_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        Err(Status::unimplemented("Implement get_flight_info"))
    }

    async fn poll_flight_info(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<PollInfo>, Status> {
        Err(Status::unimplemented("Implement poll_flight_info"))
    }

    async fn get_schema(
        &self,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<SchemaResult>, Status> {
        Err(Status::unimplemented("Implement get_schema"))
    }

    async fn do_put(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoPutStream>, Status> {
        Err(Status::unimplemented("Implement do_put"))
    }

    async fn do_action(
        &self,
        _request: Request<Action>,
    ) -> Result<Response<Self::DoActionStream>, Status> {
        Err(Status::unimplemented("Implement do_action"))
    }

    async fn list_actions(
        &self,
        _request: Request<Empty>,
    ) -> Result<Response<Self::ListActionsStream>, Status> {
        Err(Status::unimplemented("Implement list_actions"))
    }

    async fn do_exchange(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<Self::DoExchangeStream>, Status> {
        Err(Status::unimplemented("Implement do_exchange"))
    }
}

struct FlightSenderStream {
    trace_id: String,
    stream: SendableRecordBatchStream,
    defer: Option<AsyncDefer>,
    start: std::time::Instant,
    timeout: u64,
}

impl FlightSenderStream {
    fn new(
        trace_id: String,
        stream: SendableRecordBatchStream,
        defer: Option<AsyncDefer>,
        start: std::time::Instant,
        timeout: u64,
    ) -> Self {
        Self {
            trace_id,
            stream,
            defer,
            start,
            timeout,
        }
    }
}

impl Stream for FlightSenderStream {
    type Item = Result<RecordBatch, FlightError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        ctx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.start.elapsed().as_secs() > self.timeout {
            return Poll::Ready(None);
        }
        match self.stream.poll_next_unpin(ctx) {
            Poll::Ready(Some(Ok(batch))) => Poll::Ready(Some(Ok(batch))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Err(e))) => {
                log::error!(
                    "[trace_id {}] flight->search: stream error: {}, took: {} ms",
                    self.trace_id,
                    e,
                    self.start.elapsed().as_millis()
                );
                Poll::Ready(Some(Err(FlightError::Tonic(
                    Status::internal(e.to_string()).into(),
                ))))
            }
        }
    }
}

impl Drop for FlightSenderStream {
    fn drop(&mut self) {
        let end = self.start.elapsed().as_millis();
        log::info!(
            "[trace_id {}] flight->search: stream end, took: {} ms",
            self.trace_id,
            end
        );

        // metrics
        let time = self.start.elapsed().as_secs_f64();
        metrics::GRPC_RESPONSE_TIME
            .with_label_values(&["/search/flight/do_get", "200", "", "", "", ""])
            .observe(time);
        metrics::GRPC_INCOMING_REQUESTS
            .with_label_values(&["/search/flight/do_get", "200", "", "", "", ""])
            .inc();

        if let Some(defer) = self.defer.take() {
            drop(defer);
        } else {
            log::info!(
                "[trace_id {}] flight->search: drop FlightSenderStream",
                self.trace_id
            );
            // clear session data
            clear_session_data(&self.trace_id);
        }
    }
}

type PlanResult = (
    datafusion::prelude::SessionContext,
    Arc<dyn datafusion::physical_plan::ExecutionPlan>,
    Option<AsyncDefer>,
    ScanStats,
);

#[cfg(feature = "enterprise")]
#[tracing::instrument(name = "service:search:grpc:flight::enter", skip_all)]
async fn get_ctx_and_physical_plan(
    trace_id: &str,
    req: &FlightSearchRequest,
) -> Result<PlanResult, infra::errors::Error> {
    if req.super_cluster_info.is_super_cluster {
        let (ctx, physical_plan, defer, scan_stats) =
            crate::service::search::super_cluster::follower::search(trace_id, req).await?;
        Ok((ctx, physical_plan, Some(defer), scan_stats))
    } else {
        let (ctx, physical_plan, scan_stats) = grpcFlight::search(trace_id, req).await?;
        Ok((ctx, physical_plan, None, scan_stats))
    }
}

#[cfg(not(feature = "enterprise"))]
#[tracing::instrument(name = "service:search:grpc:flight::enter", skip_all)]
async fn get_ctx_and_physical_plan(
    trace_id: &str,
    req: &FlightSearchRequest,
) -> Result<PlanResult, infra::errors::Error> {
    let (ctx, physical_plan, scan_stats) = grpcFlight::search(trace_id, req).await?;
    Ok((ctx, physical_plan, None, scan_stats))
}

fn add_scan_stats_to_schema(schema: Arc<Schema>, scan_stats: ScanStats) -> Arc<Schema> {
    let mut metadata = schema.metadata().clone();
    let stats_string = serde_json::to_string(&scan_stats).unwrap_or_default();
    metadata.insert("scan_stats".to_string(), stats_string);
    Arc::new(schema.as_ref().clone().with_metadata(metadata))
}

fn clear_session_data(trace_id: &str) {
    // clear session data
    crate::service::search::datafusion::storage::file_list::clear(trace_id);
    // release wal lock files
    crate::common::infra::wal::release_request(trace_id);
}
