FROM nginx:latest AS builder

# Create the build objects that correspond to nginx:latest
#
WORKDIR /tmp
RUN apt-get update -qq && apt-get install -y --no-install-recommends wget git gcc make libpcre2-dev zlib1g-dev libclang-dev && \
    wget -O - https://nginx.org/download/`nginx -v 2>&1 | tr / - | cut -f3 -d' '`.tar.gz | tar xfz - && \
    mv nginx-* nginx && \
    cd nginx && \
    ./configure --with-compat && \
    make modules

# Install Rust toolchain and make it available to the default shell
#
RUN wget --https-only -O - https://sh.rustup.rs | bash -s -- -y && \
    rm -fr /usr/local/bin && \
    ln -s /root/.cargo/bin /usr/local/bin

# Build the nginx-tokenizer module from local repo clone
#
COPY . /tmp/nginx-tokenizer
WORKDIR /tmp/nginx-tokenizer
RUN NGINX_SOURCE_DIR=/tmp/nginx cargo build --release && \
    strip target/release/libnginx_tokenizer.so -o /tmp/ngx_http_tokenizer_module.so

# Create final image with the tokenizer module loaded
#
FROM nginx:latest
COPY --from=builder /tmp/ngx_http_tokenizer_module.so /etc/nginx/modules
RUN sed -i "s/events/load_module modules\/ngx_http_tokenizer_module.so;\nevents/" /etc/nginx/nginx.conf
