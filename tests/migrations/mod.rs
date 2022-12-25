///! Integration tests for the migrations feature of `Canyon-SQL`
use canyon_sql::{crud::Transaction, migrations::handler::Migrations};

use crate::constants;

/// Brings the information of the `PostgreSQL` requested schema
#[canyon_sql::macros::canyon_tokio_test]
fn test_migrations_postgresql_status_query() {
    let results = Migrations::query(constants::FETCH_PUBLIC_SCHEMA, &[], constants::PSQL_DS).await;
    assert!(!results.is_err());

    let public_schema_info = results.ok().unwrap().postgres;

    let first_result = public_schema_info.get(0).unwrap();

    assert_eq!(first_result.columns().get(0).unwrap().name(), "table_name");
    assert_eq!(
        first_result.columns().get(0).unwrap().type_().name(),
        "name"
    );
    assert_eq!(first_result.columns().get(0).unwrap().type_().oid(), 19);
    assert_eq!(
        first_result.columns().get(0).unwrap().type_().schema(),
        "pg_catalog"
    );
}
