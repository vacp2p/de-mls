# de-mls

Decentralized MLS PoC using a smart contract for group coordination

> Note: The frontend implementation is based on [chatr](https://github.com/0xLaurens/chatr), a real-time chat application built with Rust and SvelteKit

## Run Test Waku Node

This node is used to easially connect different instances of the app between each other.

```bash
docker run -p 8645:8645 -p 60000:60000 wakuorg/nwaku:v0.33.1 --cluster-id=15 --rest --relay --rln-relay=false --pubsub-topic=/waku/2/rs/15/1
```

## Run User Instance

Create a `.env` file in the `.env` folder for each client containing the following variables:

```text
NAME=client1
BACKEND_PORT=3000
FRONTEND_PORT=4000
NODE_PORT=60000
PEER_ADDRESSES=[/ip4/x.x.x.x/tcp/60000/p2p/xxxx...xxxx]
```

Run docker compose up for the user instance

```bash
docker-compose --env-file ./.env/client1.env up --build
```

For each client, run the following command to start the frontend on the local host with the port specified in the `.env` file

Run from the frontend directory

```bash
PUBLIC_API_URL=http://0.0.0.0:3000 PUBLIC_WEBSOCKET_URL=ws://localhost:3000 npm run dev
```

Run from the root directory

```bash
RUST_BACKTRACE=full RUST_LOG=info NODE_PORT=60001 PEER_ADDRESSES=/ip4/x.x.x.x/tcp/60000/p2p/xxxx...xxxx,/ip4/y.y.y.y/tcp/60000/p2p/yyyy...yyyy cargo run --  --nocapture
```

### Example of ban user

In chat message block run ban command, note that user wallet address should be in the format without `0x`

```bash
/ban f39555ce6ab55579cfffb922665825e726880af6
```
