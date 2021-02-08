const listener = Deno.listen({ port: 8080, transport: "tcp" });

for await (const conn of listener) {
  handleConnection(conn);
}

async function handleConnection(conn) {
  const httpConn = Deno.startHttp(conn);
  for await (const event of httpConn) {
    event.respondWith(handle(event.request));
    break;
  }
}

function handle(request) {
  return new Response("Hello World!");
}
