use std::any::Any;

use crate::catalog::{Database, UserCatalog, UserCatalogRef};
use datafusion::arrow::datatypes::DataType;
use datafusion::physical_plan::common::SizedRecordBatchStream;
use datafusion::physical_plan::metrics::{ExecutionPlanMetricsSet, MemTrackingMetrics};
use datafusion::physical_plan::SendableRecordBatchStream;
use datafusion::{
    error::DataFusionError,
    logical_expr::{AggregateUDF, ScalarUDF, TableSource},
    sql::{planner::ContextProvider, TableReference},
};

use models::schema::{TableColumn, TableSchema};

use datafusion::arrow::record_batch::RecordBatch;

use crate::table::ClusterTable;
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::datasource::provider_as_source;
use models::schema::DatabaseSchema;

use spi::catalog::{
    MetaData, MetaDataRef, MetadataError, Result, DEFAULT_CATALOG, DEFAULT_DATABASE,
};
use spi::query::function::FuncMetaManagerRef;
use std::sync::Arc;
use tskv::engine::EngineRef;

/// remote meta
pub struct RemoteCatalogMeta {}

/// local meta
#[derive(Clone)]
pub struct LocalCatalogMeta {
    catalog_name: String,
    database_name: String,
    engine: EngineRef,
    catalog: UserCatalogRef,
    func_manager: FuncMetaManagerRef,
}

impl LocalCatalogMeta {
    pub fn new_with_default(engine: EngineRef, func_manager: FuncMetaManagerRef) -> Result<Self> {
        let meta = Self {
            catalog_name: DEFAULT_CATALOG.to_string(),
            database_name: DEFAULT_DATABASE.to_string(),
            engine: engine.clone(),
            catalog: Arc::new(UserCatalog::new(engine)),
            func_manager,
        };
        if let Err(e) = meta.create_database(
            &meta.database_name,
            DatabaseSchema::new(&meta.database_name),
        ) {
            match e {
                MetadataError::DatabaseAlreadyExists { .. } => {}
                _ => return Err(e),
            }
        };
        Ok(meta)
    }
}

impl MetaData for LocalCatalogMeta {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn with_catalog(&self, catalog_name: &str) -> Arc<dyn MetaData> {
        let mut metadata = self.clone();
        metadata.catalog_name = catalog_name.to_string();

        Arc::new(metadata)
    }

    fn with_database(&self, database: &str) -> Arc<dyn MetaData> {
        let mut metadata = self.clone();
        metadata.database_name = database.to_string();

        Arc::new(metadata)
    }

    //todo: local mode dont support multi-tenant

    fn catalog_name(&self) -> &str {
        self.catalog_name.as_str()
    }

    fn schema_name(&self) -> &str {
        self.database_name.as_str()
    }

    fn table(&self, table: TableReference) -> Result<TableSchema> {
        let catalog_name = self.catalog_name();
        let schema_name = self.schema_name();
        let name = table.resolve(catalog_name, schema_name);
        // note: local mod dont support multiple catalog use DEFAULT_CATALOG
        // let catalog_name = name.catalog;
        self.catalog
            .schema(name.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: name.schema.to_string(),
            })?
            .table(name.table)
            .ok_or_else(|| MetadataError::TableNotExists {
                table_name: name.table.to_string(),
            })
    }

    fn database(&self, name: &str) -> Result<DatabaseSchema> {
        self.engine
            .get_db_schema(name)
            .ok_or(MetadataError::DatabaseNotExists {
                database_name: name.to_string(),
            })
    }

    fn function(&self) -> FuncMetaManagerRef {
        self.func_manager.clone()
    }

    fn drop_table(&self, name: &str) -> Result<()> {
        let table: TableReference = name.into();
        let name = table.resolve(self.catalog_name.as_str(), self.database_name.as_str());
        self.catalog
            .schema(name.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: name.schema.to_string(),
            })?
            .deregister_table(name.table)
            .map(|_| ())
    }

    fn drop_database(&self, name: &str) -> Result<()> {
        self.catalog.deregister_schema(name).map(|_| ())
    }

    fn create_table(&self, name: &str, table_schema: TableSchema) -> Result<()> {
        let table: TableReference = name.into();
        let table_ref = table.resolve(self.catalog_name.as_str(), self.database_name.as_str());

        self.catalog
            .schema(table_ref.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: table_ref.schema.to_string(),
            })?
            // Currently the SchemaProvider creates a temporary table
            .register_table(table.table().to_owned(), table_schema)
            .map(|_| ())
    }

    fn create_database(&self, name: &str, database: DatabaseSchema) -> Result<()> {
        let user_schema = Database::new(name.to_string(), self.engine.clone(), database);
        self.catalog
            .register_schema(name, Arc::new(user_schema))
            .map(|_| ())
    }

    fn database_names(&self) -> Result<Vec<String>> {
        self.catalog.schema_names()
    }

    fn show_tables(&self, name: &Option<String>) -> Result<Vec<String>> {
        let database_name = match name {
            None => self.database_name.as_str(),
            Some(v) => v.as_str(),
        };

        self.catalog
            .schema(database_name)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: database_name.to_string(),
            })?
            .table_names()
    }

    fn alter_database(&self, database: DatabaseSchema) -> Result<()> {
        self.engine
            .alter_database(&database)
            .map_err(|e| MetadataError::External {
                message: format!("{}", e),
            })
    }

    fn alter_table_add_column(&self, table_name: &str, column: TableColumn) -> Result<()> {
        let table_ref = TableReference::from(table_name)
            .resolve(self.catalog_name.as_str(), self.database_name.as_str());
        self.catalog
            .schema(table_ref.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: table_ref.schema.to_string(),
            })?
            .table_add_column(table_ref.table, column)
    }

    fn alter_table_alter_column(
        &self,
        table_name: &str,
        column_name: &str,
        new_column: TableColumn,
    ) -> Result<()> {
        let table_ref = TableReference::from(table_name)
            .resolve(self.catalog_name.as_str(), self.database_name.as_str());
        self.catalog
            .schema(table_ref.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: table_ref.schema.to_string(),
            })?
            .table_alter_column(table_ref.table, column_name, new_column)
    }

    fn alter_table_drop_column(&self, table_name: &str, column_name: &str) -> Result<()> {
        let table_ref = TableReference::from(table_name)
            .resolve(self.catalog_name.as_str(), self.database_name.as_str());

        self.catalog
            .schema(table_ref.schema)
            .ok_or_else(|| MetadataError::DatabaseNotExists {
                database_name: table_ref.schema.to_string(),
            })?
            .table_drop_column(table_name, column_name)
    }
}

pub struct MetadataProvider {
    meta: MetaDataRef,
}

impl MetadataProvider {
    #[inline(always)]
    pub fn new(meta: MetaDataRef) -> Self {
        Self { meta }
    }
}
impl ContextProvider for MetadataProvider {
    fn get_table_provider(
        &self,
        name: TableReference,
    ) -> datafusion::common::Result<Arc<dyn TableSource>> {
        match self.meta.table(name) {
            Ok(table) => {
                // todo: we need a DataSourceManager to get engine and build table provider
                let local_catalog_meta = self
                    .meta
                    .as_any()
                    .downcast_ref::<LocalCatalogMeta>()
                    .ok_or_else(|| DataFusionError::Plan("failed to get meta data".to_string()))?;
                match table {
                    TableSchema::TsKvTableSchema(schema) => Ok(provider_as_source(Arc::new(
                        ClusterTable::new(local_catalog_meta.engine.clone(), schema),
                    ))),
                    TableSchema::ExternalTableSchema(schema) => {
                        let table_path = ListingTableUrl::parse(&schema.location)?;
                        let options = schema.table_options()?;
                        let config = ListingTableConfig::new(table_path)
                            .with_listing_options(options)
                            .with_schema(Arc::new(schema.schema));
                        Ok(provider_as_source(Arc::new(ListingTable::try_new(config)?)))
                    }
                }
            }
            Err(_) => {
                let catalog_name = self.meta.catalog_name();
                let schema_name = self.meta.schema_name();
                let resolved_name = name.resolve(catalog_name, schema_name);
                Err(DataFusionError::Plan(format!(
                    "failed to resolve user:{}  db: {}, table: {}",
                    resolved_name.catalog, resolved_name.schema, resolved_name.table
                )))
            }
        }
    }

    fn get_function_meta(&self, name: &str) -> Option<Arc<ScalarUDF>> {
        self.meta.function().udf(name).ok()
    }

    fn get_aggregate_meta(&self, name: &str) -> Option<Arc<AggregateUDF>> {
        self.meta.function().udaf(name).ok()
    }

    fn get_variable_type(&self, _variable_names: &[String]) -> Option<DataType> {
        // TODO
        None
    }
}

pub fn stream_from_batches(batches: Vec<Arc<RecordBatch>>) -> SendableRecordBatchStream {
    let dummy_metrics = ExecutionPlanMetricsSet::new();
    let mem_metrics = MemTrackingMetrics::new(&dummy_metrics, 0);
    let stream = SizedRecordBatchStream::new(batches[0].schema(), batches, mem_metrics);
    Box::pin(stream)
}
