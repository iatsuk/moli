//! Shared native interception transport for XHR and stylesheet resources.

mod context;
mod result;
mod service_worker;
mod stream;
pub(crate) use context::{ControlledXhrContext, XhrInterceptionRoute};
use result::BufferedXhrResponse;
pub(crate) use result::{XhrLoadResult, start_buffered_xhr};
pub(crate) use service_worker::XhrServiceWorkerContext;
pub(crate) use stream::{ControlledXhrCompletion, ControlledXhrStream};

use moli_fetch::{FetchCancelHandle, Request, Response, ResponseHead};
use tokio::sync::oneshot;

use crate::{
    document_runtime::DocumentConnectPolicySnapshot,
    network::{
        ResourceRequestClient,
        interception_control::{
            InterceptionPhase, ResourceInterceptionDecision, ResourceInterceptionPause,
        },
        interception_output::ResourceInterceptionContext,
        loads::ResourceLoadLease,
    },
    runtime::RendererOwnerAction,
    types::{
        PendingSubresourceAuthInfo, PendingSubresourceContinueEvent, PendingSubresourceFetchInfo,
        PendingSubresourceResponseInfo, SubresourceRequestInitiatorType, SubresourceResponseBody,
    },
};

pub(crate) struct ControlledResourceLoad {
    request: Request,
    info: PendingSubresourceFetchInfo,
    load: ResourceLoadLease,
    cancel: FetchCancelHandle,
    interception: Option<ResourceInterceptionContext>,
    service_worker: Option<XhrServiceWorkerContext>,
    connect_policy: Option<DocumentConnectPolicySnapshot>,
    preflight_headers: Vec<(String, String)>,
    initial_decision: Option<oneshot::Receiver<ResourceInterceptionDecision>>,
    request_initiator_type: SubresourceRequestInitiatorType,
}

use super::loaded_response::LoadedResponse;

impl LoadedResponse {
    async fn deliver(
        &self,
        info: &PendingSubresourceFetchInfo,
        stream: Option<&mut ControlledXhrStream>,
        network_request_headers: Option<Vec<(String, String)>>,
    ) -> Result<Option<BufferedXhrResponse>, String> {
        if let Some(stream) = stream
            && self.body_is_null != Some(true)
        {
            stream.start(
                info,
                self.head.clone(),
                network_request_headers,
                self.body.clone(),
                self.response_filter.clone(),
                self.source == moli_fetch::RedirectSource::ServiceWorker,
            )?;
            let mut offset = 0;
            loop {
                let (bytes, eof) = self
                    .body
                    .read_chunk(offset, 64 * 1024)
                    .await
                    .map_err(|error| format!("{error:#}"))?;
                if !bytes.is_empty() {
                    offset += bytes.len();
                    stream.chunk(bytes)?;
                }
                if eof {
                    break;
                }
            }
            // The stream consumer already owns delivery; no buffered Response is needed.
            return Ok(None);
        }
        let (response, body) = self.materialize().await?;
        Ok(Some(BufferedXhrResponse {
            response_filter: self.response_filter.clone(),
            body_is_null: self.body_is_null,
            skip_fetch_security_validation: self.source
                == moli_fetch::RedirectSource::ServiceWorker,
            response,
            body,
        }))
    }
}

impl ControlledResourceLoad {
    pub(crate) fn for_stylesheet(
        interception: crate::network::interception_output::ResourceInterceptionContext,
        request: Request,
        info: PendingSubresourceFetchInfo,
        load: ResourceLoadLease,
        cancel: FetchCancelHandle,
        request_initiator_type: SubresourceRequestInitiatorType,
    ) -> Self {
        Self {
            request,
            info,
            load,
            cancel,
            interception: Some(interception),
            service_worker: None,
            connect_policy: None,
            preflight_headers: Vec::new(),
            initial_decision: None,
            request_initiator_type,
        }
    }

    pub(crate) fn with_service_worker(
        mut self,
        service_worker: Option<XhrServiceWorkerContext>,
    ) -> Self {
        self.service_worker = service_worker;
        self
    }

    fn native_interception(&self) -> &ResourceInterceptionContext {
        self.interception
            .as_ref()
            .expect("native XHR pauses have an interception owner")
    }

    /// The Page can publish request admission in the initiating turn. Network
    /// execution then awaits the same native port, preserving async XHR's
    /// existing request-notification boundary without running a Page task.
    pub(crate) fn prepare_initial_pause(&mut self) -> RendererOwnerAction {
        let (pause, decision) = self.pause(InterceptionPhase::Request);
        self.initial_decision = Some(decision);
        RendererOwnerAction::SubresourceFetchPause {
            redirect: None,
            source_document: self.native_interception().source_document,
            info: Box::new(self.info.clone()),
            child_document: None,
            network_pause: Some(pause),
        }
    }

    pub(crate) async fn run(self) -> XhrLoadResult {
        match self.run_with_stream(None).await {
            ControlledXhrCompletion::Buffered(completed) => *completed,
            ControlledXhrCompletion::Streamed { .. } => {
                unreachable!("buffered XHR has no streaming sink")
            }
        }
    }

    pub(crate) async fn run_with_stream(
        mut self,
        mut stream: Option<ControlledXhrStream>,
    ) -> ControlledXhrCompletion {
        let service_worker = self
            .service_worker
            .as_ref()
            .map(|worker| worker.runtime.clone());
        let cancel = self.cancel.clone();
        let result = match service_worker {
            Some(runtime) => {
                runtime
                    .await_synchronous_fetch(self.run_inner(stream.as_mut()), cancel)
                    .await
            }
            None => self.run_inner(stream.as_mut()).await,
        };
        if let Some(body_source_id) = stream.as_ref().and_then(|stream| stream.body_source_id) {
            self.publish_completed();
            return ControlledXhrCompletion::Streamed {
                internal_id: self.info.internal_id,
                body_source_id,
                result: result.map(|_| ()),
            };
        }
        let (result, network_headers) = match result {
            Ok((Some(response), headers)) => (Ok(response), headers),
            Ok((None, _)) => {
                unreachable!("streamed body requires its registered sink")
            }
            Err(error) => (Err(error), None),
        };
        let mut completed = XhrLoadResult::new(self.info.clone(), result, network_headers);
        completed.network_record = completed
            .network_record
            .with_request_initiator_type(self.request_initiator_type);
        if completed.info.network_request_handle.is_some() {
            // Redirects already crossed their own native request-pause boundary.
            // Diagnostics retain the full chain; protocol sees only the terminal.
            self.native_interception()
                .output
                .record_subresource_network(
                    self.native_interception().source_document,
                    completed
                        .network_record
                        .clone()
                        .with_redirect_chain(Vec::new()),
                );
        }
        self.publish_completed();
        ControlledXhrCompletion::Buffered(Box::new(completed))
    }

    fn publish_completed(&self) {
        if self.interception.is_none() {
            return;
        }
        let _ =
            self.native_interception()
                .output
                .publish(RendererOwnerAction::SubresourceContinue {
                    response_body: None,
                    source_document: self.native_interception().source_document,
                    event: Box::new(PendingSubresourceContinueEvent::Completed {
                        internal_id: self.info.internal_id,
                    }),
                    network_pause: None,
                });
    }

    fn pause(
        &self,
        phase: InterceptionPhase,
    ) -> (
        ResourceInterceptionPause,
        oneshot::Receiver<ResourceInterceptionDecision>,
    ) {
        let (pause, rx) = ResourceInterceptionPause::new(
            self.info.internal_id,
            phase,
            self.load.request_client().page_network_policy().clone(),
        );
        let redirect = (phase == InterceptionPhase::Request)
            .then(|| self.request.redirect_chain().last().cloned())
            .flatten();
        (
            pause
                .with_request_redirect(redirect)
                .with_request_initiator_type(self.request_initiator_type),
            rx,
        )
    }

    async fn decision(
        &self,
        rx: oneshot::Receiver<ResourceInterceptionDecision>,
    ) -> Result<ResourceInterceptionDecision, String> {
        tokio::select! {
            biased;
            () = self.cancel.cancelled() => Err("net::ERR_ABORTED".into()),
            result = rx => result.map_err(|_| "network interception owner was dropped".into()),
        }
    }

    fn sync_info(&mut self) {
        self.info.url = self.request.url.clone();
        self.info.method = self.request.method.clone();
        self.info.request_headers = self.request.interception_request_headers();
        self.info.request_body_bytes = self.request.body.clone();
        self.info.request_body = self
            .request
            .body
            .as_ref()
            .map(|body| String::from_utf8_lossy(body).into_owned());
    }

    fn apply_request_overrides(&mut self, overrides: moli_fetch::RequestOverrides) {
        let headers_changed = overrides.headers.is_some();
        overrides.apply(&mut self.request);
        if headers_changed {
            self.preflight_headers = self.request.request_headers.to_byte_strings();
        }
        self.sync_info();
    }

    fn check_request(&self) -> Result<(), String> {
        let redirect_status = if self.request.redirect_count() == 0 {
            crate::content_security_policy::ContentSecurityPolicyRedirectStatus::NoRedirect
        } else {
            crate::content_security_policy::ContentSecurityPolicyRedirectStatus::FollowedRedirect
        };
        if let Some(policy) = &self.connect_policy
            && let Some(violation) = policy.enforce_violation(
                &self.info.document_url,
                &self.request.url,
                redirect_status,
            )
        {
            return Err(
                crate::document_runtime::document_content_security_policy_error_message(
                    &violation,
                    "XMLHttpRequest",
                ),
            );
        }
        if self.connect_policy.is_none() {
            self.request
                .admit_request_url(&self.request.url)
                .map_err(|error| error.to_string())?;
        }
        moli_url_policy::ensure_http_network_transport_url(&self.request.url)
            .map_err(|error| error.to_string())
    }

    fn synthetic_response(
        &self,
        status: u16,
        headers: Vec<(String, Vec<u8>)>,
        body: crate::RendererSyntheticResponseBody,
    ) -> Response {
        body.into_fetch_response(ResponseHead {
            request_include_credentials: self.request.allows_credentials_for_url(&self.request.url),

            final_url: self.request.url.clone(),
            status,
            status_text: None,
            headers,
            request_cookie_report: None,
            cookie_set_reports: Vec::new(),
            redirected: self.request.redirect_count() != 0,
            redirect_chain: self.request.redirect_chain().to_vec(),
            from_cache: false,
            cache_state: Default::default(),
            preload_state: Default::default(),
            negotiated_http_version: None,
        })
    }

    async fn run_inner(
        &mut self,
        mut stream: Option<&mut ControlledXhrStream>,
    ) -> Result<(Option<BufferedXhrResponse>, Option<Vec<(String, String)>>), String> {
        use ResourceInterceptionDecision as D;
        let mut next_request_pause = self.interception.is_some();
        let mut select_service_worker = true;
        let mut intercept_response = false;
        let mut handle_auth_requests = false;
        let mut synthetic = None;
        let mut network_request_headers = None;
        let preflight_observer = self.interception.as_ref().map(|context| {
            super::preflight_events::CorsPreflightNetworkObserver::for_native_xhr(
                context.output.clone(),
                context.source_document,
                &self.info,
            )
        });
        loop {
            if self.cancel.is_cancelled() || self.load.is_cancelled() {
                return Err("net::ERR_ABORTED".into());
            }
            if next_request_pause {
                // ExtraInfo describes the first on-wire request of this hop.
                // Authentication retries retain it; a redirect starts a new hop.
                network_request_headers = None;
                self.sync_info();
                let rx = if let Some(rx) = self.initial_decision.take() {
                    rx
                } else {
                    let (pause, rx) = self.pause(InterceptionPhase::Request);
                    if !self.native_interception().output.publish(
                        RendererOwnerAction::SubresourceFetchPause {
                            redirect: None,
                            source_document: self.native_interception().source_document,
                            info: Box::new(self.info.clone()),
                            child_document: None,
                            network_pause: Some(pause),
                        },
                    ) {
                        return Err("network interception output owner is closed".into());
                    }
                    rx
                };
                match self.decision(rx).await? {
                    D::ContinueRequest {
                        url,
                        method,
                        body,
                        headers,
                        intercept_response: response,
                        handle_auth_requests: auth,
                    } => {
                        self.apply_request_overrides(moli_fetch::RequestOverrides {
                            url,
                            method,
                            body,
                            headers,
                        });
                        intercept_response = response;
                        handle_auth_requests = auth;
                    }
                    D::Fail(error) => return Err(error),
                    D::Fulfill {
                        request_overrides,
                        status,
                        headers,
                        body,
                    } => {
                        if let Some(overrides) = request_overrides {
                            self.apply_request_overrides(*overrides);
                        }
                        synthetic = Some(self.synthetic_response(status, headers, body));
                        intercept_response = false;
                        handle_auth_requests = false;
                    }
                    _ => unreachable!("request pause accepts only request decisions"),
                }
            }
            self.check_request()?;
            let loader: ResourceRequestClient = self.load.request_client();
            let hop_cancel = self.cancel.child_for_subrequest();
            let worker_response = if synthetic.is_none() && select_service_worker {
                match self.service_worker.as_ref() {
                    Some(worker) => {
                        worker
                            .load_hop(
                                &self.load,
                                &mut self.request,
                                &self.info,
                                &hop_cancel,
                                &mut self.preflight_headers,
                            )
                            .await?
                    }
                    None => None,
                }
            } else {
                None
            };
            // SW network fallback returns the retained, policy-configured native request.
            self.check_request()?;
            self.sync_info();
            let (response, mut is_synthetic, observed_request_headers) =
                match (synthetic.take(), worker_response) {
                    (Some(response), _) => (LoadedResponse::from_response(response), true, None),
                    (None, Some(response)) => (response, false, None),
                    (None, None) => {
                        let observed = super::async_fetch::fetch_subresource_hop(
                            &loader,
                            self.request.clone(),
                            hop_cancel.clone(),
                            self.preflight_headers.clone(),
                            preflight_observer.as_ref(),
                        )
                        .await?;
                        let (response, observation) = observed.into_parts();
                        let headers = observation.map(|observation| observation.into_headers());
                        (response, false, headers)
                    }
                };
            if network_request_headers.is_none() {
                network_request_headers = observed_request_headers;
            }
            let mut response = response;
            if handle_auth_requests
                && response.source == moli_fetch::RedirectSource::Network
                && matches!(response.head.status, 401 | 407)
                && let Some(challenge) = crate::protocol_types::extract_subresource_auth_challenge(
                    &response.head.headers,
                )
            {
                let (pause, rx) = self.pause(InterceptionPhase::Auth);
                let pause = pause.with_response_body(response.body.clone());
                if !self.native_interception().output.publish(
                    RendererOwnerAction::SubresourceContinue {
                        response_body: None,
                        source_document: self.native_interception().source_document,
                        event: Box::new(PendingSubresourceContinueEvent::AuthRequired(
                            PendingSubresourceAuthInfo {
                                response_preload_state: response.head.preload_state.clone(),

                                request_include_credentials: response
                                    .head
                                    .request_include_credentials,

                                internal_id: self.info.internal_id,
                                url: self.request.url.clone(),
                                method: self.request.method.clone(),
                                request_headers: self.request.request_headers.clone(),
                                request_body: self.info.request_body.clone(),
                                resource_type: self.info.resource_type,
                                request_cookie_report: response.head.request_cookie_report.clone(),
                                network_request_headers: network_request_headers.clone(),
                                challenge,
                                intercept_response,
                                response_final_url: response.head.final_url.clone(),
                                response_status: response.head.status,
                                response_headers: response.head.headers.clone(),
                                response_body: SubresourceResponseBody::from_bytes(Vec::new()),
                                response_from_cache: response.head.from_cache,
                                response_cache_state: response.head.cache_state,
                            },
                        )),
                        network_pause: Some(pause),
                    },
                ) {
                    return Err("network authentication output owner is closed".into());
                }
                match self.decision(rx).await? {
                    D::Auth(auth) => {
                        if !self.load.try_restart_with_auth() {
                            return Err(moli_fetch::AuthRetryBudget::ERROR_TEXT.into());
                        }
                        self.request = self.request.clone().with_auth(auth.into());
                        next_request_pause = false;
                        select_service_worker = false;
                        continue;
                    }
                    D::CancelAuth => {}
                    D::Fail(error) => return Err(error),
                    _ => unreachable!("auth pause accepts only auth decisions"),
                }
            }
            if intercept_response {
                let (pause, rx) = self.pause(InterceptionPhase::Response);
                let pause = pause.with_response_body(response.body.clone());
                if !self.native_interception().output.publish(
                    RendererOwnerAction::SubresourceContinue {
                        response_body: None,
                        source_document: self.native_interception().source_document,
                        event: Box::new(PendingSubresourceContinueEvent::ResponsePaused(
                            PendingSubresourceResponseInfo {
                                preload_state: response.head.preload_state.clone(),

                                request_include_credentials: response
                                    .head
                                    .request_include_credentials,

                                internal_id: self.info.internal_id,
                                url: self.request.url.clone(),
                                final_url: response.head.final_url.clone(),
                                method: self.request.method.clone(),
                                request_headers: self.request.request_headers.clone(),
                                request_body: self.info.request_body.clone(),
                                resource_type: self.info.resource_type,
                                request_cookie_report: response.head.request_cookie_report.clone(),
                                network_request_headers: network_request_headers.clone(),
                                response_status: response.head.status,
                                response_headers: response.head.headers.clone(),
                                response_body: SubresourceResponseBody::from_bytes(Vec::new()),
                                from_cache: response.head.from_cache,
                                cache_state: response.head.cache_state,
                            },
                        )),
                        network_pause: Some(pause),
                    },
                ) {
                    return Err("network response output owner is closed".into());
                }
                match self.decision(rx).await? {
                    D::ContinueResponse { status, headers } => {
                        if let Some(status) = status {
                            response.head.status = status;
                            response.head.status_text = None;
                        }
                        if let Some(headers) = headers {
                            response.head.headers = headers;
                        }
                    }
                    D::Fulfill {
                        request_overrides: _,
                        status,
                        headers,
                        body,
                    } => {
                        response = LoadedResponse::from_response(
                            self.synthetic_response(status, headers, body),
                        );
                        is_synthetic = true;
                    }
                    D::Fail(error) => return Err(error),
                    _ => unreachable!("response pause accepts only response decisions"),
                }
            }
            if let Some(worker) = &self.service_worker {
                worker.check_response(&response.head, response.source)?;
            }
            use super::response_hop::{
                SubresourceResponseTransition, advance_subresource_response,
            };
            let transition = advance_subresource_response(
                &mut self.request,
                response.head.clone(),
                response.request_extra_info.is_some(),
                response.request_extra_info.as_ref(),
                response.source,
            )?;
            if let SubresourceResponseTransition::ManualResponse { .. } = transition {
                return Err("XHR requests must follow redirects".into());
            }
            if transition == SubresourceResponseTransition::FinalResponse {
                response.head.redirect_chain = self.request.redirect_chain().to_vec();
                response.head.redirected = !response.head.redirect_chain.is_empty();
                // Client-provided responses have the same CORS obligations as
                // transport responses. Validate before any success is published.
                if response.source == moli_fetch::RedirectSource::Network
                    && self.request.request_mode == moli_fetch::RequestMode::Cors
                {
                    super::validate_cors_response_chain(
                        self.request
                            .request_origin()
                            .expect("XHR has a requesting origin"),
                        &response.head,
                        self.request.credentials_mode,
                    )?;
                }
                if !is_synthetic && response.source == moli_fetch::RedirectSource::Network {
                    self.cancel.adopt_response_progress(&hop_cancel);
                }
                let delivered = response
                    .deliver(
                        &self.info,
                        if is_synthetic {
                            None
                        } else {
                            stream.as_deref_mut()
                        },
                        network_request_headers.clone(),
                    )
                    .await?;
                return Ok((delivered, network_request_headers));
            }
            self.preflight_headers.retain(|(name, _)| {
                self.request
                    .request_headers
                    .iter()
                    .any(|(remaining, _)| name.eq_ignore_ascii_case(remaining))
            });
            // Retire the previous hop's protocol correlation before installing
            // its next one. The stable native network handle keeps Network ID.
            if self.interception.is_some() {
                self.native_interception().output.publish(
                    RendererOwnerAction::SubresourceContinue {
                        response_body: None,
                        source_document: self.native_interception().source_document,
                        event: Box::new(PendingSubresourceContinueEvent::Completed {
                            internal_id: self.info.internal_id,
                        }),
                        network_pause: None,
                    },
                );
            }
            next_request_pause = self.interception.is_some();
            select_service_worker = true;
        }
    }
}
