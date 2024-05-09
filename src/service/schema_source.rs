use std::sync::Arc;

use arrow::{
    array::{Int64Array, StringArray},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use arrow_schema::SchemaRef;
use async_trait::async_trait;
use chrono::Utc;
use config::CONFIG;
use datafusion::{
    datasource::TableProvider,
    error::Result,
    execution::context::SessionState,
    physical_plan::{memory::MemoryExec, ExecutionPlan},
};
use datafusion_expr::{Expr, TableType};
use infra::schema::STREAM_SCHEMAS;

struct SchemaVersion {
    org: String,
    stream_type: String,
    stream_name: String,
    num_fields: usize,
    start_dt: i64,
    end_dt: i64,
    _timestamp: i64,
}

/// Define a simple in-memory map as our data source
pub struct InMemorySchemaDS {
    data: Vec<SchemaVersion>,
    last_updated: i64,
}

impl InMemorySchemaDS {
    pub async fn load() -> Self {
        let mut rows = Vec::new();

        let r = STREAM_SCHEMAS.read().await;
        for (key, versions) in r.iter() {
            if !key.contains('/') {
                continue;
            }
            let keys = key.split('/').collect::<Vec<&str>>();
            if keys.len() < 3 {
                continue; // Ensure there are enough parts in the key to avoid panics
            }
            for (_, schema) in versions {
                let schema_metadata = schema.metadata();
                let start_dt = schema_metadata
                    .get("start_dt")
                    .unwrap_or(&"0".to_string())
                    .parse::<i64>()
                    .unwrap_or(0);
                let schema_version = SchemaVersion {
                    org: keys[0].to_string(),
                    stream_type: keys[1].to_string(),
                    stream_name: keys[2].to_string(),
                    num_fields: schema.fields().len(),
                    start_dt,
                    end_dt: schema_metadata
                        .get("end_dt")
                        .unwrap_or(&"0".to_string())
                        .parse::<i64>()
                        .unwrap_or(0),
                    _timestamp: start_dt,
                };
                rows.push(schema_version);
            }
        }
        drop(r);

        InMemorySchemaDS {
            data: rows,
            last_updated: Utc::now().timestamp(),
        }
    }
}

#[async_trait]
impl TableProvider for InMemorySchemaDS {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        let schema = Schema::new(vec![
            Field::new("org", DataType::Utf8, false),
            Field::new("stream_type", DataType::Utf8, false),
            Field::new("stream_name", DataType::Utf8, false),
            Field::new("num_fields", DataType::Int32, false),
            Field::new("start_dt", DataType::Int32, false),
            Field::new("end_dt", DataType::Int32, false),
            Field::new("_timestamp", DataType::Int32, false),
        ]);
        Arc::new(schema)
    }

    async fn scan(
        &self,
        _state: &SessionState,
        _projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        // print filters ,limits and projection
        println!("{:?} {:?} {:?}", _filters, _limit, _projection);

        if (Utc::now().timestamp() - self.last_updated)
            > CONFIG.common.schema_memtable_refresh_interval
        {
            InMemorySchemaDS::load().await;
        } else {
        };
        // Create vectors for each field to be used in the RecordBatch
        let mut orgs = Vec::new();
        let mut stream_types = Vec::new();
        let mut stream_names = Vec::new();
        let mut num_fields = Vec::new();
        let mut start_dts = Vec::new();
        let mut end_dts = Vec::new();

        for row in &self.data {
            orgs.push(row.org.clone());
            stream_types.push(row.stream_type.clone());
            stream_names.push(row.stream_name.clone());
            num_fields.push(row.num_fields as i64);
            start_dts.push(row.start_dt);
            end_dts.push(row.end_dt);
        }
        // Define the schema
        let schema = Arc::new(Schema::new(vec![
            Field::new("org", DataType::Utf8, false),
            Field::new("stream_type", DataType::Utf8, false),
            Field::new("stream_name", DataType::Utf8, false),
            Field::new("num_fields", DataType::Int64, false),
            Field::new("start_dt", DataType::Int64, false),
            Field::new("end_dt", DataType::Int64, false),
            Field::new("_timestamp", DataType::Int64, false),
        ]));

        // Create a RecordBatch
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(orgs)),
                Arc::new(StringArray::from(stream_types)),
                Arc::new(StringArray::from(stream_names)),
                Arc::new(Int64Array::from(num_fields)),
                Arc::new(Int64Array::from(start_dts.clone())),
                Arc::new(Int64Array::from(end_dts)),
                Arc::new(Int64Array::from(start_dts)),
            ],
        )?;

        // Create MemoryExec plan
        let exec = MemoryExec::try_new(&[vec![batch]], schema, None)?;
        Ok(Arc::new(exec))
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }
}
