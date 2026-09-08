//! Shared response-header transition for native Fetch and XHR owners.
use super::next_redirect_url;
use moli_fetch::{RedirectInfo, Request, RequestMode, RequestRedirectMode, ResponseHead};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SubresourceResponseTransition {
    FinalResponse,
    ManualResponse { response_url: url::Url },
    FollowedRedirect,
}

pub(crate) fn advance_subresource_response(
    request: &mut Request,
    head: ResponseHead,
    network_extra_info_available: bool,
    request_extra_info: Option<&moli_fetch::NetworkRequestExtraInfo>,
    source: moli_fetch::RedirectSource,
) -> Result<SubresourceResponseTransition, String> {
    // The single-hop transport refines request snapshots, including cache
    // hits and internal redirects. Keep them when carrying the chain on.
    if source == moli_fetch::RedirectSource::Network {
        request.observe_redirect_requests(&head.redirect_chain);
    }
    // Keep redirect-mode error precedence aligned with Fetch: an error-mode
    // redirect is rejected before the redirect response is CORS-checked.
    if request.redirect_mode() == RequestRedirectMode::Error {
        if next_redirect_url(&head.final_url, head.status, &head.headers, 0)?.is_some() {
            return Err(redirect_mode_error_message(&head.final_url));
        }
        if source == moli_fetch::RedirectSource::Network {
            request.validate_cors_response_for_url(&head.final_url, &head.headers)?;
        }
        return Ok(SubresourceResponseTransition::FinalResponse);
    }

    if source == moli_fetch::RedirectSource::Network {
        request.validate_cors_response_for_url(&head.final_url, &head.headers)?;
    }
    // Manual responses are filtered without following Location. Parsing it or
    // applying the redirect limit would reject an otherwise valid response.
    if request.redirect_mode() == RequestRedirectMode::Manual {
        return Ok(if matches!(head.status, 301 | 302 | 303 | 307 | 308) {
            SubresourceResponseTransition::ManualResponse {
                response_url: head.final_url,
            }
        } else {
            SubresourceResponseTransition::FinalResponse
        });
    }
    let Some(next_url) = next_redirect_url(
        &head.final_url,
        head.status,
        &head.headers,
        request.redirect_count(),
    )?
    else {
        return Ok(SubresourceResponseTransition::FinalResponse);
    };

    match request.redirect_mode() {
        RequestRedirectMode::Follow => {}
        RequestRedirectMode::Error | RequestRedirectMode::Manual => {
            unreachable!("redirect modes were handled before the follow transition")
        }
    }
    if !matches!(next_url.scheme(), "http" | "https") {
        return Err(format!("CORS redirect requires an HTTP(S) URL: {next_url}"));
    }
    if request.request_mode == RequestMode::Cors
        && (!next_url.username().is_empty() || next_url.password().is_some())
        && request.request_origin().is_some_and(|origin| {
            !origin.same_origin_url(&head.final_url) || !origin.same_origin_url(&next_url)
        })
    {
        return Err("CORS redirect URL must not include credentials".to_owned());
    }
    if !moli_url::same_origin(&head.final_url, &next_url) {
        request
            .request_headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
        if request
            .auth()
            .is_some_and(|auth| auth.target == moli_fetch::RequestAuthTarget::Server)
        {
            request.set_auth(None);
        }
    }

    let redirect_status = head.status;
    let response_extra_info = request_extra_info.cloned().map(|request_extra_info| {
        moli_fetch::NetworkResponseExtraInfo {
            request_extra_info,
            status: redirect_status,
            headers: head.headers.clone(),
            cookie_set_reports: head.cookie_set_reports.clone(),
        }
    });
    request
        .follow_redirect(RedirectInfo {
            source,
            from_url: head.final_url,
            to_url: next_url.clone(),
            status: redirect_status,
            headers: head.headers,
            network_extra_info_available,
            request_extra_info: None,
            dispatched_request: None,
            response_extra_info,
            redirect_has_extra_info: network_extra_info_available,
            request_cookie_report: head.request_cookie_report,
            cookie_set_reports: head.cookie_set_reports,
            from_cache: head.from_cache,
            negotiated_http_version: head.negotiated_http_version,
        })
        .map_err(|error| error.to_string())?;

    Ok(SubresourceResponseTransition::FollowedRedirect)
}

pub(super) fn redirect_mode_error_message(url: &url::Url) -> String {
    format!("redirect mode error blocked redirect from {url}")
}
