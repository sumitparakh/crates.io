use crate::config::CdnLogStorageConfig;
use crate::db::DieselPool;
use crate::tasks::spawn_blocking;
use crate::worker::Environment;
use anyhow::Context;
use chrono::NaiveDate;
use crates_io_cdn_logs::{count_downloads, Decompressor, DownloadsMap};
use crates_io_worker::BackgroundJob;
use diesel::prelude::*;
use diesel::{PgConnection, QueryResult};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::ObjectStore;
use semver::Version;
use std::cmp::Reverse;
use std::fmt::Debug;
use std::sync::Arc;
use tokio::io::BufReader;

/// A background job that loads a CDN log file from an object store (aka. S3),
/// counts the number of downloads for each crate and version, and then inserts
/// the results into the database.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessCdnLog {
    pub region: String,
    pub bucket: String,
    pub path: String,
}

impl ProcessCdnLog {
    pub fn new(region: String, bucket: String, path: String) -> Self {
        Self {
            region,
            bucket,
            path,
        }
    }
}

impl BackgroundJob for ProcessCdnLog {
    const JOB_NAME: &'static str = "process_cdn_log";

    type Context = Arc<Environment>;

    async fn run(&self, ctx: Self::Context) -> anyhow::Result<()> {
        // The store is rebuilt for each run because we don't want to assume
        // that all log files live in the same AWS region or bucket, and those
        // two pieces are necessary for the store construction.
        let store = build_store(&ctx.config.cdn_log_storage, &self.region, &self.bucket)
            .context("Failed to build object store")?;

        let db_pool = ctx.connection_pool.clone();
        let writing_enabled = ctx.config.cdn_log_counting_enabled;
        run(store, &self.path, db_pool, writing_enabled).await
    }
}

/// Builds an object store based on the [CdnLogStorageConfig] and the
/// `region` and `bucket` arguments.
///
/// If the passed in [CdnLogStorageConfig] is using local file or in-memory
/// storage the `region` and `bucket` arguments are ignored.
fn build_store(
    config: &CdnLogStorageConfig,
    region: impl Into<String>,
    bucket: impl Into<String>,
) -> anyhow::Result<Arc<dyn ObjectStore>> {
    match config {
        CdnLogStorageConfig::S3 {
            access_key,
            secret_key,
        } => {
            use secrecy::ExposeSecret;

            let store = AmazonS3Builder::new()
                .with_region(region.into())
                .with_bucket_name(bucket.into())
                .with_access_key_id(access_key)
                .with_secret_access_key(secret_key.expose_secret())
                .build()?;

            Ok(Arc::new(store))
        }
        CdnLogStorageConfig::Local { path } => {
            Ok(Arc::new(LocalFileSystem::new_with_prefix(path)?))
        }
        CdnLogStorageConfig::Memory => Ok(Arc::new(InMemory::new())),
    }
}

/// Loads the given log file from the object store and counts the number of
/// downloads for each crate and version. The results are printed to the log.
///
/// This function is separate from the [`BackgroundJob`] trait method so that
/// it can be tested without having to construct a full [`Environment`]
/// struct.
#[instrument(skip_all, fields(cdn_log_store.path = %path))]
async fn run(
    store: Arc<dyn ObjectStore>,
    path: &str,
    db_pool: DieselPool,
    writing_enabled: bool,
) -> anyhow::Result<()> {
    let path = Path::parse(path).with_context(|| format!("Failed to parse path: {path:?}"))?;

    let downloads = load_and_count(&path, store).await?;
    if downloads.is_empty() {
        info!("No downloads found in log file");
        return Ok(());
    }

    log_stats(&downloads);

    if writing_enabled {
        spawn_blocking(move || {
            let mut conn = db_pool.get()?;
            conn.transaction(|conn| save_downloads(downloads, conn))?;

            Ok::<_, anyhow::Error>(())
        })
        .await?;
    } else {
        log_top_downloads(downloads, 30);
    }

    Ok(())
}

/// Loads the given log file from the object store and counts the number of
/// downloads for each crate and version.
async fn load_and_count(path: &Path, store: Arc<dyn ObjectStore>) -> anyhow::Result<DownloadsMap> {
    let meta = store.head(path).await;
    let meta = meta.with_context(|| format!("Failed to request metadata for {path:?}"))?;

    let reader = object_store::buffered::BufReader::new(store, &meta);
    let decompressor = Decompressor::from_extension(reader, path.extension())?;
    let reader = BufReader::new(decompressor);

    count_downloads(reader).await
}

/// Prints the total number of downloads, the number of crates, and the number
/// of needed inserts to the log.
fn log_stats(downloads: &DownloadsMap) {
    let total_downloads = downloads.sum_downloads();
    info!("Total number of downloads: {total_downloads}");

    let num_crates = downloads.unique_crates().len();
    info!("Number of crates: {num_crates}");

    let total_inserts = downloads.len();
    info!("Number of needed inserts: {total_inserts}");
}

/// Prints the top `num` downloads from the given [`DownloadsMap`] map to the log.
fn log_top_downloads(downloads: DownloadsMap, num: usize) {
    let mut downloads = downloads.into_vec();
    downloads.sort_by_key(|(_, _, _, downloads)| Reverse(*downloads));

    let top_downloads = downloads
        .into_iter()
        .take(num)
        .map(|(krate, version, date, downloads)| {
            format!("{date}  {krate}@{version} .. {downloads}")
        })
        .collect::<Vec<_>>();

    info!("Top {num} downloads: {top_downloads:?}");
}

table! {
    /// Diesel table definition for the temporary `temp_downloads` table that is
    /// created by the [`create_temp_downloads_table`] function.
    ///
    /// The primary key does not actually exist, but specifying one is
    /// required by Diesel.
    temp_downloads (name, version, date) {
        name -> Text,
        version -> Text,
        date -> Date,
        downloads -> BigInt,
    }
}

/// Helper struct for inserting downloads into the `temp_downloads` table.
#[derive(Insertable)]
#[diesel(table_name = temp_downloads)]
struct NewDownload {
    name: String,
    version: String,
    date: NaiveDate,
    downloads: i64,
}

impl From<(String, Version, NaiveDate, u64)> for NewDownload {
    fn from((name, version, date, downloads): (String, Version, NaiveDate, u64)) -> Self {
        Self {
            name,
            version: version.to_string(),
            date,
            downloads: downloads as i64,
        }
    }
}

/// Saves the downloads from the given [`DownloadsMap`] to the database into
/// the `version_downloads` table.
///
/// This function **should be run inside a transaction** to ensure that the
/// temporary `temp_downloads` table is dropped after the inserts are
/// completed!
///
/// The temporary table only exists on the current connection, but if a
/// connection pool is used, the temporary table will not be dropped when
/// the connection is returned to the pool.
pub fn save_downloads(downloads: DownloadsMap, conn: &mut PgConnection) -> anyhow::Result<()> {
    debug!("Creating temp_downloads table");
    create_temp_downloads_table(conn).context("Failed to create temp_downloads table")?;

    debug!("Saving counted downloads to temp_downloads table");
    fill_temp_downloads_table(downloads, conn).context("Failed to fill temp_downloads table")?;

    debug!("Saving temp_downloads to version_downloads table");
    let failed_inserts = save_to_version_downloads(conn)
        .context("Failed to save temp_downloads to version_downloads table")?;

    if !failed_inserts.is_empty() {
        warn!(
            "Failed to insert downloads for the following crates and versions: {failed_inserts:?}"
        );
    }

    Ok(())
}

/// Creates the temporary `temp_downloads` table that is used to store the
/// counted downloads before they are inserted into the `version_downloads`
/// table.
///
/// We can't insert directly into `version_downloads` table because we need to
/// look up the `version_id` for each crate and version combination, and that
/// requires a join with the `crates` and `versions` tables.
#[instrument("db.query", skip_all, fields(message = "CREATE TEMPORARY TABLE ..."))]
fn create_temp_downloads_table(conn: &mut PgConnection) -> QueryResult<usize> {
    diesel::sql_query(
        r#"
            CREATE TEMPORARY TABLE temp_downloads (
                name VARCHAR NOT NULL,
                version VARCHAR NOT NULL,
                date DATE NOT NULL,
                downloads INTEGER NOT NULL
            ) ON COMMIT DROP;
        "#,
    )
    .execute(conn)
}

/// Fills the temporary `temp_downloads` table with the downloads from the
/// given [`DownloadsMap`].
#[instrument(
    "db.query",
    skip_all,
    fields(message = "INSERT INTO temp_downloads ...")
)]
fn fill_temp_downloads_table(
    downloads: DownloadsMap,
    conn: &mut PgConnection,
) -> QueryResult<usize> {
    let map = downloads
        .into_vec()
        .into_iter()
        .map(NewDownload::from)
        .collect::<Vec<_>>();

    diesel::insert_into(temp_downloads::table)
        .values(map)
        .execute(conn)
}

/// Saves the downloads from the temporary `temp_downloads` table to the
/// `version_downloads` table and returns the name/version combinations that
/// were not found in the database.
#[instrument(
    "db.query",
    skip_all,
    fields(message = "INSERT INTO version_downloads ...")
)]
fn save_to_version_downloads(conn: &mut PgConnection) -> QueryResult<Vec<NameAndVersion>> {
    diesel::sql_query(
        r#"
            WITH joined_data AS (
                SELECT versions.id, temp_downloads.*
                FROM temp_downloads
                LEFT JOIN crates ON crates.name = temp_downloads.name
                LEFT JOIN versions ON versions.num = temp_downloads.version AND versions.crate_id = crates.id
            ), inserted AS (
                INSERT INTO version_downloads (version_id, date, downloads)
                SELECT joined_data.id, joined_data.date, joined_data.downloads
                FROM joined_data
                WHERE joined_data.id IS NOT NULL
                ON CONFLICT (version_id, date)
                DO UPDATE SET downloads = version_downloads.downloads + EXCLUDED.downloads
                RETURNING version_downloads.version_id
            )
            SELECT joined_data.name, joined_data.version
            FROM joined_data
            WHERE joined_data.id IS NULL;
        "#,
    )
        .load(conn)
}

table! {
    /// Imaginary table to make Diesel happy when using the `sql_query` macro in
    /// the [`save_to_version_downloads()`] function.
    name_and_versions (name, version) {
        name -> Text,
        version -> Text,
    }
}

/// A helper struct for the result of the query in the
/// [`save_to_version_downloads()`] function.
///
/// The result of `sql_query` can not be a tuple, so we have to define a
/// proper struct for the result.
#[derive(QueryableByName)]
struct NameAndVersion {
    name: String,
    version: String,
}

impl Debug for NameAndVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{crates, version_downloads, versions};
    use crates_io_test_db::TestDatabase;
    use diesel::r2d2::{ConnectionManager, Pool};
    use insta::assert_debug_snapshot;

    const CLOUDFRONT_PATH: &str =
        "cloudfront/static.crates.io/E35K556QRQDZXW.2024-01-16-16.d01d5f13.gz";

    #[tokio::test]
    async fn test_process_cdn_log() {
        let _guard = crate::util::tracing::init_for_test();

        let test_database = TestDatabase::new();
        let db_pool = build_connection_pool(test_database.url());
        create_dummy_crates_and_versions(db_pool.clone()).await;

        let store = build_dummy_store().await;

        let writing_enabled = true;
        assert_ok!(run(store, CLOUDFRONT_PATH, db_pool.clone(), writing_enabled).await);
        assert_debug_snapshot!(all_version_downloads(db_pool).await, @r###"
        [
            "bindgen | 0.65.1 | 1 | 0 | 2024-01-16 | false",
            "quick-error | 1.2.3 | 2 | 0 | 2024-01-16 | false",
            "quick-error | 1.2.3 | 1 | 0 | 2024-01-17 | false",
            "tracing-core | 0.1.32 | 1 | 0 | 2024-01-16 | false",
        ]
        "###);
    }

    #[tokio::test]
    async fn test_process_cdn_log_report_only() {
        let _guard = crate::util::tracing::init_for_test();

        let test_database = TestDatabase::new();
        let db_pool = build_connection_pool(test_database.url());
        create_dummy_crates_and_versions(db_pool.clone()).await;

        let store = build_dummy_store().await;

        let writing_enabled = false;
        assert_ok!(run(store, CLOUDFRONT_PATH, db_pool.clone(), writing_enabled).await);
        assert_debug_snapshot!(all_version_downloads(db_pool).await, @"[]");
    }

    #[test]
    fn test_build_store_s3() {
        let access_key = "access_key".into();
        let secret_key = "secret_key".to_string().into();
        let config = CdnLogStorageConfig::s3(access_key, secret_key);
        assert_ok!(build_store(&config, "us-west-1", "bucket"));
    }

    #[test]
    fn test_build_store_local() {
        let path = std::env::current_dir().unwrap();
        let config = CdnLogStorageConfig::local(path);
        assert_ok!(build_store(&config, "us-west-1", "bucket"));
    }

    #[test]
    fn test_build_store_memory() {
        let config = CdnLogStorageConfig::memory();
        assert_ok!(build_store(&config, "us-west-1", "bucket"));
    }

    /// Builds a dummy object store with a log file in it.
    async fn build_dummy_store() -> Arc<dyn ObjectStore> {
        let store = InMemory::new();

        // Add dummy data to the store
        let path = CLOUDFRONT_PATH.into();
        let bytes =
            include_bytes!("../../../../crates_io_cdn_logs/test_data/cloudfront/basic.log.gz");

        store.put(&path, bytes[..].into()).await.unwrap();

        Arc::new(store)
    }

    /// Builds a connection pool to the test database.
    fn build_connection_pool(url: &str) -> DieselPool {
        let pool = Pool::builder().build(ConnectionManager::new(url)).unwrap();
        DieselPool::new_background_worker(pool)
    }

    /// Inserts some dummy crates and versions into the database.
    async fn create_dummy_crates_and_versions(db_pool: DieselPool) {
        spawn_blocking(move || {
            let mut conn = db_pool.get().unwrap();

            create_crate_and_version("bindgen", "0.65.1", &mut conn);
            create_crate_and_version("tracing-core", "0.1.32", &mut conn);
            create_crate_and_version("quick-error", "1.2.3", &mut conn);

            Ok::<_, anyhow::Error>(())
        })
        .await
        .unwrap();
    }

    /// Inserts a dummy crate and version into the database.
    fn create_crate_and_version(name: &str, version: &str, conn: &mut PgConnection) {
        let crate_id: i32 = diesel::insert_into(crates::table)
            .values(crates::name.eq(name))
            .returning(crates::id)
            .get_result(conn)
            .unwrap();

        diesel::insert_into(versions::table)
            .values((
                versions::crate_id.eq(crate_id),
                versions::num.eq(version),
                versions::checksum.eq("checksum"),
            ))
            .execute(conn)
            .unwrap();
    }

    /// Queries all version downloads from the database and returns them as a
    /// [`Vec`] of strings for use with [`assert_debug_snapshot!()`].
    async fn all_version_downloads(db_pool: DieselPool) -> Vec<String> {
        let downloads = spawn_blocking(move || {
            let mut conn = db_pool.get().unwrap();
            Ok::<_, anyhow::Error>(query_all_version_downloads(&mut conn))
        })
        .await
        .unwrap();

        downloads
            .into_iter()
            .map(|(name, version, downloads, counted, date, processed)| {
                format!("{name} | {version} | {downloads} | {counted} | {date} | {processed}")
            })
            .collect()
    }

    /// Queries all version downloads from the database and returns them as a
    /// [`Vec`] of tuples.
    fn query_all_version_downloads(
        conn: &mut PgConnection,
    ) -> Vec<(String, String, i32, i32, NaiveDate, bool)> {
        version_downloads::table
            .inner_join(versions::table)
            .inner_join(crates::table.on(versions::crate_id.eq(crates::id)))
            .select((
                crates::name,
                versions::num,
                version_downloads::downloads,
                version_downloads::counted,
                version_downloads::date,
                version_downloads::processed,
            ))
            .order((crates::name, versions::num, version_downloads::date))
            .load(conn)
            .unwrap()
    }
}
