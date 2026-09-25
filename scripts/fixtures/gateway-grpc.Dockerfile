FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-grpcio && rm -rf /var/lib/apt/lists/*
USER 10101:10101
ENTRYPOINT ["python3", "/app/grpc_origin.py"]
