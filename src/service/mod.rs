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

use config::{meta::stream::StreamParams, utils::schema::format_stream_name};
use infra::errors::Result;
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub mod alerts;
pub mod cluster_info;
pub mod compact;
pub mod dashboards;
pub mod db;
pub mod enrichment;
pub mod enrichment_table;
pub mod exporter;
pub mod file_list;
pub mod file_list_dump;
pub mod folders;
pub mod functions;
pub mod grpc;
pub mod ingestion;
pub mod kv;
pub mod logs;
pub mod metadata;
pub mod metrics;
pub mod node;
#[cfg(feature = "cloud")]
pub mod org_usage;
pub mod organization;
pub mod pipeline;
pub mod promql;
#[cfg(feature = "enterprise")]
pub mod ratelimit;
pub mod schema;
pub mod search;
pub mod tantivy;

#[cfg(feature = "enterprise")]
pub mod search_jobs;
pub mod self_reporting;
pub mod session;
pub mod short_url;
pub mod stream;
pub mod syslogs_route;
pub mod tls;
pub mod traces;
pub mod users;

// format stream name
pub async fn get_formatted_stream_name(params: StreamParams) -> Result<String> {
    let stream_name = params.stream_name.to_string();
    let schema = infra::schema::get_cache(&params.org_id, &stream_name, params.stream_type).await?;
    Ok(if schema.fields_map().is_empty() {
        format_stream_name(&stream_name)
    } else {
        stream_name
    })
}

/// Setup tracing with a trace ID
pub async fn setup_tracing_with_trace_id(trace_id: &str, span: tracing::Span) -> tracing::Span {
    let mut headers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let traceparent = format!(
        "00-{}-{}-01", /* 01 to indicate that the span is sampled i.e. needs to be
                        * recorded/exported */
        trace_id,
        config::ider::generate_span_id()
    );
    headers.insert("traceparent".to_string(), traceparent);
    let parent_ctx = opentelemetry::global::get_text_map_propagator(|prop| prop.extract(&headers));
    span.set_parent(parent_ctx);
    span
}
