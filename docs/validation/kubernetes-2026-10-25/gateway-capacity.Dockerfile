FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 nginx wrk && rm -rf /var/lib/apt/lists/*
USER 10101:10101
CMD ["sleep", "infinity"]
