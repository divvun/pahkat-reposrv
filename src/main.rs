mod indexing;
mod toml;
mod graphql;

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fmt::Display,
    path::{self, PathBuf},
    process::Command,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use async_graphql::{Schema, EmptyMutation, EmptySubscription, http::{GraphQLPlaygroundConfig, playground_source}};
use async_graphql_poem::GraphQL;
use chrono::{DateTime, Utc};
use fbs::FlatBufferBuilder;
use figment::{
    providers::{Env, Format, Toml as FigmentToml},
    Figment,
};
use once_cell::sync::Lazy;
use pahkat_repomgr::package;
use pahkat_types::{
    package::{version::SemanticVersion, Descriptor, DescriptorData, Release, Version},
    package_key::PackageKeyParams,
    payload::Target,
};
use parking_lot::RwLock;
use poem::{
    error::{BadRequest, Conflict, InternalServerError, NotFoundError},
    http::StatusCode,
    listener::TcpListener,
    web::{headers::UserAgent, Data, Html},
    EndpointExt, Request, Result, Route, get, IntoResponse, handler,
};
use poem_openapi::{
    auth::Bearer,
    param::{Header, Path},
    payload::{Binary, Json, Response},
    Object, OpenApi, OpenApiService, SecurityScheme,
};
use serde::{Deserialize, Serialize};
use structopt::StructOpt;
use tempfile::TempDir;
use uuid::Uuid;

use crate::graphql::Query;

use self::toml::Toml;

struct Api;

#[derive(Object, Debug, Clone)]
struct Error {
    id: String,
    message: String,
}

#[derive(Object, Debug, Clone)]
struct UpdatePackageMetadataResponse {
    repo_id: String,
    package_id: String,
    success: bool,
    error: Option<Error>,
    timestamp: DateTime<Utc>,
}

#[derive(Object, Debug, Clone)]
pub struct UpdatePackageMetadataRequest {
    pub version: String,
    pub channel: Option<String>,
    #[oai(default)]
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub license_url: Option<String>,
    pub target: pahkat_types::payload::Target,
}

#[derive(Object, Debug, Clone)]
pub struct CreatePackageMetadataRequest {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Object, Debug, Clone)]
pub struct CreatePackageMetadataResponse {
    repo_id: String,
    package_id: String,
    success: bool,
    error: Option<Error>,
    timestamp: DateTime<Utc>,
}

impl Display for UpdatePackageMetadataRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{} {} ({})",
            self.version,
            self.target.platform,
            self.channel.as_deref().unwrap_or("stable")
        ))
    }
}

#[derive(SecurityScheme)]
#[oai(type = "bearer", checker = "check_bearer_token")]
struct BearerTokenAuth(());

async fn check_bearer_token(req: &Request, bearer: Bearer) -> Option<()> {
    let token = &req.data::<ServerToken>().expect("server token").0;

    if &bearer.token == token {
        Some(())
    } else {
        None
    }
}

#[derive(Debug, thiserror::Error)]
enum PackageUpdateError {
    #[error("Invalid version provided")]
    VersionError(#[from] pahkat_types::package::version::Error),

    #[error("Repo error: {0}")]
    RepoError(#[source] package::update::Error),
}

#[derive(Debug, thiserror::Error)]
#[error("Missing query parameter for `platform`")]
struct MissingQueryParamPlatformError;

#[derive(Debug, thiserror::Error)]
#[error("Package with identifier `{0}` already exists.")]
struct PackageExistsError(String);

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

    let mut pahkat_package: pahkat_types::package::Descriptor = match ::toml::from_str(&pahkat_file) {
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

    static NONCE: Lazy<String> = Lazy::new(|| Uuid::new_v4().to_string() );
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

fn generate_repo_index(path: &path::Path) -> Result<Vec<u8>, std::io::Error> {
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
    Ok(index.to_vec())
}

fn modify_repo_metadata(
    path: &path::Path,
    package_id: &str,
    release: &UpdatePackageMetadataRequest,
) -> Result<(), PackageUpdateError> {
    let version: pahkat_types::package::Version = match release.version.parse() {
        Ok(v) => v,
        Err(e) => return Err(PackageUpdateError::VersionError(e)),
    };

    let inner_req = package::update::Request::builder()
        .repo_path(path.clone().into())
        .id(package_id.clone().into())
        .version(Cow::Owned(version))
        .channel(release.channel.as_ref().map(|x| Cow::Borrowed(&**x)))
        .target(Cow::Borrowed(&release.target))
        .url(None)
        .build();

    tracing::info!("Updating package...");
    match package::update::update(inner_req) {
        Ok(_) => {}
        Err(e) => return Err(PackageUpdateError::RepoError(e)),
    };

    Ok(())
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

#[OpenApi]
impl Api {
    /// Server status
    #[oai(path = "/status", method = "get")]
    async fn status(&self, repo_indexes: Data<&RepoIndexes>) -> Result<Json<ServerStatus>> {
        let index_ref = server_status(repo_indexes.0);
        Ok(Json(ServerStatus { index_ref }))
    }

    /// Create package metadata
    #[oai(path = "/:repo_id/packages/:package_id", method = "post")]
    async fn create_package_metadata(
        &self,
        _auth: BearerTokenAuth,
        git_repo_mutex: Data<&GitRepoMutex>,
        config: Data<&Config>,
        repo_id: Path<String>,
        package_id: Path<String>,
        data: Json<CreatePackageMetadataRequest>,
    ) -> Result<Json<CreatePackageMetadataResponse>> {
        if !config.repos.contains(&repo_id) {
            return Err(NotFoundError.into());
        }

        let mut guard = git_repo_mutex.write();

        if guard
            .path
            .join(&repo_id.0)
            .join("packages")
            .join(&package_id.0)
            .join("index.toml")
            .exists()
        {
            return Err(Conflict(PackageExistsError(package_id.0.clone())));
        }

        guard.cleanup(&config).map_err(|e| InternalServerError(e))?;

        package::init::init(
            package::init::Request::builder()
                .repo_path(guard.path.join(&repo_id.0).into())
                .id(Cow::Borrowed(&package_id.0))
                .name(Cow::Borrowed(&data.0.name))
                .description(Cow::Borrowed(&data.0.description))
                .tags(Cow::Borrowed(&data.0.tags))
                .build(),
        )
        .map_err(BadRequest)?;

        guard
            .add_package_to_index_tree(&repo_id.0, &package_id.0)
            .map_err(|e| InternalServerError(e))?;
        guard
            .commit_create(&repo_id.0, &package_id.0)
            .map_err(|e| InternalServerError(e))?;
        guard.push(&config).map_err(|e| InternalServerError(e))?;

        Ok(Json(CreatePackageMetadataResponse {
            repo_id: repo_id.0,
            package_id: package_id.0,
            success: true,
            error: None,
            timestamp: Utc::now(),
        }))
    }

    /// Update package metadata
    #[oai(path = "/:repo_id/packages/:package_id", method = "patch")]
    async fn update_package_metadata(
        &self,
        _auth: BearerTokenAuth,
        git_repo_mutex: Data<&GitRepoMutex>,
        config: Data<&Config>,
        repo_id: Path<String>,
        package_id: Path<String>,
        data: Json<UpdatePackageMetadataRequest>,
    ) -> Result<Json<UpdatePackageMetadataResponse>> {
        if !config.repos.contains(&repo_id) {
            return Err(NotFoundError.into());
        }

        let mut guard = git_repo_mutex.write();
        let repo_path = guard.path.join(&repo_id.0);

        if !repo_path
            .join("packages")
            .join(&package_id.0)
            .join("index.toml")
            .exists()
        {
            return Err(NotFoundError.into());
        }

        guard.cleanup(&config).map_err(|e| InternalServerError(e))?;
        modify_repo_metadata(&repo_path, &package_id.0, &data.0)
            .map_err(|e| InternalServerError(e))?;
        guard
            .add_package_to_index_tree(&repo_id.0, &package_id.0)
            .map_err(|e| InternalServerError(e))?;
        guard
            .commit_update(&repo_id.0, &package_id.0, &data.0)
            .map_err(|e| InternalServerError(e))?;
        guard.push(&config).map_err(|e| InternalServerError(e))?;

        Ok(Json(UpdatePackageMetadataResponse {
            repo_id: repo_id.0.to_string(),
            package_id: package_id.0.to_string(),
            success: true,
            error: None,
            timestamp: Utc::now(),
        }))
    }

    #[oai(
        path = "/1AAB4845-32A9-41A8-BBDE-120847548A82/:filename",
        method = "get"
    )]
    async fn workaround_download_pahkat(
        &self,
        git_repo_mutex: Data<&GitRepoMutex>,
        filename: Path<String>,
    ) -> Result<Response<Binary<String>>> {
        let platform = "windows";

        let guard = git_repo_mutex.read();

        let index = std::fs::read_to_string(
            guard
                .path
                .join("divvun-installer")
                .join("packages")
                .join("pahkat-service")
                .join("index.toml"),
        )
        .map_err(InternalServerError)?;
        let descriptor: Descriptor = ::toml::from_str(&index).map_err(InternalServerError)?;

        for release in descriptor.release {
            if release.channel.as_deref().unwrap_or("stable") != "stable" {
                continue;
            }

            let target = release.target.iter().find(|x| x.platform == platform);
            if let Some(target) = target {
                let url = target.payload.url();
                return Ok(Response::new(Binary("".into()))
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("Location", url.as_str()));
            }
        }

        Err(NotFoundError.into())
    }

    #[oai(
        path = "/1AAB4845-32A9-41A8-BBDE-120847548A81/:filename",
        method = "get"
    )]
    async fn workaround_download_divvun_manager(
        &self,
        git_repo_mutex: Data<&GitRepoMutex>,
        filename: Path<String>,
    ) -> Result<Response<Binary<String>>> {
        let platform = "windows";

        let guard = git_repo_mutex.read();

        let index = std::fs::read_to_string(
            guard
                .path
                .join("divvun-installer")
                .join("packages")
                .join("divvun-installer")
                .join("index.toml"),
        )
        .map_err(InternalServerError)?;
        let descriptor: Descriptor = ::toml::from_str(&index).map_err(InternalServerError)?;

        for release in descriptor.release {
            if release.channel.as_deref().unwrap_or("stable") != "stable" {
                continue;
            }

            let target = release.target.iter().find(|x| x.platform == platform);
            if let Some(target) = target {
                let url = target.payload.url();
                return Ok(Response::new(Binary("".into()))
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("Location", url.as_str()));
            }
        }

        Err(NotFoundError.into())
    }

    /// Download package
    #[oai(path = "/:repo_id/download/:package_id", method = "get")]
    async fn download(
        &self,
        git_repo_mutex: Data<&GitRepoMutex>,
        config: Data<&Config>,
        repo_id: Path<String>,
        package_id: Path<String>,
        params: poem::web::Query<PackageKeyParams>,
    ) -> Result<Response<Binary<String>>> {
        if !config.repos.contains(&repo_id) {
            return Err(NotFoundError.into());
        }

        let platform = match params.0.platform {
            Some(v) => v,
            None => {
                return Err(BadRequest(MissingQueryParamPlatformError));
            }
        };

        let guard = git_repo_mutex.read();

        let index = std::fs::read_to_string(
            guard
                .path
                .join(&repo_id.0)
                .join("packages")
                .join(&package_id.0)
                .join("index.toml"),
        )
        .map_err(InternalServerError)?;
        let descriptor: Descriptor = ::toml::from_str(&index).map_err(InternalServerError)?;

        for release in descriptor.release {
            if release.channel.as_deref().unwrap_or("stable")
                != params.0.channel.as_deref().unwrap_or("stable")
            {
                continue;
            }

            let target = release.target.iter().find(|x| x.platform == platform);
            if let Some(target) = target {
                let url = target.payload.url();
                return Ok(Response::new(Binary("".into()))
                    .status(StatusCode::TEMPORARY_REDIRECT)
                    .header("Location", url.as_str()));
            }
        }

        Err(NotFoundError.into())
    }

    /// Get package descriptor
    #[oai(path = "/:repo_id/packages/:package_id/index.toml", method = "get")]
    async fn package_descriptor(
        &self,
        config: Data<&Config>,
        repo_id: Path<String>,
        package_id: Path<String>,
    ) -> Result<Toml<String>> {
        let path = config
            .git_path
            .join(&repo_id.0)
            .join("packages")
            .join(package_id.0)
            .join("index.toml");

        let output = std::fs::read_to_string(path).map_err(|_| poem::Error::from(NotFoundError))?;

        Ok(Toml(output))
    }

    /// Get i18n strings
    ///
    /// {lang} must end in `.toml`.
    #[oai(path = "/:repo_id/strings/:lang", method = "get")]
    async fn strings(
        &self,
        config: Data<&Config>,
        repo_id: Path<String>,
        lang: Path<String>,
    ) -> Result<Toml<String>> {
        let lang = lang
            .0
            .strip_suffix(".toml")
            .ok_or_else(|| poem::Error::from(NotFoundError))?;

        let lang = if lang.is_empty() { "en" } else { lang };

        let lang_path = config
            .git_path
            .join(&repo_id.0)
            .join("strings")
            .join(lang)
            .with_extension("toml");

        tracing::debug!("Strings path: {:?}", &lang_path);

        let output =
            std::fs::read_to_string(lang_path).map_err(|_| poem::Error::from(NotFoundError))?;

        Ok(Toml(output))
    }

    /// Get repository toml index
    #[oai(path = "/:repo_id/index.toml", method = "get")]
    async fn repository_index_toml(
        &self,
        config: Data<&Config>,
        repo_id: Path<String>,
    ) -> Result<Toml<String>> {
        let index_path = config.git_path.join(&repo_id.0).join("index.toml");

        let output =
            std::fs::read_to_string(index_path).map_err(|_| poem::Error::from(NotFoundError))?;

        Ok(Toml(output))
    }

    /// Get repository binary index
    #[oai(path = "/:repo_id/packages/index.bin", method = "get")]
    async fn repository_index_bin(
        &self,
        config: Data<&Config>,
        repo_indexes: Data<&RepoIndexes>,
        repo_id: Path<String>,
        #[oai(name = "User-Agent")] user_agent: Header<Option<String>>,
    ) -> Result<Binary<Vec<u8>>> {
        let user_agent = user_agent.0.unwrap_or_else(|| "".to_string());

        if user_agent == "pahkat-client/0.1.0" {
            tracing::debug!("Detected old pahkat, serving workaround index");
            if repo_id.0 == "divvun-installer" {
                let index = generate_010_workaround_index(
                    &config.0,
                    &config.git_path.join("divvun-installer"),
                )
                .unwrap();
                return Ok(Binary(index));
            } else {
                return Ok(Binary(generate_empty_index().unwrap()));
            }
        }

        match repo_indexes.get(&repo_id.0) {
            Some(state) => {
                let state = state.load();
                Ok(Binary(state.1.clone()))
            }
            None => Err(NotFoundError.into()),
        }
    }
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

fn git_revparse_head(path: &path::Path) -> String {
    let output = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .current_dir(path)
        .output()
        .unwrap();
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .to_string()
}

struct GitRepo {
    path: PathBuf,
    head_ref: String,
}

impl GitRepo {
    fn new(path: PathBuf) -> Self {
        let path = dunce::canonicalize(&path).expect(&format!("Git path does not exist: '{}'", path.display()));
        let head_ref = git_revparse_head(&path);
        Self { path, head_ref }
    }

    fn add_package_to_index_tree(
        &mut self,
        repo_id: &str,
        package_id: &str,
    ) -> Result<(), std::io::Error> {
        Command::new("git")
            .arg("add")
            .arg(format!("{}/packages/{}", repo_id, package_id))
            .current_dir(&self.path)
            .status()?;

        Ok(())
    }

    fn commit_create(&mut self, repo_id: &str, package_id: &str) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["commit", "-m"])
            .arg(format!("[{}:create] `{}`", repo_id, package_id))
            .current_dir(&self.path)
            .status()?;

        self.head_ref = git_revparse_head(&self.path);

        Ok(())
    }

    fn commit_update(
        &mut self,
        repo_id: &str,
        package_id: &str,
        release: &UpdatePackageMetadataRequest,
    ) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["commit", "-m"])
            .arg(format!("[{}:update] `{} {}`", repo_id, package_id, release))
            .current_dir(&self.path)
            .status()?;

        self.head_ref = git_revparse_head(&self.path);

        Ok(())
    }

    fn push(&self, config: &Config) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["push", "origin", &format!("HEAD:{}", &config.branch_name)])
            .current_dir(&self.path)
            .status()?;
        Ok(())
    }

    fn cleanup(&self, config: &Config) -> Result<(), std::io::Error> {
        Command::new("git")
            .args(&["clean", "-dfx"])
            .current_dir(&self.path)
            .status()?;

        Command::new("git")
            .args(&["fetch", "origin", &config.branch_name])
            .current_dir(&self.path)
            .status()?;

        Command::new("git")
            .args(&["reset", "--hard", &format!("origin/{}", config.branch_name)])
            .current_dir(&self.path)
            .status()?;

        Ok(())
    }

    fn shallow_clone_to_tempdir(&self) -> Result<TempDir, std::io::Error> {
        let tmpdir = tempfile::tempdir()?;

        Command::new("git")
            .args(&["clone", "--depth", "1"])
            .arg(format!("file://{}", &self.path.display()))
            .arg(tmpdir.path())
            .output()?;

        Ok(tmpdir)
    }
}

type GitRepoMutex = Arc<RwLock<GitRepo>>;
type RepoIndexes = Arc<HashMap<String, ArcSwap<(String, Vec<u8>)>>>;

#[derive(Debug, Clone)]
struct ServerToken(String);

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
                    ArcSwap::new(Arc::new(("".to_string(), vec![]))),
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
        .finish();

    let api_service =
        OpenApiService::new(Api, "Pahkat Repository Server", env!("CARGO_PKG_VERSION"))
            .server(&config.url);
    let ui = api_service.rapidoc();
    let app = Route::new()
        .nest("/", api_service)
        .nest("/playground", ui)
        .at("/graphql", get(graphql_playground).post(GraphQL::new(schema)))
        .data(config.clone())
        .data(git_repo_mutex)
        .data(repo_indexes)
        .data(ServerToken(config.api_token.clone()));

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

