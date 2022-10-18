mod git;
mod graphql;
mod indexing;
mod openapi;
mod state;
mod toml;

use std::{
    collections::HashMap,
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
use poem_openapi::OpenApiService;
use serde::{Deserialize, Serialize};
use state::GIT_REPO;
use structopt::StructOpt;

use uuid::Uuid;

use crate::{
    git::GitRepo,
    graphql::Query,
    state::{init_repo_indexes, set_repo_indexes, REPO_INDEXES},
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
    head_ref: Arc<str>,
    path: &path::Path,
) -> Result<RepoIndexData, std::io::Error> {
    tracing::debug!("Attempting to load repo in path: {:?}", &path);

    let index_path = path.join("index.toml");
    let repo_index = std::fs::read_to_string(index_path).unwrap();
    let repo_index = ::toml::from_str(&repo_index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

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

    Ok(RepoIndexData {
        head_ref,
        packages: Arc::from(packages),
        repo_index: Arc::new(repo_index),
        package_index: Arc::from(index.to_vec()),
    })
}

async fn refresh_indexes(
    git_repo_mutex: &RwLock<GitRepo>,
    repo_indexes: &RepoIndexes,
) -> Result<(), std::io::Error> {
    let (tmpdir, head_ref) = {
        let guard = git_repo_mutex.read();
        (guard.shallow_clone_to_tempdir()?, guard.head_ref.clone())
    };
    let head_ref = Arc::from(head_ref);

    for (repo_id, state) in repo_indexes.iter() {
        tracing::debug!("Index check for: {}", repo_id);
        let s = state.load();
        if s.head_ref != head_ref {
            tracing::info!("Updating index for {}", repo_id);
            let repo_index_data =
                generate_repo_index(head_ref.clone(), &tmpdir.path().join(repo_id)).unwrap();
            set_repo_indexes(state, repo_index_data);
            tracing::info!("Finished updating index for {}", repo_id);
        }
    }

    Ok(())
}

async fn refresh_indexes_forever(
    config: Config,
    git_repo_mutex: &RwLock<GitRepo>,
    repo_indexes: &RepoIndexes,
) {
    loop {
        tracing::debug!("Sleeping for {} seconds", config.index_interval);
        tokio::time::sleep(Duration::from_secs(config.index_interval)).await;
        match refresh_indexes(git_repo_mutex, repo_indexes).await {
            Ok(_) => {}
            Err(e) => {
                tracing::error!(error = ?e, "Error while refreshing indexes");
            }
        };
    }
}

#[derive(Debug)]
struct RepoIndexData {
    head_ref: Arc<str>,
    packages: Arc<[pahkat_types::package::Package]>,
    repo_index: Arc<pahkat_types::repo::Index>,
    package_index: Arc<[u8]>,
}

type RepoIndex = ArcSwap<RepoIndexData>;
type RepoIndexes = Arc<HashMap<String, RepoIndex>>;

#[handler]
async fn graphql_playground() -> impl IntoResponse {
    Html(playground_source(GraphQLPlaygroundConfig::new("/graphql")))
}

async fn run(config: Config) -> Result<(), std::io::Error> {
    init_repo_indexes(&config)?;

    // refresh_indexes(GIT_REPO.get().unwrap(), REPO_INDEXES.get().unwrap()).await?;

    tokio::spawn(refresh_indexes_forever(
        config.clone(),
        GIT_REPO.get().unwrap(),
        REPO_INDEXES.get().unwrap(),
    ));

    let schema = Schema::build(Query, EmptyMutation, EmptySubscription)
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
        .data(openapi::ServerToken(config.api_token.clone()));

    poem::Server::new(TcpListener::bind((config.host, config.port)))
        .run(app)
        .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// API token used by GraphQL API for mutations
    api_token: String,

    /// Local path to Pahkat git repos to host
    git_path: PathBuf,

    /// The names of the repositories to host
    repos: Vec<String>,

    /// The host URL prefix for this server
    url: String,

    /// IP/hostname (may be different to URL)
    host: String,

    /// Port
    port: u16,

    /// How often to re-index the git repositories
    index_interval: u64,

    /// Branch name (default: main)
    #[serde(default = "default_branch_name")]
    branch_name: String,

    /// Skip git repo clean-up (useful for development)
    #[serde(default)]
    skip_repo_cleanup: bool,
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
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("starting pahkat-reposrv");

    let args = Args::from_args();

    let mut figment = Figment::new();
    if let Some(config_path) = args.config_path {
        figment = figment.merge(FigmentToml::file(config_path));
    }

    let config: Config = match figment.merge(Env::raw()).extract() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Could not load config:");
            return Err(e.into());
        }
    };

    Ok(run(config).await?)
}
