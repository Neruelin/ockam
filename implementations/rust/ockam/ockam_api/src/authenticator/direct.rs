pub mod types;

use core::str;
use lru::LruCache;
use minicbor::Decoder;
use ockam::identity::{AttributesEntry, IdentityAttributesWriter};
use ockam::identity::{IdentityIdentifier, IdentitySecureChannelLocalInfo};
use ockam::identity::{OneTimeCode, Timestamp};
use ockam_core::api::{self, Method, Request, Response, Status};
use ockam_core::compat::sync::{Arc, RwLock};
use ockam_core::errcode::{Kind, Origin};
use ockam_core::{self, CowStr, Result, Routed, Worker};
use ockam_identity::{secure_channel_required, LEGACY_ID, TRUST_CONTEXT_ID};
use ockam_node::{Context, RpcClient};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};
use tracing::{trace, warn};
use types::AddMember;

use crate::authenticator::direct::types::CreateToken;

const MAX_TOKEN_DURATION: Duration = Duration::from_secs(600);

/// Schema identifier for a project membership credential.
///
/// The credential will consist of the following attributes:
///
/// - `project_id` : bytes
/// - `role`: b"member"
pub const LEGACY_MEMBER: &str = "member";

// This acts as a facade, modifying and forwarding incoming messages from legacy clients
// to the new endpoints.   It's going to be removed once we don't need to maintain compatibility
// with old clients anymore.
pub struct LegacyApiConverter {}

impl LegacyApiConverter {
    pub fn new() -> Self {
        Self {}
    }
}

// Keep clippy happy
impl Default for LegacyApiConverter {
    fn default() -> Self {
        LegacyApiConverter::new()
    }
}

#[ockam_core::worker]
impl Worker for LegacyApiConverter {
    type Context = Context;
    type Message = Vec<u8>;

    async fn handle_message(
        &mut self,
        ctx: &mut Context,
        msg: Routed<Self::Message>,
    ) -> Result<()> {
        let body = msg.as_body().clone();
        let mut dec = Decoder::new(&body);
        let mut message = msg.into_local_message();
        let mut second_msg = message.clone(); // Borrow checker.  When authenticating using an enrollment token,
                                              // to adhere to the previous API this legacy worker actually issues
                                              // _two_ request on behalf of the user: one to enroll, other to get
                                              // the credential
        let transport_message = message.transport_mut();

        // Remove my address from the onward_route
        transport_message.onward_route.step()?;

        match dec.decode::<Request>() {
            Ok(req) => match (req.method(), req.path()) {
                (Some(Method::Post), "/tokens") => {
                    transport_message
                        .onward_route
                        .modify()
                        .append("enrollment_token_issuer");
                    ctx.forward(message).await
                }
                (Some(Method::Post), "/members") => {
                    transport_message
                        .onward_route
                        .modify()
                        .append("direct_authenticator");
                    ctx.forward(message).await
                }
                (Some(Method::Post), "/credential") if req.has_body() => {
                    transport_message
                        .onward_route
                        .modify()
                        .append("enrollment_token_acceptor");

                    // We don't want the 200 OK to be routed back to the client here,
                    // as legacy client is expecting the response to contain the credential.
                    transport_message
                        .return_route
                        .modify()
                        .prepend(ctx.address());
                    ctx.forward(message).await?;
                    // Give time for the enrollment to be done before asking for a credential.
                    // A better alternative is to wait for the response on handle_message,
                    // then decode it and issue the next message, then returning the credential
                    // to the client.   But it's too cumbersome for this that is a workaround
                    // to get previous clients to work, and we are removing this code soon after.
                    ockam_node::compat::tokio::time::sleep(Duration::from_millis(2000)).await;
                    // Request the credential,  note the return route points to the client
                    let body = Request::post("/credential").to_vec()?;
                    let transport_message = second_msg.transport_mut();
                    transport_message
                        .onward_route
                        .modify()
                        .append("credential_issuer");
                    transport_message.payload = body;
                    ctx.forward(second_msg).await?;
                    Ok(())
                }
                (Some(Method::Post), "/credential") => {
                    transport_message
                        .onward_route
                        .modify()
                        .append("credential_issuer");
                    ctx.forward(message).await
                }
                (_, _) => {
                    warn!("Legacy Authority Compatibility Worker received request at unknown path: {req:?}");
                    Ok(())
                }
            },
            Err(_) => {
                let mut dec = Decoder::new(&body);
                match dec.decode::<Response>() {
                    Ok(resp) => {
                        if resp.status() == Some(Status::Ok) {
                            debug!("Received resp: {resp:?}");
                        } else {
                            warn!("Received a non-ok response {resp:?}");
                        }
                    }
                    _ => warn!("Received and discarded a non request/response message {message:?}"),
                }
                Ok(())
            }
        }
    }
}

pub struct DirectAuthenticator {
    trust_context: String,
    attributes_writer: Arc<dyn IdentityAttributesWriter>,
}

impl DirectAuthenticator {
    pub async fn new(
        trust_context: String,
        attributes_writer: Arc<dyn IdentityAttributesWriter>,
    ) -> Result<Self> {
        Ok(Self {
            trust_context,
            attributes_writer,
        })
    }

    async fn add_member<'a>(
        &self,
        enroller: &IdentityIdentifier,
        id: &IdentityIdentifier,
        attrs: &HashMap<CowStr<'a>, CowStr<'a>>,
    ) -> Result<()> {
        let auth_attrs = attrs
            .iter()
            .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
            .chain(
                [
                    (LEGACY_ID.to_owned(), self.trust_context.as_bytes().to_vec()),
                    (
                        TRUST_CONTEXT_ID.to_owned(),
                        self.trust_context.as_bytes().to_vec(),
                    ),
                ]
                .into_iter(),
            )
            .collect();
        let entry = AttributesEntry::new(
            auth_attrs,
            Timestamp::now().unwrap(),
            None,
            Some(enroller.clone()),
        );
        self.attributes_writer.put_attributes(id, entry).await
    }
}

#[ockam_core::worker]
impl Worker for DirectAuthenticator {
    type Context = Context;
    type Message = Vec<u8>;

    async fn handle_message(&mut self, c: &mut Context, m: Routed<Self::Message>) -> Result<()> {
        if let Ok(i) = IdentitySecureChannelLocalInfo::find_info(m.local_message()) {
            let from = i.their_identity_id();
            let mut dec = Decoder::new(m.as_body());
            let req: Request = dec.decode()?;
            trace! {
                target: "ockam_api::authenticator::direct::direct_authenticator",
                from   = %from,
                id     = %req.id(),
                method = ?req.method(),
                path   = %req.path(),
                body   = %req.has_body(),
                "request"
            }
            let res = match (req.method(), req.path()) {
                (Some(Method::Post), "/") | (Some(Method::Post), "/members") => {
                    let add: AddMember = dec.decode()?;
                    self.add_member(&from, add.member(), add.attributes())
                        .await?;
                    Response::ok(req.id()).to_vec()?
                }
                _ => api::unknown_path(&req).to_vec()?,
            };
            c.send(m.return_route(), res).await
        } else {
            secure_channel_required(c, m).await
        }
    }
}

#[derive(Clone)]
pub struct EnrollmentTokenAuthenticator {
    trust_context: String,
    tokens: Arc<RwLock<LruCache<[u8; 32], Token>>>,
}

pub struct EnrollmentTokenIssuer(EnrollmentTokenAuthenticator);

pub struct EnrollmentTokenAcceptor(
    EnrollmentTokenAuthenticator,
    Arc<dyn IdentityAttributesWriter>,
);

impl EnrollmentTokenAuthenticator {
    pub fn new_worker_pair(
        trust_context: String,
        attributes_writer: Arc<dyn IdentityAttributesWriter>,
    ) -> (EnrollmentTokenIssuer, EnrollmentTokenAcceptor) {
        let base = Self {
            trust_context,
            tokens: Arc::new(RwLock::new(LruCache::new(
                NonZeroUsize::new(128).expect("0 < 128"),
            ))),
        };
        (
            EnrollmentTokenIssuer(base.clone()),
            EnrollmentTokenAcceptor(base, attributes_writer),
        )
    }
}

impl EnrollmentTokenIssuer {
    async fn issue_token(
        &self,
        enroller: &IdentityIdentifier,
        attrs: HashMap<String, String>,
    ) -> Result<OneTimeCode> {
        let otc = OneTimeCode::new();
        let tkn = Token {
            attrs,
            generated_by: enroller.clone(),
            time: Instant::now(),
        };
        self.0
            .tokens
            .write()
            .map(|mut r| {
                r.put(*otc.code(), tkn);
                otc
            })
            .map_err(|_| {
                ockam_core::Error::new(
                    Origin::Other,
                    Kind::Internal,
                    "failed to get read lock on tokens table",
                )
            })
    }
}

#[ockam_core::worker]
impl Worker for EnrollmentTokenIssuer {
    type Context = Context;
    type Message = Vec<u8>;

    async fn handle_message(&mut self, c: &mut Context, m: Routed<Self::Message>) -> Result<()> {
        if let Ok(i) = IdentitySecureChannelLocalInfo::find_info(m.local_message()) {
            let from = i.their_identity_id();
            let mut dec = Decoder::new(m.as_body());
            let req: Request = dec.decode()?;
            trace! {
                target: "ockam_api::authenticator::direct::enrollment_token_issuer",
                from   = %from,
                id     = %req.id(),
                method = ?req.method(),
                path   = %req.path(),
                body   = %req.has_body(),
                "request"
            }
            let res = match (req.method(), req.path()) {
                (Some(Method::Post), "/") | (Some(Method::Post), "/tokens") => {
                    let att: CreateToken = dec.decode()?;
                    match self.issue_token(&from, att.into_owned_attributes()).await {
                        Ok(otc) => Response::ok(req.id()).body(&otc).to_vec()?,
                        Err(error) => api::internal_error(&req, &error.to_string()).to_vec()?,
                    }
                }
                _ => api::unknown_path(&req).to_vec()?,
            };
            c.send(m.return_route(), res).await
        } else {
            secure_channel_required(c, m).await
        }
    }
}

#[ockam_core::worker]
impl Worker for EnrollmentTokenAcceptor {
    type Context = Context;
    type Message = Vec<u8>;

    async fn handle_message(&mut self, c: &mut Context, m: Routed<Self::Message>) -> Result<()> {
        if let Ok(i) = IdentitySecureChannelLocalInfo::find_info(m.local_message()) {
            let from = i.their_identity_id();
            let mut dec = Decoder::new(m.as_body());
            let req: Request = dec.decode()?;
            trace! {
                target: "ockam_api::authenticator::direct::enrollment_token_acceptor",
                from   = %from,
                id     = %req.id(),
                method = ?req.method(),
                path   = %req.path(),
                body   = %req.has_body(),
                "request"
            }
            let res = match (req.method(), req.path()) {
                (Some(Method::Post), "/") | (Some(Method::Post), "/credential") => {
                    //TODO: move out of the worker handle_message implementation
                    let otc: OneTimeCode = dec.decode()?;
                    let token = match self.0.tokens.write() {
                        Ok(mut r) => {
                            if let Some(tkn) = r.pop(otc.code()) {
                                if tkn.time.elapsed() > MAX_TOKEN_DURATION {
                                    Err(api::forbidden(&req, "expired token"))
                                } else {
                                    Ok(tkn)
                                }
                            } else {
                                Err(api::forbidden(&req, "unknown token"))
                            }
                        }
                        Err(_) => Err(api::internal_error(
                            &req,
                            "Failed to get read lock on tokens table",
                        )),
                    };
                    match token {
                        Ok(tkn) => {
                            //TODO: fixme:  unify use of hashmap vs btreemap
                            let trust_context = self.0.trust_context.as_bytes().to_vec();
                            let attrs = tkn
                                .attrs
                                .iter()
                                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                                .chain(
                                    [
                                        (LEGACY_ID.to_owned(), trust_context.clone()),
                                        (TRUST_CONTEXT_ID.to_owned(), trust_context),
                                    ]
                                    .into_iter(),
                                )
                                .collect();
                            let entry = AttributesEntry::new(
                                attrs,
                                Timestamp::now().unwrap(),
                                None,
                                Some(tkn.generated_by),
                            );
                            self.1.put_attributes(&from, entry).await?;
                            Response::ok(req.id()).to_vec()?
                        }
                        Err(err) => err.to_vec()?,
                    }
                }
                _ => api::unknown_path(&req).to_vec()?,
            };
            c.send(m.return_route(), res).await
        } else {
            secure_channel_required(c, m).await
        }
    }
}

struct Token {
    attrs: HashMap<String, String>,
    generated_by: IdentityIdentifier,
    time: Instant,
}

pub struct DirectAuthenticatorClient(RpcClient);

impl DirectAuthenticatorClient {
    pub fn new(client: RpcClient) -> Self {
        DirectAuthenticatorClient(client)
    }

    pub async fn add_member(
        &self,
        id: IdentityIdentifier,
        attributes: HashMap<&str, &str>,
    ) -> Result<()> {
        self.0
            .request_no_resp_body(
                &Request::post("/").body(AddMember::new(id).with_attributes(attributes)),
            )
            .await
    }
}

pub struct TokenIssuerClient(RpcClient);

impl TokenIssuerClient {
    pub fn new(client: RpcClient) -> Self {
        TokenIssuerClient(client)
    }

    pub async fn create_token(&self, attributes: HashMap<&str, &str>) -> Result<OneTimeCode> {
        self.0
            .request(&Request::post("/").body(CreateToken::new().with_attributes(attributes)))
            .await
    }
}

pub struct TokenAcceptorClient(RpcClient);

impl TokenAcceptorClient {
    pub fn new(client: RpcClient) -> Self {
        TokenAcceptorClient(client)
    }

    pub async fn present_token(&self, c: &OneTimeCode) -> Result<()> {
        self.0
            .request_no_resp_body(&Request::post("/").body(c))
            .await
    }
}
