<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import {
    user,
    channel,
    eth_private_key,
    group,
    createNewRoom,
  } from "../../lib/stores/user";
  import { goto } from "$app/navigation";
  import { env } from "$env/dynamic/public";
  import toast from "svelte-french-toast";
  import { json } from "@sveltejs/kit";

  let status = "🔴";
  let statusTip = "Disconnected";
  let message = "";
  let messages: any[] = [];
  let socket: WebSocket;
  let interval: number;
  let delay = 2000;
  let timeout = false;

  // Voting state
  let currentVotingProposal: any = null;
  let showVotingUI = false;

  $: {
    if (interval || (!timeout && interval)) {
      clearInterval(interval);
    }

    if (timeout == true) {
      interval = setInterval(() => {
        if (delay < 30_000) delay = delay * 2;
        console.log("reconnecting in:", delay);
        connect();
      }, delay);
    }
  }

  function connect() {
    socket = new WebSocket(`${env.PUBLIC_WEBSOCKET_URL}/ws`);
    socket.addEventListener("open", () => {
      status = "🟢";
      statusTip = "Connected";
      timeout = false;
      socket.send(
        JSON.stringify({
          eth_private_key: $eth_private_key,
          group_id: $group,
          should_create: $createNewRoom,
        })
      );
    });

    socket.addEventListener("close", () => {
      status = "🔴";
      statusTip = "Disconnected";
      if (timeout == false) {
        delay = 2000;
        timeout = true;
      }
    });

    socket.addEventListener("message", function (event) {
      if (event.data == "Username already taken.") {
        toast.error(event.data);
        goto("/");
      } else {
        // Try to parse as JSON to check for voting proposals
        try {
          const data = JSON.parse(event.data);
          if (data.type === "voting_proposal") {
            currentVotingProposal = data.proposal;
            showVotingUI = true;
            toast.success("New voting proposal received!");
          } else {
            messages = [...messages, event.data];
          }
        } catch (e) {
          // If not JSON, treat as regular message
          messages = [...messages, event.data];
        }
      }
    });
  }

  onMount(() => {
    if ($eth_private_key.length < 1 || $group.length < 1) {
      toast.error("Something went wrong!");
      goto("/");
    } else {
      connect();
    }
  });

  onDestroy(() => {
    if (socket) {
      socket.close();
    }
    if (interval) {
      clearInterval(interval);
    }
    timeout = false;
  });

  const sendMessage = () => {
    socket.send(
      JSON.stringify({
        message: message,
        group_id: $group,
      })
    );
    message = "";
  };

  const clear_messages = () => {
    messages = [];
  };

  const submitVote = (vote: boolean) => {
    if (currentVotingProposal) {
      socket.send(
        JSON.stringify({
          type: "user_vote",
          proposal_id: currentVotingProposal.proposal_id,
          vote: vote,
          group_id: $group,
        })
      );

      toast.success(`Vote submitted: ${vote ? "YES" : "NO"}`);
      showVotingUI = false;
      currentVotingProposal = null;
    }
  };

  const dismissVoting = () => {
    showVotingUI = false;
    currentVotingProposal = null;
  };
</script>

<div class="title flex justify-between">
  <h1 class="text-3xl font-bold cursor-default">
    Chat Room <span class="tooltip" data-tip={statusTip}>{status}</span>
  </h1>
  <button class="btn btn-accent" on:click={clear_messages}>clear</button>
</div>

<!-- Voting UI -->
{#if showVotingUI && currentVotingProposal}
  <div class="card bg-warning shadow-xl my-5">
    <div class="card-body">
      <h2 class="card-title text-warning-content">Voting Proposal</h2>
      <div class="text-warning-content">
        <p><strong>Group Name:</strong> {currentVotingProposal.group_name}</p>
        <p><strong>Proposal ID:</strong> {currentVotingProposal.proposal_id}</p>
        <p><strong>Proposal Payload:</strong></p>
        <div class="ml-4 whitespace-pre-line">
          {currentVotingProposal.payload}
        </div>
      </div>
      <div class="card-actions justify-end mt-4">
        <button class="btn btn-success" on:click={() => submitVote(true)}
          >Vote YES</button
        >
        <button class="btn btn-error" on:click={() => submitVote(false)}
          >Vote NO</button
        >
        <button class="btn btn-ghost" on:click={dismissVoting}>Dismiss</button>
      </div>
    </div>
  </div>
{/if}

<div class="card h-96 flex-grow bg-base-300 shadow-xl my-10">
  <div class="card-body">
    <div class="flex flex-col overflow-y-auto max-h-80 scroll-smooth">
      {#each messages as msg}
        <div class="my-2">{msg}</div>
      {/each}
    </div>
  </div>
</div>

<div class="message-box flex justify-end">
  <form on:submit|preventDefault={sendMessage}>
    <input
      placeholder="Message"
      class="input input-bordered input-primary w-[51rem] bg-base-200 mb-2"
      bind:value={message}
    />
    <button class="btn btn-primary w-full sm:w-auto btn-wide">Send</button>
  </form>
</div>
