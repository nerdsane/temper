# Context is produced by prepare.py; it contains no credentials.
FROM ubuntu:24.04
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates libssl3t64 libstdc++6 python3 nginx && rm -rf /var/lib/apt/lists/*
COPY temper /usr/local/bin/temper
COPY node /usr/local/bin/node
COPY libz3.so /usr/local/lib/libz3.so
COPY libz3.so.4 /usr/local/lib/libz3.so.4
ENV LD_LIBRARY_PATH=/usr/local/lib
RUN mkdir -p /etc/temper /var/lib/temper /root/.local/share && ln -s /var/lib/temper /root/.local/share/temper
COPY observe /opt/temper/observe
COPY start.sh /opt/temper/start.sh
COPY nginx.conf /etc/nginx/nginx.conf
COPY build.json /opt/temper/build.json
RUN chmod 755 /opt/temper/start.sh && temper --help && node --version
USER root
WORKDIR /opt/temper
EXPOSE 3000 8080
