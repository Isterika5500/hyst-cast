# Сервер hyst-cast (собирается на самом сервере, Oracle ARM)
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p hyst-cast-server

FROM alpine:3
COPY --from=build /src/target/release/hyst-cast-server /usr/local/bin/
WORKDIR /data
ENV HYST_CAST_DATA=/data HYST_CAST_PORT=4433
ENTRYPOINT ["hyst-cast-server"]
