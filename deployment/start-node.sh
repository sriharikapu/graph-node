#!/bin/bash

set -e
set -x

echo "Start Graph node"

graph-node \
  --postgres-url "postgresql://$POSTGRES_USER:$POSTGRES_PASSWORD@$POSTGRES_URL/$POSTGRES_DB" \
  --ethereum-rpc "$ETHEREUM_RPC_URL" \
  --ipfs $IPFS_URL
