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
NODE_NAME=<waku-node-ip>
```

Run docker compose up for the user instance

```bash
docker-compose --env-file ./.env/client1.env up --build
```

For each client, run the following command to start the frontend on the local host with the port specified in the `.env` file
