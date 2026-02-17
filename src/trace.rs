// Copyright 2025 Union.ai. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::io::Result;

use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

/// Initialize the OpenTelemetry tracing pipeline with an OTLP gRPC exporter.
///
/// The collector endpoint is configured via the `OTEL_EXPORTER_OTLP_ENDPOINT`
/// environment variable, defaulting to `http://localhost:4318`.
///
/// Returns the [`SdkTracerProvider`] which must be kept alive for the
/// lifetime of the application. Call [`SdkTracerProvider::shutdown`] before
/// exiting to flush any pending spans.
pub fn setup_tracing(service_name: String) -> Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
        .map_err(|e| eother!(format!("failed to create OTLP span exporter: {e}")))?;

    let resource = Resource::builder()
        .with_service_name(service_name.clone())
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer(service_name);
    let telemetry_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let subscriber = Registry::default().with(telemetry_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| eother!(format!("failed to set global tracing subscriber: {e}")))?;

    Ok(provider)
}
