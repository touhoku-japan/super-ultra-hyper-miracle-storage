//! Picks the metadata backend for tests: a fresh database on the server named by
//! `SUHMS_TEST_POSTGRES` (e.g. `postgres://postgres:pw@127.0.0.1:5432`) or `SUHMS_TEST_MYSQL`
//! (e.g. `mysql://root:pw@127.0.0.1:3306`), otherwise a SQLite file in `dir`.

#![allow(dead_code)]

use std::path::Path;

use sqlx::{AssertSqlSafe, Connection};

pub async fn metadata_url(dir: &Path) -> String {
    let db = format!("t{}", uuid::Uuid::new_v4().simple());
    if let Ok(base) = std::env::var("SUHMS_TEST_POSTGRES") {
        let mut c = sqlx::PgConnection::connect(&format!("{base}/postgres"))
            .await
            .expect("connect to SUHMS_TEST_POSTGRES");
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {db}")))
            .execute(&mut c)
            .await
            .unwrap();
        return format!("{base}/{db}");
    }
    if let Ok(base) = std::env::var("SUHMS_TEST_MYSQL") {
        let mut c = sqlx::MySqlConnection::connect(&base)
            .await
            .expect("connect to SUHMS_TEST_MYSQL");
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {db}")))
            .execute(&mut c)
            .await
            .unwrap();
        return format!("{base}/{db}");
    }
    format!("sqlite://{}", dir.join("meta.db").display())
}

/// Runs a query returning one integer (e.g. `SELECT COUNT(*) ...`) on the test database.
pub async fn query_i64(url: &str, sql: &str) -> i64 {
    let sql = AssertSqlSafe(sql.to_owned());
    if url.starts_with("postgres") {
        let mut c = sqlx::PgConnection::connect(url).await.unwrap();
        sqlx::query_scalar(sql).fetch_one(&mut c).await.unwrap()
    } else if url.starts_with("mysql") {
        let mut c = sqlx::MySqlConnection::connect(url).await.unwrap();
        sqlx::query_scalar(sql).fetch_one(&mut c).await.unwrap()
    } else {
        let mut c = sqlx::SqliteConnection::connect(url).await.unwrap();
        sqlx::query_scalar(sql).fetch_one(&mut c).await.unwrap()
    }
}
