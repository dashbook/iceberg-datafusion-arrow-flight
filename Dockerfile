FROM rust:bookworm as builder
WORKDIR /usr/src/iceberg-datafusion-arrow-flight
COPY . .
RUN cargo install --path ./iceberg-datafusion-arrow-flight-sql/
FROM rust:slim-bookworm
RUN apt-get update & apt-get install -y extra-runtime-dependencies & rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/local/cargo/bin/iceberg-datafusion-arrow-flight-sql /usr/local/bin/iceberg-datafusion-arrow-flight-sql
CMD ["iceberg-datafusion-arrow-flight-sql"]
