import { writable } from "svelte/store";

export const user = writable("");
export const channel = writable("");
export const eth_private_key = writable("");
export const group = writable("");
export const createNewRoom = writable(false);
