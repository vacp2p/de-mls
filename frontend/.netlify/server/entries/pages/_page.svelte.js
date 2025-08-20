import { c as create_ssr_component, e as escape, i as each, g as add_attribute } from "../../chunks/index3.js";
import "../../chunks/Toaster.svelte_svelte_type_style_lang.js";
const Page = create_ssr_component(($$result, $$props, $$bindings, slots) => {
  let status, rooms;
  let { data } = $$props;
  let eth_pk = "";
  let room = "";
  let create_new_room = false;
  const filled_in = () => {
    return !(eth_pk.length > 0 && room.length > 0);
  };
  if ($$props.data === void 0 && $$bindings.data && data !== void 0)
    $$bindings.data(data);
  ({ status, rooms } = data);
  return `<div class="flex flex-col justify-center"><div class="title"><h1 class="text-3xl font-bold text-center">Chatr: a Websocket chatroom</h1></div>
    <div class="join self-center"></div>
    <div class="rooms self-center my-5"><div class="flex justify-between py-2"><h2 class="text-xl font-bold ">List of active chatroom&#39;s
            </h2>
            <button class="btn btn-square btn-sm btn-accent">↻</button></div>
        ${status && rooms.length < 1 ? `<div class="card bg-base-300 w-96 shadow-xl text-center"><div class="card-body"><h3 class="card-title ">${escape(status)}</h3></div></div>` : ``}
        ${rooms ? `${each(rooms, (room2) => {
    return `<button class="card bg-base-300 w-96 shadow-xl my-3 w-full"><div class="card-body"><div class="flex justify-between"><h2 class="card-title">${escape(room2)}</h2>
                            <button class="btn btn-primary btn-md">Select Room</button>
                        </div></div>
                </button>`;
  })}` : ``}</div>
    <div class="create self-center my-5 w-[40rem]"><div><label class="label" for="eth-private-key"><span class="label-text">Eth Private Key</span></label>
            <input id="eth-private-key" placeholder="Eth Private Key" class="input input-bordered input-primary w-full bg-base-200 mb-4 mr-3"${add_attribute("value", eth_pk, 0)}></div>
        <div><label class="label" for="room-name"><span class="label-text">Room name</span></label>
            <input id="room-name" placeholder="Room Name" class="input input-bordered input-primary w-full bg-base-200 mb-4 mr-3"${add_attribute("value", room, 0)}></div>
        <div class="form-control"><label class="label cursor-pointer"><span class="label-text">Create Room</span>
                <input type="checkbox" class="checkbox checkbox-primary"${add_attribute("checked", create_new_room, 1)}></label></div>
        <button class="btn btn-primary" ${filled_in() ? "disabled" : ""}>Join Room.</button></div>
    <div class="github self-center"><p>Check out <a class="link link-accent" href="https://github.com/0xLaurens/chatr" target="_blank" rel="noreferrer">Chatr</a>, to view the source code!
        </p></div></div>`;
});
export {
  Page as default
};
