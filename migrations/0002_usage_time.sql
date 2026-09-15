-- All-user reports and request history filter by endpoint and time before grouping.
CREATE INDEX usage_endpoint_time ON request_usage(endpoint, started_at DESC, id DESC);
