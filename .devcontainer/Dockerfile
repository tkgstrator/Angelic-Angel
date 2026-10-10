FROM mcr.microsoft.com/devcontainers/base:dev-ubuntu24.04

RUN apt-get update \
  && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev cmake \
  && rm -rf /var/lib/apt/lists/*
