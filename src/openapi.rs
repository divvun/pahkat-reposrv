use crate::{
    generate_010_workaround_index, generate_empty_index, git::GitRepoMutex, server_status,
    toml::Toml, Config, RepoIndexes, ServerStatus,
};
use chrono::{DateTime, Utc};
use pahkat_repomgr::package;
use pahkat_types::{package::Descriptor, package_key::PackageKeyParams};
use poem::{
    error::{BadRequest, Conflict, InternalServerError, NotFoundError},
    http::StatusCode,
    web::Data,
    Request, Result,
};
use poem_openapi::{
    auth::Bearer,
    param::{Header, Path},
    payload::{Binary, Json, Response},
    Object, OpenApi, SecurityScheme,
};
use std::{borrow::Cow, fmt::Display, path};

pub struct Api;

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

#[derive(Debug, Clone)]
pub struct ServerToken(pub(crate) String);

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
        #[allow(unused_variables)] filename: Path<String>,
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
        #[allow(unused_variables)] filename: Path<String>,
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
                let (_, ref state) = state.load().1;
                Ok(Binary(state.clone()))
            }
            None => Err(NotFoundError.into()),
        }
    }
}
