import { p as public_env } from "../../chunks/shared-server.js";
const load = async ({ fetch }) => {
  try {
    let url = `${public_env.PUBLIC_API_URL}`;
    if (url.endsWith("/")) {
      url = url.slice(0, -1);
    }
    const res = await fetch(`${url}/rooms`);
    return await res.json();
  } catch (e) {
    return {
      status: "API offline (try again in a min)",
      rooms: []
    };
  }
};
export {
  load
};
