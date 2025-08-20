import { c as create_ssr_component, h as subscribe, o as onDestroy, g as add_attribute, e as escape, i as each } from "../../../chunks/index3.js";
import { w as writable } from "../../../chunks/index2.js";
import { p as public_env } from "../../../chunks/shared-server.js";
import { t as toast } from "../../../chunks/Toaster.svelte_svelte_type_style_lang.js";
import "../../../chunks/index.js";
const eth_private_key = writable("");
const group = writable("");
const createNewRoom = writable(false);
function guard(name) {
  return () => {
    throw new Error(`Cannot call ${name}(...) on the server`);
  };
}
const goto = guard("goto");
const Page = create_ssr_component(($$result, $$props, $$bindings, slots) => {
  let $group, $$unsubscribe_group;
  let $eth_private_key, $$unsubscribe_eth_private_key;
  let $createNewRoom, $$unsubscribe_createNewRoom;
  $$unsubscribe_group = subscribe(group, (value) => $group = value);
  $$unsubscribe_eth_private_key = subscribe(eth_private_key, (value) => $eth_private_key = value);
  $$unsubscribe_createNewRoom = subscribe(createNewRoom, (value) => $createNewRoom = value);
  let status = "🔴";
  let statusTip = "Disconnected";
  let message = "";
  let messages = [];
  let socket;
  let interval;
  let delay = 2e3;
  let timeout = false;
  let currentVotingProposal = null;
  let showVotingUI = false;
  function connect() {
    socket = new WebSocket(`${public_env.PUBLIC_WEBSOCKET_URL}/ws`);
    socket.addEventListener("open", () => {
      status = "🟢";
      statusTip = "Connected";
      timeout = false;
      socket.send(JSON.stringify({
        eth_private_key: $eth_private_key,
        group_id: $group,
        should_create: $createNewRoom
      }));
    });
    socket.addEventListener("close", () => {
      status = "🔴";
      statusTip = "Disconnected";
      if (timeout == false) {
        delay = 2e3;
        timeout = true;
      }
    });
    socket.addEventListener("message", function(event) {
      if (event.data == "Username already taken.") {
        toast.error(event.data);
        goto("/");
      } else {
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
          messages = [...messages, event.data];
        }
      }
    });
  }
  onDestroy(() => {
    if (socket) {
      socket.close();
    }
    if (interval) {
      clearInterval(interval);
    }
    timeout = false;
  });
  {
    {
      if (interval || !timeout && interval) {
        clearInterval(interval);
      }
      if (timeout == true) {
        interval = setInterval(
          () => {
            if (delay < 3e4)
              delay = delay * 2;
            console.log("reconnecting in:", delay);
            connect();
          },
          delay
        );
      }
    }
  }
  $$unsubscribe_group();
  $$unsubscribe_eth_private_key();
  $$unsubscribe_createNewRoom();
  return `<div class="container mx-auto p-4 max-w-4xl"><div class="flex justify-between items-center mb-8"><h1 class="text-3xl font-bold">MLS Chat <span class="tooltip"${add_attribute("data-tip", statusTip, 0)}>${escape(status)}</span></h1>
        <button class="btn btn-accent">Clear Messages</button></div>
    
    
    <div class="text-center mb-4"><button class="btn btn-warning btn-outline">🧪 Test Voting Proposal
        </button></div>

    
    ${showVotingUI && currentVotingProposal ? `<div class="card bg-warning shadow-xl my-5"><div class="card-body"><h2 class="card-title text-warning-content">Voting Proposal</h2>
                <div class="text-warning-content"><p><strong>Group Name:</strong> ${escape(currentVotingProposal.group_name)}</p>
                    <p><strong>Proposal ID:</strong> ${escape(currentVotingProposal.proposal_id)}</p>
                    <p><strong>Proposal Payload:</strong></p>
                    <div class="ml-4 whitespace-pre-line">${escape(currentVotingProposal.payload)}</div></div>
                <div class="card-actions justify-end mt-4"><button class="btn btn-success">Vote YES</button>
                    <button class="btn btn-error">Vote NO</button>
                    <button class="btn btn-ghost">Dismiss</button></div></div></div>` : ``}

    <div class="card h-96 flex-grow bg-base-300 shadow-xl my-10"><div class="card-body"><div class="flex flex-col overflow-y-auto max-h-80 scroll-smooth">${each(messages, (msg) => {
    return `<div class="my-2">${escape(msg)}</div>`;
  })}</div></div></div>

    <div class="message-box flex justify-end"><form><input placeholder="Message" class="input input-bordered input-primary w-[51rem] bg-base-200 mb-2"${add_attribute("value", message, 0)}>
            <button class="btn btn-primary w-full sm:w-auto btn-wide">Send</button></form></div></div>`;
});
export {
  Page as default
};
