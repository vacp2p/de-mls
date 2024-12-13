<script lang="ts">
    import {onMount, onDestroy} from "svelte";
    import {user, channel, eth_private_key, group, createNewRoom} from "../../lib/stores/user";
    import {goto} from '$app/navigation';
    import {env} from '$env/dynamic/public'
    import toast from "svelte-french-toast";
    import { json } from "@sveltejs/kit";

    let status = "🔴";
    let statusTip = "Disconnected";
    let message = "";
    let messages = [];
    let socket: WebSocket;
    let interval: number;
    let delay = 2000;
    let timeout = false;
    $: {
        if (interval || (!timeout && interval)) {
            clearInterval(interval);
        }

        if (timeout == true) {
            interval = setInterval(() => {
                if (delay < 30_000) delay = delay * 2;
                console.log("reconnecting in:", delay)
                connect();
            }, delay)
        }
    }

    function connect() {
        socket = new WebSocket(`${env.PUBLIC_WEBSOCKET_URL}/ws`)
        socket.addEventListener("open", () => {
            status = "🟢"
            statusTip = "Connected";
            timeout = false;
            socket.send(JSON.stringify({
                eth_private_key: $eth_private_key,
                group_id: $group,
                should_create: $createNewRoom,
            }));
        })

        socket.addEventListener("close", () => {
            status = "🔴";
            statusTip = "Disconnected";
            if (timeout == false) {
                delay = 2000;
                timeout = true;
            }
        })

        socket.addEventListener('message', function (event) {
            if (event.data == "Username already taken.") {
                toast.error(event.data)
                goto("/");
            } else {
                messages = [...messages, event.data]
            }
        })
    }

    onMount(() => {
            if ($eth_private_key.length < 1 || $group.length < 1 ) {
                toast.error("Something went wrong!")
                goto("/");
            } else {
                connect()
            }
        }
    )

    onDestroy(() => {
        if (socket) {
            socket.close()
        }
        if (interval) {
            clearInterval(interval)
        }
        timeout = false
    })

    const sendMessage = () => {
        socket.send(message)
        message = "";
    };
    const clear_messages = () => {
        messages = [];
    };


</script>
<div class="title flex justify-between">
    <h1 class="text-3xl font-bold cursor-default">Chat Room <span class="tooltip" data-tip="{statusTip}">{status}</span>
    </h1>
    <button class="btn btn-accent" on:click={clear_messages}>clear</button>
</div>
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
        <input placeholder="Message" class="input input-bordered input-primary w-full sm:w-auto bg-base-200 mb-2"
               bind:value={message}>
        <button class="btn btn-primary w-full sm:w-auto btn-wide">Send</button>
    </form>
</div>
