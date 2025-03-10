#[cfg(all(test, feature = "datadog-agent-integration-tests"))]
mod integration_tests;
#[cfg(test)]
mod tests;

pub mod logs;
pub mod metrics;
pub mod traces;

#[allow(warnings, clippy::pedantic, clippy::nursery)]
pub(crate) mod ddmetric_proto {
    include!(concat!(env!("OUT_DIR"), "/datadog.agentpayload.rs"));
}

#[allow(warnings)]
pub(crate) mod ddtrace_proto {
    include!(concat!(env!("OUT_DIR"), "/dd_trace.rs"));
}

use std::{fmt::Debug, io::Read, net::SocketAddr, sync::Arc};

use bytes::{Buf, Bytes};
use chrono::{serde::ts_milliseconds, DateTime, Utc};
use codecs::decoding::{DeserializerConfig, FramingConfig};
use flate2::read::{MultiGzDecoder, ZlibDecoder};
use futures::FutureExt;
use http::StatusCode;
use lookup::owned_value_path;
use regex::Regex;
use serde::{Deserialize, Serialize};
use snafu::Snafu;
use tracing::Span;
use vector_common::internal_event::{EventsReceived, Registered};
use vector_config::configurable_component;
use vector_core::config::{LegacyKey, LogNamespace};
use vector_core::event::{BatchNotifier, BatchStatus};
use vrl::path::OwnedTargetPath;
use vrl::value::Kind;
use warp::{filters::BoxedFilter, reject::Rejection, reply::Response, Filter, Reply};

use crate::{
    codecs::{Decoder, DecodingConfig},
    config::{
        log_schema, DataType, GenerateConfig, Resource, SourceAcknowledgementsConfig, SourceConfig,
        SourceContext, SourceOutput,
    },
    event::Event,
    internal_events::{HttpBytesReceived, HttpDecompressError, StreamClosedError},
    schema,
    serde::{bool_or_struct, default_decoding, default_framing_message_based},
    sources::{self, util::ErrorMessage},
    tls::{MaybeTlsSettings, TlsEnableableConfig},
    SourceSender,
};

pub const LOGS: &str = "logs";
pub const METRICS: &str = "metrics";
pub const TRACES: &str = "traces";

/// Configuration for the `datadog_agent` source.
#[configurable_component(source(
    "datadog_agent",
    "Receive logs, metrics, and traces collected by a Datadog Agent."
))]
#[derive(Clone, Debug)]
pub struct DatadogAgentConfig {
    /// The socket address to accept connections on.
    ///
    /// It _must_ include a port.
    #[configurable(metadata(docs::examples = "0.0.0.0:80"))]
    #[configurable(metadata(docs::examples = "localhost:80"))]
    address: SocketAddr,

    /// If this is set to `true`, when incoming events contain a Datadog API key, it is
    /// stored in the event metadata and used if the event is sent to a Datadog sink.
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::serde::default_true")]
    store_api_key: bool,

    /// If this is set to `true`, logs are not accepted by the component.
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::serde::default_false")]
    disable_logs: bool,

    /// If this is set to `true`, metrics are not accepted by the component.
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::serde::default_false")]
    disable_metrics: bool,

    /// If this is set to `true`, traces are not accepted by the component.
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::serde::default_false")]
    disable_traces: bool,

    /// If this is set to `true` logs, metrics, and traces are sent to different outputs.
    ///
    ///
    /// For a source component named `agent`, the received logs, metrics, and traces can then be
    /// configured as input to other components by specifying `agent.logs`, `agent.metrics`, and
    /// `agent.traces`, respectively.
    #[configurable(metadata(docs::advanced))]
    #[serde(default = "crate::serde::default_false")]
    multiple_outputs: bool,

    /// The namespace to use for logs. This overrides the global setting.
    #[serde(default)]
    #[configurable(metadata(docs::hidden))]
    log_namespace: Option<bool>,

    #[configurable(derived)]
    tls: Option<TlsEnableableConfig>,

    #[configurable(derived)]
    #[serde(default = "default_framing_message_based")]
    framing: FramingConfig,

    #[configurable(derived)]
    #[serde(default = "default_decoding")]
    decoding: DeserializerConfig,

    #[configurable(derived)]
    #[serde(default, deserialize_with = "bool_or_struct")]
    acknowledgements: SourceAcknowledgementsConfig,
}

impl GenerateConfig for DatadogAgentConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self {
            address: "0.0.0.0:8080".parse().unwrap(),
            tls: None,
            store_api_key: true,
            framing: default_framing_message_based(),
            decoding: default_decoding(),
            acknowledgements: SourceAcknowledgementsConfig::default(),
            disable_logs: false,
            disable_metrics: false,
            disable_traces: false,
            multiple_outputs: false,
            log_namespace: Some(false),
        })
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_agent")]
impl SourceConfig for DatadogAgentConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<sources::Source> {
        let log_namespace = cx.log_namespace(self.log_namespace);

        let logs_schema_definition = cx
            .schema_definitions
            .get(&Some(LOGS.to_owned()))
            .or_else(|| cx.schema_definitions.get(&None))
            .expect("registered log schema required")
            .clone();

        let decoder =
            DecodingConfig::new(self.framing.clone(), self.decoding.clone(), log_namespace)
                .build()?;

        let tls = MaybeTlsSettings::from_config(&self.tls, true)?;
        let source = DatadogAgentSource::new(
            self.store_api_key,
            decoder,
            tls.http_protocol_name(),
            logs_schema_definition,
            log_namespace,
        );
        let listener = tls.bind(&self.address).await?;
        let acknowledgements = cx.do_acknowledgements(self.acknowledgements);
        let filters = source.build_warp_filters(cx.out, acknowledgements, self)?;
        let shutdown = cx.shutdown;

        info!(message = "Building HTTP server.", address = %self.address);

        Ok(Box::pin(async move {
            let span = Span::current();
            let routes = filters
                .with(warp::trace(move |_info| span.clone()))
                .recover(|r: Rejection| async move {
                    if let Some(e_msg) = r.find::<ErrorMessage>() {
                        let json = warp::reply::json(e_msg);
                        Ok(warp::reply::with_status(json, e_msg.status_code()))
                    } else {
                        // other internal error - will return 500 internal server error
                        Err(r)
                    }
                });

            warp::serve(routes)
                .serve_incoming_with_graceful_shutdown(
                    listener.accept_stream(),
                    shutdown.map(|_| ()),
                )
                .await;

            Ok(())
        }))
    }

    fn outputs(&self, global_log_namespace: LogNamespace) -> Vec<SourceOutput> {
        let definition = self
            .decoding
            .schema_definition(global_log_namespace.merge(self.log_namespace))
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("status"))),
                &owned_value_path!("status"),
                Kind::bytes(),
                Some("severity"),
            )
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("timestamp"))),
                &owned_value_path!("timestamp"),
                Kind::timestamp(),
                Some("timestamp"),
            )
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("hostname"))),
                &owned_value_path!("hostname"),
                Kind::bytes(),
                Some("host"),
            )
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("service"))),
                &owned_value_path!("service"),
                Kind::bytes(),
                Some("service"),
            )
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("ddsource"))),
                &owned_value_path!("ddsource"),
                Kind::bytes(),
                Some("source"),
            )
            .with_source_metadata(
                Self::NAME,
                Some(LegacyKey::InsertIfEmpty(owned_value_path!("ddtags"))),
                &owned_value_path!("ddtags"),
                Kind::bytes(),
                Some("tags"),
            )
            .with_standard_vector_source_metadata();

        if self.multiple_outputs {
            vec![
                SourceOutput::new_logs(DataType::Log, definition).with_port(LOGS),
                SourceOutput::new_metrics().with_port(METRICS),
                SourceOutput::new_traces().with_port(TRACES),
            ]
        } else {
            vec![SourceOutput::new_logs(DataType::all(), definition)]
        }
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::tcp(self.address)]
    }

    fn can_acknowledge(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Snafu)]
pub(crate) enum ApiError {
    BadRequest,
    InvalidDataFormat,
    ServerShutdown,
}

impl warp::reject::Reject for ApiError {}

#[derive(Deserialize)]
pub struct ApiKeyQueryParams {
    #[serde(rename = "dd-api-key")]
    pub dd_api_key: Option<String>,
}

#[derive(Clone)]
pub(crate) struct DatadogAgentSource {
    pub(crate) api_key_extractor: ApiKeyExtractor,
    pub(crate) log_schema_host_key: OwnedTargetPath,
    pub(crate) log_schema_source_type_key: OwnedTargetPath,
    pub(crate) log_namespace: LogNamespace,
    pub(crate) decoder: Decoder,
    protocol: &'static str,
    logs_schema_definition: Arc<schema::Definition>,
    events_received: Registered<EventsReceived>,
}

#[derive(Clone)]
pub struct ApiKeyExtractor {
    matcher: Regex,
    store_api_key: bool,
}

impl ApiKeyExtractor {
    pub fn extract(
        &self,
        path: &str,
        header: Option<String>,
        query_params: Option<String>,
    ) -> Option<Arc<str>> {
        if !self.store_api_key {
            return None;
        }
        // Grab from URL first
        self.matcher
            .captures(path)
            .and_then(|cap| cap.name("api_key").map(|key| key.as_str()).map(Arc::from))
            // Try from query params
            .or_else(|| query_params.map(Arc::from))
            // Try from header next
            .or_else(|| header.map(Arc::from))
    }
}

impl DatadogAgentSource {
    pub(crate) fn new(
        store_api_key: bool,
        decoder: Decoder,
        protocol: &'static str,
        logs_schema_definition: schema::Definition,
        log_namespace: LogNamespace,
    ) -> Self {
        Self {
            api_key_extractor: ApiKeyExtractor {
                store_api_key,
                matcher: Regex::new(r"^/v1/input/(?P<api_key>[[:alnum:]]{32})/??")
                    .expect("static regex always compiles"),
            },
            log_schema_host_key: log_schema()
                .host_key_target_path()
                .expect("global log_schema.host_key to be valid path")
                .clone(),
            log_schema_source_type_key: log_schema()
                .source_type_key_target_path()
                .expect("global log_schema.source_type_key to be valid path")
                .clone(),
            decoder,
            protocol,
            logs_schema_definition: Arc::new(logs_schema_definition),
            log_namespace,
            events_received: register!(EventsReceived),
        }
    }

    fn build_warp_filters(
        &self,
        out: SourceSender,
        acknowledgements: bool,
        config: &DatadogAgentConfig,
    ) -> crate::Result<BoxedFilter<(Response,)>> {
        let mut filters = (!config.disable_logs).then(|| {
            logs::build_warp_filter(
                acknowledgements,
                config.multiple_outputs,
                out.clone(),
                self.clone(),
            )
        });

        if !config.disable_traces {
            let trace_filter = traces::build_warp_filter(
                acknowledgements,
                config.multiple_outputs,
                out.clone(),
                self.clone(),
            );
            filters = filters
                .map(|f| f.or(trace_filter.clone()).unify().boxed())
                .or(Some(trace_filter));
        }

        if !config.disable_metrics {
            let metrics_filter = metrics::build_warp_filter(
                acknowledgements,
                config.multiple_outputs,
                out,
                self.clone(),
            );
            filters = filters
                .map(|f| f.or(metrics_filter.clone()).unify().boxed())
                .or(Some(metrics_filter));
        }

        filters.ok_or_else(|| "At least one of the supported data type shall be enabled".into())
    }

    pub(crate) fn decode(
        &self,
        header: &Option<String>,
        mut body: Bytes,
        path: &str,
    ) -> Result<Bytes, ErrorMessage> {
        if let Some(encodings) = header {
            for encoding in encodings.rsplit(',').map(str::trim) {
                body = match encoding {
                    "identity" => body,
                    "gzip" | "x-gzip" => {
                        let mut decoded = Vec::new();
                        MultiGzDecoder::new(body.reader())
                            .read_to_end(&mut decoded)
                            .map_err(|error| handle_decode_error(encoding, error))?;
                        decoded.into()
                    }
                    "deflate" | "x-deflate" => {
                        let mut decoded = Vec::new();
                        ZlibDecoder::new(body.reader())
                            .read_to_end(&mut decoded)
                            .map_err(|error| handle_decode_error(encoding, error))?;
                        decoded.into()
                    }
                    encoding => {
                        return Err(ErrorMessage::new(
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            format!("Unsupported encoding {}", encoding),
                        ))
                    }
                }
            }
        }
        emit!(HttpBytesReceived {
            byte_size: body.len(),
            http_path: path,
            protocol: self.protocol,
        });
        Ok(body)
    }
}

pub(crate) async fn handle_request(
    events: Result<Vec<Event>, ErrorMessage>,
    acknowledgements: bool,
    mut out: SourceSender,
    output: Option<&str>,
) -> Result<Response, Rejection> {
    match events {
        Ok(mut events) => {
            let receiver = BatchNotifier::maybe_apply_to(acknowledgements, &mut events);
            let count = events.len();

            if let Some(name) = output {
                out.send_batch_named(name, events).await
            } else {
                out.send_batch(events).await
            }
            .map_err(|_| {
                emit!(StreamClosedError { count });
                warp::reject::custom(ApiError::ServerShutdown)
            })?;
            match receiver {
                None => Ok(warp::reply().into_response()),
                Some(receiver) => match receiver.await {
                    BatchStatus::Delivered => Ok(warp::reply().into_response()),
                    BatchStatus::Errored => Err(warp::reject::custom(ErrorMessage::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Error delivering contents to sink".into(),
                    ))),
                    BatchStatus::Rejected => Err(warp::reject::custom(ErrorMessage::new(
                        StatusCode::BAD_REQUEST,
                        "Contents failed to deliver to sink".into(),
                    ))),
                },
            }
        }
        Err(err) => Err(warp::reject::custom(err)),
    }
}

fn handle_decode_error(encoding: &str, error: impl std::error::Error) -> ErrorMessage {
    emit!(HttpDecompressError {
        encoding,
        error: &error
    });
    ErrorMessage::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("Failed decompressing payload with {} decoder.", encoding),
    )
}

// https://github.com/DataDog/datadog-agent/blob/a33248c2bc125920a9577af1e16f12298875a4ad/pkg/logs/processor/json.go#L23-L49
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LogMsg {
    pub message: Bytes,
    pub status: Bytes,
    #[serde(
        deserialize_with = "ts_milliseconds::deserialize",
        serialize_with = "ts_milliseconds::serialize"
    )]
    pub timestamp: DateTime<Utc>,
    pub hostname: Bytes,
    pub service: Bytes,
    pub ddsource: Bytes,
    pub ddtags: Bytes,
}
