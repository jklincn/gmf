export default {
  async fetch(request, env, ctx) {
    return new Response("Hello, World from Cloudflare Worker!\n", {
      status: 200,
      headers: { "Content-Type": "text/plain" },
    });
  },
};
