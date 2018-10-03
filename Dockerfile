FROM rust:latest

# Clone and build the graph-node repository
RUN cd /tmp \
    && git clone https://github.com/graphprotocol/graph-node \
    && cd graph-node \
    && $HOME/.cargo/bin/cargo install --release --path node

# Start everything on startup
ADD deployment/start-node.sh /usr/local/bin
CMD start-node.sh
