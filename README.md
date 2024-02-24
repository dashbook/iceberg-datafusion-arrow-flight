# Arrow Flight SQL Server with Datafusion and Iceberg

Uses the Sql catalog for iceberg

## Environment Variables

- CATALOG_URL - Url of the iceberg sql catalog
- BUCKET - Bucket of the cloud object store
- FLIGHT_USER - Username to use for basic auth when connecting to the Server
- FLIGHT_PASSWORD - Password to use for the basic auth when connecting to the Server
- TLS_DOMAIN - Domain to use for the TLS certificate
- CURRENT_DATABASE - Return value to set for the current_database() function

### AWS S3 Object Store

- AWS_DEFAULT_REGION
- AWS_ACCESS_KEY_ID
- AWS_SECRET_ACCESS_KEY
- AWS_ENDPOINT
