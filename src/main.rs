mod git;
mod graphql;
mod indexing;
mod openapi;
mod toml;

use std::{
    collections::{BTreeMap, HashMap},
    path::{self, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use async_graphql::{
    http::{playground_source, GraphQLPlaygroundConfig},
    EmptyMutation, EmptySubscription, Schema,
};
use async_graphql_poem::GraphQL;
use fbs::FlatBufferBuilder;
use figment::{
    providers::{Env, Format, Toml as FigmentToml},
    Figment,
};
use once_cell::sync::Lazy;
use pahkat_types::package::{version::SemanticVersion, Version};
use parking_lot::RwLock;
use poem::{
    get, handler, listener::TcpListener, web::Html, EndpointExt, IntoResponse, Result, Route,
};
use poem_openapi::{Object, OpenApiService};
use serde::{Deserialize, Serialize};
use structopt::StructOpt;

use uuid::Uuid;

use crate::{
    git::{GitRepo, GitRepoMutex},
    graphql::Query,
};

fn generate_010_workaround_index(
    config: &Config,
    divvun_installer_repo_path: &std::path::Path,
) -> Result<Vec<u8>, std::io::Error> {
    let dm_path = divvun_installer_repo_path
        .join("packages")
        .join("divvun-installer")
        .join("index.toml");
    let pahkat_path = divvun_installer_repo_path
        .join("packages")
        .join("pahkat-service")
        .join("index.toml");

    tracing::trace!("Attempting read to string: {:?}", &dm_path);
    let dm_file = match std::fs::read_to_string(&dm_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Could not handle path: {:?}", &dm_path);
            tracing::error!("{}", e);
            tracing::error!("Continuing.");
            return Err(e);
        }
    };
    tracing::trace!("Attempting read to string: {:?}", &pahkat_path);
    let pahkat_file = match std::fs::read_to_string(&pahkat_path) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Could not handle path: {:?}", &pahkat_path);
            tracing::error!("{}", e);
            tracing::error!("Continuing.");
            return Err(e);
        }
    };

    let mut dm_package: pahkat_types::package::Descriptor = match ::toml::from_str(&dm_file) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Could not parse: {:?}", &dm_path);
            tracing::error!("{}", e);
            tracing::error!("Continuing.");
            return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
        }
    };

    let mut pahkat_package: pahkat_types::package::Descriptor = match ::toml::from_str(&pahkat_file)
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Could not parse: {:?}", &pahkat_path);
            tracing::error!("{}", e);
            tracing::error!("Continuing.");
            return Err(std::io::Error::new(std::io::ErrorKind::Other, e));
        }
    };

    let mut windows_divvun_inst = dm_package
        .release
        .into_iter()
        .filter(|x| x.channel.is_none() && x.target.iter().any(|t| t.platform == "windows"))
        .max_by_key(|x| match &x.version {
            pahkat_types::package::Version::Semantic(v) => v.clone(),
            _ => panic!("invalid version"),
        })
        .unwrap();

    let mut pahkat_inst = pahkat_package
        .release
        .into_iter()
        .filter(|x| x.channel.is_none() && x.target.iter().any(|t| t.platform == "windows"))
        .max_by_key(|x| match &x.version {
            pahkat_types::package::Version::Semantic(v) => v.clone(),
            _ => panic!("invalid version"),
        })
        .unwrap();

    windows_divvun_inst.version = Version::Semantic(SemanticVersion::from_str("99.0.0").unwrap());
    pahkat_inst.version = Version::Semantic(SemanticVersion::from_str("99.0.0").unwrap());

    let dm_index = windows_divvun_inst
        .target
        .iter()
        .position(|x| x.platform == "windows")
        .unwrap();
    let pahkat_index = pahkat_inst
        .target
        .iter()
        .position(|x| x.platform == "windows")
        .unwrap();

    let mut dm_target = windows_divvun_inst.target[dm_index].clone();
    let mut pahkat_target = pahkat_inst.target[pahkat_index].clone();

    static NONCE: Lazy<String> = Lazy::new(|| Uuid::new_v4().to_string());
    dm_target.payload.set_url(
        format!(
            "{}/1AAB4845-32A9-41A8-BBDE-120847548A81/divvun-installer-{}.exe",
            config.url, *NONCE
        )
        .parse()
        .unwrap(),
    );
    pahkat_target.payload.set_url(
        format!(
            "{}/1AAB4845-32A9-41A8-BBDE-120847548A82/pahkat-service-{}.exe",
            config.url, *NONCE
        )
        .parse()
        .unwrap(),
    );

    windows_divvun_inst.target = vec![dm_target];
    pahkat_inst.target = vec![pahkat_target];

    dm_package.release = vec![windows_divvun_inst];
    pahkat_package.release = vec![pahkat_inst];

    let dm_pkg = pahkat_types::package::Package::Concrete(dm_package);
    let pahkat_pkg = pahkat_types::package::Package::Concrete(pahkat_package);

    let mut builder = FlatBufferBuilder::new();
    let index = indexing::build_index(&mut builder, &[dm_pkg, pahkat_pkg]).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "failed to generate flatbuffer")
    })?;
    Ok(index.to_vec())
}

fn generate_empty_index() -> Result<Vec<u8>, std::io::Error> {
    let mut builder = FlatBufferBuilder::new();
    let index = indexing::build_index(&mut builder, &[]).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "failed to generate flatbuffer")
    })?;
    Ok(index.to_vec())
}

fn generate_repo_index(
    path: &path::Path,
) -> Result<(Vec<pahkat_types::package::Package>, Vec<u8>), std::io::Error> {
    tracing::debug!("Attempting to load repo in path: {:?}", &path);
    let packages_path = path.join("packages");
    std::fs::create_dir_all(&packages_path)?;

    // Attempt to make strings directory if it doesn't exist
    let strings_path = path.join("strings");
    std::fs::create_dir_all(&strings_path)?;

    // Find all package descriptor TOMLs
    let packages = std::fs::read_dir(&*packages_path)?
        .filter_map(Result::ok)
        .filter(|x| {
            let v = x.file_type().ok().map(|x| x.is_dir()).unwrap_or(false);
            tracing::trace!("Attempting {:?} := {:?}", &x, &v);
            v
        })
        .filter_map(|x| {
            let path = x.path().join("index.toml");
            tracing::trace!("Attempting read to string: {:?}", &path);
            let file = match std::fs::read_to_string(&path) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Could not handle path: {:?}", &path);
                    tracing::error!("{}", e);
                    tracing::error!("Continuing.");
                    return None;
                }
            };
            let package: pahkat_types::package::Package = match ::toml::from_str(&file) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("Could not parse: {:?}", &path);
                    tracing::error!("{}", e);
                    tracing::error!("Continuing.");
                    return None;
                }
            };
            Some(package)
        })
        .collect::<Vec<pahkat_types::package::Package>>();

    let mut builder = FlatBufferBuilder::new();
    let index = indexing::build_index(&mut builder, &packages).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "failed to generate flatbuffer")
    })?;
    Ok((packages, index.to_vec()))
}

#[derive(Debug, Clone, Object)]
struct ServerStatus {
    index_ref: BTreeMap<String, String>,
}

pub(crate) fn server_status(repo_indexes: &RepoIndexes) -> BTreeMap<String, String> {
    repo_indexes
        .iter()
        .map(|(k, v)| (k.clone(), v.load().0.clone()))
        .collect::<BTreeMap<_, _>>()
}

async fn refresh_indexes(
    git_repo_mutex: GitRepoMutex,
    repo_indexes: RepoIndexes,
) -> Result<(), std::io::Error> {
    let (tmpdir, head_ref) = {
        let guard = git_repo_mutex.read();
        (guard.shallow_clone_to_tempdir()?, guard.head_ref.clone())
    };

    for (repo_id, state) in repo_indexes.iter() {
        tracing::debug!("Index check for: {}", repo_id);
        let s = state.load();
        if s.0 != head_ref {
            tracing::info!("Updating index for {}", repo_id);
            let fbs_data = generate_repo_index(&tmpdir.path().join(repo_id)).unwrap();
            state.swap(Arc::new((head_ref.clone(), fbs_data)));
            tracing::info!("Finished updating index for {}", repo_id);
        }
    }

    Ok(())
}

async fn refresh_indexes_forever(
    config: Config,
    git_repo_mutex: GitRepoMutex,
    repo_indexes: RepoIndexes,
) {
    loop {
        tracing::debug!("Sleeping for {} seconds", config.index_interval);
        tokio::time::sleep(Duration::from_secs(config.index_interval)).await;
        match refresh_indexes(git_repo_mutex.clone(), repo_indexes.clone()).await {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = ?e, "Error while refreshing indexes");
            }
        };
    }
}

type RepoIndexes =
    Arc<HashMap<String, ArcSwap<(String, (Vec<pahkat_types::package::Package>, Vec<u8>))>>>;

#[handler]
async fn graphql_playground() -> impl IntoResponse {
    Html(playground_source(GraphQLPlaygroundConfig::new("/graphql")))
}

async fn run(config: Config) -> Result<(), std::io::Error> {
    let git_repo_mutex: GitRepoMutex = Arc::new(RwLock::new(GitRepo::new(config.git_path.clone())));

    tracing::info!("Cleaning up repo state...");
    {
        let guard = git_repo_mutex.write();
        guard.cleanup(&config)?;
    }

    let repo_indexes: RepoIndexes = Arc::new(
        config
            .repos
            .iter()
            .map(|r| {
                (
                    r.to_string(),
                    ArcSwap::new(Arc::new(("".to_string(), (vec![], vec![])))),
                )
            })
            .collect(),
    );

    refresh_indexes(git_repo_mutex.clone(), repo_indexes.clone()).await?;

    tokio::spawn(refresh_indexes_forever(
        config.clone(),
        git_repo_mutex.clone(),
        repo_indexes.clone(),
    ));

    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
        .data(repo_indexes.clone())
        .data(config.clone())
        .finish();

    let api_service = OpenApiService::new(
        openapi::Api,
        "Pahkat Repository Server",
        env!("CARGO_PKG_VERSION"),
    )
    .server(&config.url);
    let ui = api_service.rapidoc();
    let app = Route::new()
        .nest("/", api_service)
        .nest("/playground", ui)
        .at(
            "/graphql",
            get(graphql_playground).post(GraphQL::new(schema)),
        )
        .data(config.clone())
        .data(git_repo_mutex)
        .data(repo_indexes)
        .data(openapi::ServerToken(config.api_token.clone()));

    poem::Server::new(TcpListener::bind((config.host, config.port)))
        .run(app)
        .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    api_token: String,
    git_path: PathBuf,
    repos: Vec<String>,
    url: String,
    host: String,
    port: u16,
    index_interval: u64,
    #[serde(default = "default_branch_name")]
    branch_name: String,
}

fn default_branch_name() -> String {
    "main".to_string()
}

#[derive(StructOpt)]
struct Args {
    #[structopt(short, long)]
    config_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    tracing::info!("starting pahkat-reposrv");

    let args = Args::from_args();

    let mut figment = Figment::new();
    if let Some(config_path) = args.config_path {
        figment = figment.merge(FigmentToml::file(config_path));
    }

    let config: Config = figment.merge(Env::raw()).extract()?;

    Ok(run(config).await?)
}
