use super::ResourcePerformanceEntry;
use crate::types::AsyncSubresourceFetchResponseFilter;
use moli_fetch::{RequestMode, ResponseCacheState, ResponseHead};
use moli_url::WebOrigin;
use url::Url;

/// Request-time metadata, independent of the callback or Realm that happens to
/// be active when the body finishes. This must not retain any V8 objects.
#[derive(Clone)]
pub(crate) struct FetchResourceTiming {
    initiator_type: &'static str,
    started: std::time::Instant,
    start_unix_millis: f64,
    response_timing: std::cell::RefCell<Option<Box<FetchResourceResponseTiming>>>,
    initiator_url: String,
}

#[derive(Clone, Default)]
struct FetchResourceResponseTiming {
    request_start: Option<f64>,
    body_sizes: Option<moli_fetch::FetchResponseBodySizes>,
    final_headers_start: Option<f64>,
    first_interim_start: Option<f64>,
    end: Option<f64>,
}

impl FetchResourceTiming {
    pub(crate) fn capture(scope: &mut v8::PinScope<'_, '_>, origin: &WebOrigin) -> Self {
        Self::capture_for_initiator_type(scope, origin, "fetch")
    }

    pub(crate) fn capture_for_initiator_type(
        scope: &mut v8::PinScope<'_, '_>,
        origin: &WebOrigin,
        initiator_type: &'static str,
    ) -> Self {
        Self::from_captured_initiator(
            initiator_type,
            crate::resource_initiator::current_url(scope, origin),
        )
    }

    pub(crate) fn from_captured_initiator(
        initiator_type: &'static str,
        initiator_url: String,
    ) -> Self {
        Self {
            initiator_type,
            started: std::time::Instant::now(),
            start_unix_millis: crate::context_bootstrap::unix_epoch_millis(),
            response_timing: Default::default(),
            initiator_url,
        }
    }

    fn now(&self) -> f64 {
        self.start_unix_millis + self.started.elapsed().as_secs_f64() * 1000.0
    }

    pub(crate) fn response_started(&self) {
        *self.response_timing.borrow_mut() = Some(Box::new(FetchResourceResponseTiming {
            final_headers_start: Some(self.now()),
            ..Default::default()
        }));
    }

    /// Use the transport's header boundary even when Worker JS delayed delivery.
    pub(crate) fn response_started_at(&self, start: Option<std::time::Instant>) {
        *self.response_timing.borrow_mut() = Some(Box::new(FetchResourceResponseTiming {
            final_headers_start: start.map(|instant| self.unix_millis(instant)),
            ..Default::default()
        }));
    }

    pub(crate) fn completed_at(
        &self,
        mut entry: ResourcePerformanceEntry,
        end: std::time::Instant,
    ) -> ResourcePerformanceEntry {
        entry.end_unix_millis = Some(self.unix_millis(end));
        entry
    }

    fn unix_millis(&self, instant: std::time::Instant) -> f64 {
        self.start_unix_millis
            + instant
                .saturating_duration_since(self.started)
                .as_secs_f64()
                * 1000.0
    }

    pub(crate) fn observe_native_timing(&self, native: moli_fetch::FetchResponseTiming) {
        let request_start = native
            .request_started_at()
            .map(|value| self.unix_millis(value));
        let final_start = native
            .response_started_at()
            .map(|value| self.unix_millis(value));
        let first_interim = native
            .first_interim_response_started_at()
            .map(|value| self.unix_millis(value));
        let end = native
            .response_end_at()
            .map(|value| self.unix_millis(value));
        if request_start.is_none()
            && final_start.is_none()
            && first_interim.is_none()
            && end.is_none()
            && native.body_sizes().is_none()
        {
            return;
        }
        let mut response_timing = self.response_timing.borrow_mut();
        let timing = response_timing.get_or_insert_with(Default::default);
        timing.request_start = request_start;
        timing.body_sizes = native.body_sizes();
        if final_start.is_some() {
            timing.final_headers_start = final_start;
            timing.first_interim_start = first_interim;
        }
        if end.is_some() {
            timing.end = end;
        }
    }

    fn end(&self) -> f64 {
        self.response_timing
            .borrow()
            .as_ref()
            .and_then(|timing| timing.end)
            .unwrap_or_else(|| self.now())
    }

    pub(crate) fn completed(
        &self,
        url: &Url,
        origin: &WebOrigin,
        mode: RequestMode,
        head: &ResponseHead,
        body_size: usize,
        filter: Option<&AsyncSubresourceFetchResponseFilter>,
    ) -> ResourcePerformanceEntry {
        let network_filter = crate::network_host::network_response_filter(
            origin,
            head,
            mode,
            moli_fetch::RequestRedirectMode::Follow,
        );
        let filter = filter.or(network_filter.as_ref());
        let readable = filter.is_none_or(|filter| filter.is_readable());
        let mut entry = ResourcePerformanceEntry::from_streaming_network_response(
            url.as_str(),
            self.initiator_type,
            Some(self.start_unix_millis),
            head,
            body_size,
        );
        // CORS controls response metadata and script attribution. TAO controls
        // timing/body sizes separately, including for opaque responses.
        if readable && head.cache_state == ResponseCacheState::None {
            entry.initiator_url.clone_from(&self.initiator_url);
        }
        self.apply_native_body_sizes(&mut entry, head);
        if !readable {
            entry.response_status = 0.0;
            entry.content_type.clear();
            entry.content_encoding.clear();
        }
        if filter.is_some_and(|filter| filter.service_worker_metadata().is_some()) {
            // Bytes delivered by a Service Worker are not a network transfer
            // to this global, even when respondWith forwards a fetched body.
            entry.transfer_size = 0.0;
        }
        entry.end_unix_millis = Some(self.end());
        if Self::allows_timing(origin, head, filter) {
            if let Some(timing) = self.response_timing.borrow().as_ref() {
                entry.request_start_unix_millis = timing.request_start;
                entry.final_response_headers_start_unix_millis = timing.final_headers_start;
                entry.first_interim_response_start_unix_millis = timing.first_interim_start;
                entry.response_start_unix_millis =
                    timing.first_interim_start.or(timing.final_headers_start);
            }
            entry.next_hop_protocol = head
                .negotiated_http_version
                .map(|version| version.protocol_name().to_owned())
                .unwrap_or_default();
        }
        if !Self::allows_timing(origin, head, filter) {
            entry.delivery_type.clear();
            entry.transfer_size = 0.0;
            entry.encoded_body_size = 0.0;
            entry.decoded_body_size = 0.0;
        }
        entry
    }

    fn apply_native_body_sizes(&self, entry: &mut ResourcePerformanceEntry, head: &ResponseHead) {
        if let Some(sizes) = self
            .response_timing
            .borrow()
            .as_ref()
            .and_then(|timing| timing.body_sizes)
        {
            entry.encoded_body_size = sizes.encoded as f64;
            entry.decoded_body_size = sizes.decoded as f64;
            entry.transfer_size = match head.cache_state {
                ResponseCacheState::Local => 0.0,
                ResponseCacheState::Validated => 300.0,
                ResponseCacheState::None => entry.encoded_body_size + 300.0,
            };
            entry.content_encoding = if sizes.encoded > 0 {
                moli_fetch::resource_timing_content_encoding(&head.headers).to_owned()
            } else {
                String::new()
            };
        }
    }

    fn allows_timing(
        origin: &WebOrigin,
        head: &ResponseHead,
        filter: Option<&AsyncSubresourceFetchResponseFilter>,
    ) -> bool {
        filter
            .and_then(|filter| filter.service_worker_metadata())
            .map_or_else(
                || head.timing_allow_passed(origin),
                |metadata| metadata.timing_allow_passed,
            )
    }

    pub(crate) fn classic_script_response(
        &self,
        name: &Url,
        origin: &WebOrigin,
        head: &ResponseHead,
        body_size: usize,
    ) -> ResourcePerformanceEntry {
        self.completed(name, origin, RequestMode::NoCors, head, body_size, None)
    }

    pub(crate) fn failed(&self, url: &Url) -> ResourcePerformanceEntry {
        let mut entry = ResourcePerformanceEntry::from_network_failure(
            url.as_str(),
            self.initiator_type,
            Some(self.start_unix_millis),
        );
        entry.end_unix_millis = Some(self.end());
        entry
    }
}
