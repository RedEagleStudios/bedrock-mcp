# Security

Report a security problem privately: on the repository's **Security** tab, choose **Report a
vulnerability**. Please do not open a public issue for it.

The server, the DLLs it loads into the game, the injector and the installers are all in scope.
The parts most worth a look:

- **The local endpoint** between the server and the DLLs listens on `127.0.0.1` only. Each DLL
  writes a random token next to itself in the agent folder, and a client must send it first.
- **The WebSocket server** the game connects to listens on `127.0.0.1` only.
- **The installers** check every download against the release's `SHA256SUMS`.
- **The DLLs** run inside the game and read its memory; a bug there can crash the game.

Fixes go into the next release. Only the latest release gets fixes.
