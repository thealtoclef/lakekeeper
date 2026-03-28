use async_trait::async_trait;
use cloudevents::{AttributesReader, Event};
use reqwest::Client;
use url::Url;

use crate::{CONFIG, service::events::CloudEventBackend};

#[derive(Debug)]
pub struct RisingWaveBackend {
    pub client: Client,
    pub webhook_url: Url,
}

/// Build a RisingWave publisher from the crate configuration.
/// Returns `None` if the RisingWave webhook URL is not set.
///
/// # Errors
/// - If the URL is configured but invalid.
pub fn build_risingwave_publisher_from_config() -> anyhow::Result<Option<RisingWaveBackend>> {
    let Some(webhook_url) = CONFIG.risingwave_webhook_url.clone() else {
        tracing::info!(
            "RisingWave webhook URL not set. Events are not published to RisingWave."
        );
        return Ok(None);
    };

    let backend = RisingWaveBackend {
        client: Client::new(),
        webhook_url,
    };

    tracing::info!(
        "Publishing events to RisingWave webhook at {}",
        backend.webhook_url,
    );
    Ok(Some(backend))
}

#[async_trait]
impl CloudEventBackend for RisingWaveBackend {
    async fn publish(&self, event: Event) -> anyhow::Result<()> {
        let payload = build_payload(&event)?;

        let response = self
            .client
            .post(self.webhook_url.as_str())
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!(e).context(format!(
                    "Failed to POST event to RisingWave webhook at {}",
                    self.webhook_url
                ))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("RisingWave webhook returned {status}: {body}");
        }

        tracing::debug!("CloudEvents event sent to RisingWave webhook");
        Ok(())
    }

    fn name(&self) -> &str {
        "risingwave-publisher"
    }
}

/// Build a JSON payload wrapping all CloudEvent fields into a single object.
/// The RisingWave webhook source ingests this as one JSONB column.
fn build_payload(event: &Event) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": event.id(),
        "type": event.ty(),
        "source": event.source().to_string(),
        "namespace": ext_str(event, "namespace"),
        "name": ext_str(event, "name"),
        "tabular_id": ext_str(event, "tabular-id"),
        "warehouse_id": ext_str(event, "warehouse-id"),
        "data": event.data().map(|d| match d {
            cloudevents::Data::Json(v) => v.clone(),
            cloudevents::Data::String(s) => serde_json::Value::String(s.clone()),
            cloudevents::Data::Binary(b) => serde_json::Value::String(
                String::from_utf8_lossy(b).into_owned()
            ),
        }),
        "trace_id": ext_str(event, "trace-id"),
    }))
}

fn ext_str(event: &Event, key: &str) -> String {
    event
        .extension(key)
        .map(|v| v.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cloudevents::{EventBuilder, EventBuilderV10};

    #[test]
    fn test_build_payload() {
        let event = EventBuilderV10::new()
            .id("test-id-123")
            .source("uri:iceberg-catalog-service:localhost")
            .ty("createTable")
            .data("application/json", serde_json::json!({"key": "value"}))
            .extension("namespace", "my_database")
            .extension("name", "my_table")
            .extension("tabular-id", "550e8400-e29b-41d4-a716-446655440000")
            .extension("warehouse-id", "6ba7b810-9dad-11d1-80b4-00c04fd430c8")
            .extension("trace-id", "trace-123")
            .build()
            .unwrap();

        let payload = build_payload(&event).unwrap();

        assert_eq!(payload["id"], "test-id-123");
        assert_eq!(payload["type"], "createTable");
        assert_eq!(payload["source"], "uri:iceberg-catalog-service:localhost");
        assert_eq!(payload["namespace"], "my_database");
        assert_eq!(payload["name"], "my_table");
        assert_eq!(payload["tabular_id"], "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(payload["warehouse_id"], "6ba7b810-9dad-11d1-80b4-00c04fd430c8");
        assert_eq!(payload["trace_id"], "trace-123");
        assert!(payload["data"].is_object());
    }

    #[test]
    fn test_build_payload_missing_extensions() {
        let event = EventBuilderV10::new()
            .id("id")
            .source("src")
            .ty("dropTable")
            .build()
            .unwrap();

        let payload = build_payload(&event).unwrap();
        assert_eq!(payload["namespace"], "");
        assert_eq!(payload["name"], "");
        assert!(payload["data"].is_null());
    }
}
