use crate::connectors::object_store::schema_mapper::TableInfo;
use crate::connectors::SourceSchemaResult;
use crate::ingestion::Ingestor;

use super::helper;
use crate::connectors::postgres::connection::helper as connection_helper;
use crate::errors::ConnectorError;
use crate::errors::PostgresConnectorError::{InvalidQueryError, PostgresSchemaError};
use crate::errors::PostgresConnectorError::{SnapshotReadError, SyncWithSnapshotError};
use crossbeam::channel::{unbounded, Sender};

use crate::connectors::postgres::schema::helper::SchemaHelper;
use crate::errors::ConnectorError::PostgresConnectorError;
use dozer_types::types::Schema;
use postgres::fallible_iterator::FallibleIterator;

use std::thread;

use dozer_types::ingestion_types::IngestionMessage;

use dozer_types::types::Operation;

pub struct PostgresSnapshotter<'a> {
    pub conn_config: tokio_postgres::Config,
    pub ingestor: &'a Ingestor,
    pub connector_id: u64,
}

impl<'a> PostgresSnapshotter<'a> {
    pub fn get_tables(
        &self,
        tables: &[TableInfo],
    ) -> Result<Vec<SourceSchemaResult>, ConnectorError> {
        let helper = SchemaHelper::new(self.conn_config.clone());
        helper.get_schemas(tables).map_err(PostgresConnectorError)
    }

    pub fn sync_table(
        schema: Schema,
        schema_name: String,
        name: String,
        conn_config: tokio_postgres::Config,
        sender: Sender<Result<Option<Operation>, ConnectorError>>,
    ) -> Result<(), ConnectorError> {
        let mut client_plain =
            connection_helper::connect(conn_config).map_err(PostgresConnectorError)?;

        let column_str: Vec<String> = schema
            .fields
            .iter()
            .map(|f| format!("\"{0}\"", f.name))
            .collect();

        let column_str = column_str.join(",");
        let query = format!("select {column_str} from {schema_name}.{name}");
        let stmt = client_plain
            .prepare(&query)
            .map_err(|e| PostgresConnectorError(InvalidQueryError(e)))?;
        let columns = stmt.columns();

        let empty_vec: Vec<String> = Vec::new();
        for msg in client_plain
            .query_raw(&stmt, empty_vec)
            .map_err(|e| PostgresConnectorError(InvalidQueryError(e)))?
            .iterator()
        {
            match msg {
                Ok(msg) => {
                    let evt = helper::map_row_to_operation_event(
                        name.to_string(),
                        schema
                            .identifier
                            .map_or(Err(ConnectorError::SchemaIdentifierNotFound), Ok)?,
                        &msg,
                        columns,
                    )
                    .map_err(|e| PostgresConnectorError(PostgresSchemaError(e)))?;

                    sender.send(Ok(Some(evt))).unwrap();
                }
                Err(e) => return Err(PostgresConnectorError(SyncWithSnapshotError(e.to_string()))),
            }
        }

        // After table read is finished, send None as message to inform receiver loop about end of table
        sender.send(Ok(None)).unwrap();
        Ok(())
    }

    pub fn sync_tables(&self, tables: &[TableInfo]) -> Result<(), ConnectorError> {
        let schemas = self.get_tables(tables)?;

        let mut left_tables_count = tables.len();

        let (tx, rx) = unbounded();

        for (schema, table) in schemas.into_iter().zip(tables) {
            let schema = schema?;
            let schema = schema.schema;
            let schema_name = table.schema.clone().unwrap_or("public".to_string());
            let name = table.name.clone();
            let conn_config = self.conn_config.clone();
            let sender = tx.clone();
            thread::spawn(move || {
                if let Err(e) =
                    Self::sync_table(schema, schema_name, name, conn_config, sender.clone())
                {
                    sender.send(Err(e)).unwrap();
                }
            });
        }

        let mut idx = 0;
        loop {
            let message = rx
                .recv()
                .map_err(|_| PostgresConnectorError(SnapshotReadError))??;
            match message {
                None => {
                    left_tables_count -= 1;
                    if left_tables_count == 0 {
                        break;
                    }
                }
                Some(evt) => {
                    self.ingestor
                        .handle_message(IngestionMessage::new_op(0, idx, evt))
                        .map_err(ConnectorError::IngestorError)?;
                    idx += 1;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use dozer_types::{ingestion_types::IngestionMessage, node::OpIdentifier};
    use rand::Rng;
    use serial_test::serial;

    use crate::{
        connectors::{
            object_store::schema_mapper,
            postgres::{
                connection::helper::map_connection_config,
                connector::{PostgresConfig, PostgresConnector},
                tests::client::TestPostgresClient,
            },
        },
        errors::ConnectorError,
        ingestion::{IngestionConfig, Ingestor},
        test_util::run_connector_test,
    };

    use super::PostgresSnapshotter;

    #[test]
    #[ignore]
    #[serial]
    fn test_connector_snapshotter_sync_tables_successfully_1_requested_table() {
        run_connector_test("postgres", |app_config| {
            let config = app_config
                .connections
                .get(0)
                .unwrap()
                .config
                .as_ref()
                .unwrap();

            let mut test_client = TestPostgresClient::new(config);

            let mut rng = rand::thread_rng();
            let table_name = format!("test_table_{}", rng.gen::<u32>());
            let connector_name = format!("pg_connector_{}", rng.gen::<u32>());

            test_client.create_simple_table("public", &table_name);
            test_client.insert_rows(&table_name, 2, None);

            let conn_config = map_connection_config(config).unwrap();

            let postgres_config = PostgresConfig {
                name: connector_name,
                config: conn_config.clone(),
            };

            let connector = PostgresConnector::new(1, postgres_config);

            let input_tables = vec![schema_mapper::TableInfo {
                name: table_name,
                schema: Some("public".to_string()),
                columns: None,
            }];

            let ingestion_config = IngestionConfig::default();
            let (ingestor, mut iterator) = Ingestor::initialize_channel(ingestion_config);

            let snapshotter = PostgresSnapshotter {
                conn_config,
                ingestor: &ingestor,
                connector_id: connector.id,
            };

            let actual = snapshotter.sync_tables(&input_tables);

            assert!(actual.is_ok());

            let mut i = 0;
            while i < 2 {
                if let Some(IngestionMessage {
                    identifier: OpIdentifier { seq_in_tx, .. },
                    ..
                }) = iterator.next()
                {
                    assert_eq!(i, seq_in_tx);
                } else {
                    panic!("Unexpected operation");
                }
                i += 1;
            }
        })
    }

    #[test]
    #[ignore]
    #[serial]
    fn test_connector_snapshotter_sync_tables_successfully_not_match_table() {
        run_connector_test("postgres", |app_config| {
            let config = app_config
                .connections
                .get(0)
                .unwrap()
                .config
                .as_ref()
                .unwrap();

            let mut test_client = TestPostgresClient::new(config);

            let mut rng = rand::thread_rng();
            let table_name = format!("test_table_{}", rng.gen::<u32>());
            let connector_name = format!("pg_connector_{}", rng.gen::<u32>());

            test_client.create_simple_table("public", &table_name);
            test_client.insert_rows(&table_name, 2, None);

            let conn_config = map_connection_config(config).unwrap();

            let postgres_config = PostgresConfig {
                name: connector_name,
                config: conn_config.clone(),
            };

            let connector = PostgresConnector::new(1, postgres_config);

            let input_table_name = String::from("not_existing_table");
            let input_tables = vec![schema_mapper::TableInfo {
                name: input_table_name,
                schema: Some("public".to_string()),
                columns: None,
            }];

            let ingestion_config = IngestionConfig::default();
            let (ingestor, mut _iterator) = Ingestor::initialize_channel(ingestion_config);

            let snapshotter = PostgresSnapshotter {
                conn_config,
                ingestor: &ingestor,
                connector_id: connector.id,
            };

            let actual = snapshotter.sync_tables(&input_tables);

            assert!(actual.is_err());

            match actual {
                Ok(_) => panic!("Test failed"),
                Err(e) => {
                    assert!(matches!(e, ConnectorError::PostgresConnectorError(_)));
                }
            }
        })
    }

    #[test]
    #[ignore]
    #[serial]
    fn test_connector_snapshotter_sync_tables_successfully_table_not_exist() {
        run_connector_test("postgres", |app_config| {
            let config = app_config
                .connections
                .get(0)
                .unwrap()
                .config
                .as_ref()
                .unwrap();

            let mut rng = rand::thread_rng();
            let table_name = format!("test_table_{}", rng.gen::<u32>());
            let connector_name = format!("pg_connector_{}", rng.gen::<u32>());

            let conn_config = map_connection_config(config).unwrap();

            let postgres_config = PostgresConfig {
                name: connector_name,
                config: conn_config.clone(),
            };

            let connector = PostgresConnector::new(1, postgres_config);

            let input_tables = vec![schema_mapper::TableInfo {
                name: table_name,
                schema: Some("public".to_string()),
                columns: None,
            }];

            let ingestion_config = IngestionConfig::default();
            let (ingestor, mut _iterator) = Ingestor::initialize_channel(ingestion_config);

            let snapshotter = PostgresSnapshotter {
                conn_config,
                ingestor: &ingestor,
                connector_id: connector.id,
            };

            let actual = snapshotter.sync_tables(&input_tables);

            assert!(actual.is_err());

            match actual {
                Ok(_) => panic!("Test failed"),
                Err(e) => {
                    assert!(matches!(e, ConnectorError::PostgresConnectorError(_)));
                }
            }
        })
    }
}
