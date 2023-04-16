use super::storage::Storage;
use crate::lexicon::com::atproto::repo::{GetRecord, ListRecords};
use crate::lexicon::com::atproto::server::{CreateSession, RefreshSession};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize)]
pub struct Jwt {
    access: String,
    refresh: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Session {
    pub did: String,
    pub handle: String,
    pub jwt: Jwt,
}

impl From<CreateSession> for Session {
    fn from(create: CreateSession) -> Self {
        Self {
            did: create.did,
            handle: create.handle,
            jwt: Jwt {
                access: create.access_jwt,
                refresh: create.refresh_jwt,
            },
        }
    }
}

impl From<RefreshSession> for Session {
    fn from(refresh: RefreshSession) -> Self {
        Self {
            did: refresh.did,
            handle: refresh.handle,
            jwt: Jwt {
                access: refresh.access_jwt,
                refresh: refresh.refresh_jwt,
            },
        }
    }
}

pub struct Client<T: Storage<Session>> {
    service: reqwest::Url,
    storage: T,
    session: Session,
}

trait GetService {
    fn get_service(&self) -> &reqwest::Url;
    fn access_token(&self) -> &str;
}

impl<T: Storage<Session>> GetService for Client<T> {
    fn get_service(&self) -> &reqwest::Url {
        &self.service
    }

    fn access_token(&self) -> &str {
        &self.session.jwt.access
    }
}

#[derive(Debug, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

#[derive(Debug)]
pub enum LoginError<T: Storage<Session>> {
    Reqwest(reqwest::Error),
    Api(ApiError),
    AuthenticationRequired(String),
    Storage(T::Error),
}

impl<T: Storage<Session>> From<reqwest::Error> for LoginError<T> {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

#[derive(Debug)]
pub enum RefreshError<T: Storage<Session>> {
    Reqwest(reqwest::Error),
    Storage(T::Error),
    Api(ApiError),
    Blank,
}

impl<T: Storage<Session>> From<reqwest::Error> for RefreshError<T> {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

#[derive(Debug)]
pub enum GetError<T: Storage<Session>> {
    Reqwest(reqwest::Error),
    Refresh(RefreshError<T>),
    Api(ApiError),
}

impl<T: Storage<Session>> From<reqwest::Error> for GetError<T> {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

impl<T: Storage<Session>> From<RefreshError<T>> for GetError<T> {
    fn from(e: RefreshError<T>) -> Self {
        Self::Refresh(e)
    }
}

#[derive(Debug)]
pub enum PostError<T: Storage<Session>> {
    Reqwest(reqwest::Error),
    Refresh(RefreshError<T>),
    Json(serde_json::Error),
    Api(ApiError),
}

impl<T: Storage<Session>> From<reqwest::Error> for PostError<T> {
    fn from(e: reqwest::Error) -> Self {
        Self::Reqwest(e)
    }
}

impl<T: Storage<Session>> From<RefreshError<T>> for PostError<T> {
    fn from(e: RefreshError<T>) -> Self {
        Self::Refresh(e)
    }
}

impl<T: Storage<Session>> From<serde_json::Error> for PostError<T> {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl<T: Storage<Session>> Client<T> {
    pub async fn login(
        service: &reqwest::Url,
        identifier: &str,
        password: &str,
        storage: &mut T,
    ) -> Result<(), LoginError<T>> {
        let response = reqwest::Client::new()
            .post(
                service
                    .join("xrpc/com.atproto.server.createSession")
                    .unwrap(),
            )
            .body(
                json!({
                    "identifier": identifier,
                    "password": password,
                })
                .to_string(),
            )
            .send()
            .await?;

        match response.status() {
            reqwest::StatusCode::UNAUTHORIZED => {
                return Err(LoginError::Api(response.json::<ApiError>().await?));
            }
            reqwest::StatusCode::OK => {}
            _ => unreachable!(),
        };

        let body = response.json::<CreateSession>().await?.into();

        if let Err(e) = storage.set(&body).await {
            Err(LoginError::Storage(e))
        } else {
            Ok(())
        }
    }

    pub async fn new(service: reqwest::Url, mut storage: T) -> Result<Self, T::Error> {
        Ok(Self {
            service,
            session: storage.get().await?,
            storage,
        })
    }

    async fn xrpc_refresh_token(&mut self) -> Result<(), RefreshError<T>> {
        let response = reqwest::Client::new()
            .post(
                self.service
                    .join("xrpc/com.atproto.server.refreshSession")
                    .unwrap(),
            )
            .header(
                "authorization",
                format!("Bearer {}", self.session.jwt.refresh),
            )
            .send()
            .await?
            .error_for_status()?
            .json::<RefreshSession>()
            .await?;

        let session = response.into();

        if let Err(e) = self.storage.set(&session).await {
            Err(RefreshError::Storage(e))
        } else {
            self.session = session;
            Ok(())
        }
    }

    pub(crate) async fn xrpc_get<D: DeserializeOwned>(
        &mut self,
        path: &str,
        query: Option<&[(&str, &str)]>,
    ) -> Result<D, GetError<T>> {
        fn make_request<T: GetService>(
            self_: &T,
            path: &str,
            query: &Option<&[(&str, &str)]>,
        ) -> reqwest::RequestBuilder {
            let mut request = reqwest::Client::new()
                .get(self_.get_service().join(&format!("xrpc/{path}")).unwrap())
                .header("authorization", format!("Bearer {}", self_.access_token()));

            if let Some(query) = query {
                request = request.query(query);
            }

            request
        }

        let mut response = make_request(self, path, &query).send().await?;

        if let reqwest::StatusCode::BAD_REQUEST = response.status() {
            let error = response.json::<ApiError>().await?;
            if error.error == "ExpiredToken" {
                self.xrpc_refresh_token().await?;
                response = make_request(self, path, &query).send().await?;
            } else {
                return Err(GetError::Api(error));
            }
        }

        Ok(response.error_for_status()?.json().await?)
    }

    pub(crate) async fn xrpc_post<D1: Serialize, D2: DeserializeOwned>(
        &mut self,
        path: &str,
        body: &D1,
    ) -> Result<D2, PostError<T>> {
        let body = serde_json::to_string(body)?;

        fn make_request<T: GetService>(
            self_: &T,
            path: &str,
            body: &str,
        ) -> reqwest::RequestBuilder {
            reqwest::Client::new()
                .post(self_.get_service().join(&format!("xrpc/{path}")).unwrap())
                .header("authorization", format!("Bearer {}", self_.access_token()))
                .body(body.to_string())
        }

        let mut response = make_request(self, path, &body).send().await?;

        if let reqwest::StatusCode::BAD_REQUEST = response.status() {
            let error = response.json::<ApiError>().await?;
            if error.error == "ExpiredToken" {
                self.xrpc_refresh_token().await?;
                response = make_request(self, path, &body).send().await?;
            } else {
                return Err(PostError::Api(error));
            }
        }

        Ok(response.error_for_status()?.json::<D2>().await?)
    }
}

impl<T: Storage<Session>> Client<T> {
    pub async fn repo_get_record<D: DeserializeOwned>(
        &mut self,
        repo: &str,
        collection: &str,
        rkey: Option<&str>,
    ) -> Result<D, GetError<T>> {
        let mut query = vec![("repo", repo), ("collection", collection)];

        if let Some(rkey) = rkey {
            query.push(("rkey", rkey));
        }

        self.xrpc_get::<GetRecord<D>>("com.atproto.repo.getRecord", Some(&query))
            .await
            .map(|r| r.value)
    }

    pub async fn repo_list_records<D: DeserializeOwned>(
        &mut self,
        repo: &str,
        collection: &str,
        rkey: Option<&str>,
    ) -> Result<Vec<D>, GetError<T>> {
        let mut query = vec![("repo", repo), ("collection", collection)];

        if let Some(rkey) = rkey {
            query.push(("rkey", rkey));
        }

        self.xrpc_get::<ListRecords<D>>("com.atproto.repo.listRecords", Some(&query))
            .await
            .map(|l| l.records.into_iter().map(|r| r.value).collect())
    }
}
