use crate::config::Config;
use crate::model::types::api::PublishOperationInfo;
use crate::model::types::{api, index};
use axum::body::Bytes;
use axum::http::StatusCode;
use deadpool_postgres::tokio_postgres::types::ToSql;
use deadpool_postgres::tokio_postgres::{IsolationLevel, NoTls};
use deadpool_postgres::{GenericClient, Pool, Runtime};
use s3::Bucket;
use semver::{Version, VersionReq};
use std::collections::HashMap;

pub mod types;

pub struct ServiceState {
    pub config: Config,
    pool: Pool,
    bucket: Bucket,
}

impl ServiceState {
    pub fn new(config: Config) -> Self {
        let pool = config.db.create_pool(Some(Runtime::Tokio1), NoTls).unwrap();

        let bucket = Bucket::new(
            &config.store.name,
            config.store.region.clone(),
            config.store.credentials.clone(),
        )
        .unwrap();

        Self {
            config,
            pool,
            bucket,
        }
    }

    pub async fn get_sparse_metadata(&self, crate_name: &str) -> Option<Vec<index::CrateVersion>> {
        let client = self.pool.get().await.unwrap();

        // prepare these at once to take advantage of pipelining
        let (existential_statement, versions_statement, features_statement, dependencies_statement) =
            tokio::try_join!(
                client.prepare_cached(include_str!("../../sql/sparse-index/get-crate.sql")),
                client.prepare_cached(include_str!("../../sql/sparse-index/get-versions.sql")),
                client.prepare_cached(include_str!("../../sql/sparse-index/get-features.sql")),
                client.prepare_cached(include_str!("../../sql/sparse-index/get-dependencies.sql"))
            )
            .unwrap();

        if let Ok(crate_row) = client
            .query_one(&existential_statement, &[&crate_name])
            .await
        {
            tracing::info!(?crate_row);
            let id: i32 = crate_row.get("id");

            // this is a major hotpath
            let version_rows = client.query(&versions_statement, &[&id]).await.unwrap();

            let mut versions = Vec::with_capacity(version_rows.len());

            // todo maybe look at running all of this concurrently for pipelining purposes
            for version_row in version_rows {
                let version_id: i32 = version_row.get("id");

                // this shouldn't be necessary but it is nonetheless
                let version_id_query = [&version_id as &(dyn ToSql + Sync)];

                // pipeline the queries
                let (feature_rows, dependency_rows) = tokio::try_join!(
                    client.query(&features_statement, &version_id_query),
                    client.query(&dependencies_statement, &version_id_query)
                )
                .unwrap();

                let mut features = HashMap::with_capacity(feature_rows.len());
                let mut deps = Vec::with_capacity(dependency_rows.len());

                for feature_row in feature_rows {
                    features.insert(feature_row.get("name"), feature_row.get("values"));
                }

                for deps_row in dependency_rows {
                    deps.push(index::Dependency {
                        name: deps_row.get("name"),
                        req: VersionReq::parse(deps_row.get("req")).unwrap(),
                        features: deps_row.get("features"),
                        optional: deps_row.get("optional"),
                        default_features: deps_row.get("default_features"),
                        target: deps_row.get("target"),
                        kind: deps_row.get("kind"),
                        registry: deps_row.get("registry"),
                        package: deps_row.get("package"),
                    });
                }

                versions.push(index::CrateVersion {
                    name: crate_name.to_string(),
                    vers: Version::parse(version_row.get("version")).unwrap(),
                    deps,
                    cksum: version_row.get("cksum"),
                    features,
                    yanked: version_row.get("yanked"),
                    links: version_row.get("links"),
                    v: 2,
                    // todo maybe scrap
                    features2: HashMap::new(),
                });
            }

            Some(versions)
        } else {
            None
        }
    }

    pub async fn publish_crate(
        &self,
        version: &api::Publish,
        checksum: &str,
        crate_bytes: &[u8],
    ) -> Result<PublishOperationInfo, StatusCode> {
        let mut client = self.pool.get().await.unwrap();

        let transaction = client
            .build_transaction()
            .isolation_level(IsolationLevel::ReadCommitted)
            .start()
            .await
            .unwrap();

        let (
            get_or_insert_crate_statement,
            insert_version_statement,
            insert_dependency_statement,
            insert_features_statement,
        ) = tokio::try_join!(
            transaction.prepare_cached(include_str!("../../sql/publish/insert-crate.sql")),
            transaction.prepare_cached(include_str!("../../sql/publish/insert-version.sql")),
            transaction.prepare_cached(include_str!("../../sql/publish/insert-dependency.sql")),
            transaction.prepare_cached(include_str!("../../sql/publish/insert-features.sql")),
        )
        .unwrap();

        if let Ok(crate_id_row) = transaction
            .query_one(&get_or_insert_crate_statement, &[&version.name])
            .await
        {
            let crate_id: i32 = crate_id_row.get("id");

            if let Ok(insert_version_row) = transaction
                .query_one(
                    &insert_version_statement,
                    &[
                        &crate_id,
                        &version.vers.to_string(),
                        &checksum,
                        &false,
                        &version.links,
                    ],
                )
                .await
            {
                let version_id: i32 = insert_version_row.get("id");

                for dependency in version.deps.iter() {
                    if let Err(error) = transaction
                        .query_one(
                            &insert_dependency_statement,
                            &[
                                &dependency.name,
                                &dependency.registry,
                                &version_id,
                                &dependency.version_req.to_string(),
                                &dependency.features,
                                &dependency.optional,
                                &dependency.default_features,
                                &dependency.target,
                                &dependency.kind,
                                &dependency.explicit_name_in_toml,
                            ],
                        )
                        .await
                    {
                        tracing::error!(?error, "Failed to insert dependency");
                        transaction.rollback().await.unwrap();

                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }

                for feature in version.features.iter() {
                    if transaction
                        .query_one(
                            &insert_features_statement,
                            &[&version_id, &feature.0, &feature.1],
                        )
                        .await
                        .is_err()
                    {
                        tracing::error!("Failed to insert feature");
                        transaction.rollback().await.unwrap();

                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                }

                let obj_path = format!("{}-{}.crate", &version.name, &version.vers);

                if let Err(error) = self.bucket.put_object(obj_path, crate_bytes).await {
                    tracing::error!(?error, "Failed to upload to store");
                    transaction.rollback().await.unwrap();

                    return Err(StatusCode::INTERNAL_SERVER_ERROR);
                }

                if transaction.commit().await.is_ok() {
                    Ok(PublishOperationInfo { warnings: None })
                } else {
                    tracing::error!("Failed to commit transaction");
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            } else {
                tracing::error!("Failed to insert version");
                transaction.rollback().await.unwrap();

                Err(StatusCode::CONFLICT)
            }
        } else {
            tracing::error!("Failed to insert or get crate");
            transaction.rollback().await.unwrap();

            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }

    pub async fn download_crate(&self, name: &str) -> Option<Bytes> {
        self.bucket
            .get_object(name)
            .await
            .ok()
            .map(|x| x.bytes().clone())
    }
}