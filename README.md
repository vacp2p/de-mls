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

## Steward State Management

The system implements a robust state machine for managing steward epochs with the following states:

### States

- **Working**: Normal operation where all users can send any message type freely
- **Waiting**: Steward epoch active, only steward can send BATCH_PROPOSALS_MESSAGE
- **Voting**: Consensus voting phase with only voting-related messages:
  - Everyone: VOTE, USER_VOTE
  - Steward only: VOTING_PROPOSAL, PROPOSAL
  - All other messages blocked during voting

### State Transitions

```text
Working --start_steward_epoch()--> Waiting (if proposals exist)
Working --start_steward_epoch()--> Working (if no proposals - no state change)
Waiting --start_voting()---------> Voting
Waiting --no_proposals_found()---> Working (edge case: proposals disappear)
Voting --complete_voting(YES)----> Waiting --apply_proposals()--> Working
Voting --complete_voting(NO)-----> Working
```

### Steward Flow Scenarios

1. **No Proposals**: Steward stays in Working state throughout epoch
2. **Successful Vote**: 
   - **Steward**: Working → Waiting → Voting → Waiting → Working
   - **Non-Steward**: Working → Waiting → Voting → Working
3. **Failed Vote**: 
   - **Steward**: Working → Waiting → Voting → Working  
   - **Non-Steward**: Working → Waiting → Voting → Working
4. **Edge Case**: Working → Waiting → Working (if proposals disappear during voting)

### Guarantees

- Steward always returns to Working state after epoch completion
- No infinite loops or stuck states
- All edge cases properly handled
- Robust error handling with detailed logging

### Example of ban user

In chat message block run ban command, note that user wallet address should be in the format without `0x`

```bash
/ban f39555ce6ab55579cfffb922665825e726880af6
```
