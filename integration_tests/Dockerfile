FROM rust:bookworm
SHELL ["/bin/bash", "-c"]

RUN apt-get update
RUN apt-get install -y git socat procps

COPY . /opt/ecdysis
WORKDIR /opt/ecdysis
