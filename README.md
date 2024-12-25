# de-mls

Decentralized MLS PoC using a smart contract for group coordination

## Run Test Waku Node

```bash
docker run -p 8645:8645 -p 60000:60000 wakuorg/nwaku:v0.33.1 --cluster-id=15 --rest --relay --rln-relay=false --pubsub-topic=/waku/2/rs/15/0
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
