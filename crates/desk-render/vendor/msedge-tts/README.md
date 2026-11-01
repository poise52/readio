# msedge-tts (local copy)

This directory is derived from [msedge-tts 0.4.1](https://github.com/hs-CN/msedge-tts), licensed under MIT OR Apache-2.0. The upstream license is retained in `LICENSE`.

The local copy exposes the Tokio-based WebSocket stream API used by `readio-speech`. Blocking, smol, proxy, standalone client, and remote voice-list APIs have been removed. It retains upstream Sec-MS-GEC v2 token generation.

Local protocol changes:

- Treat WebSocket close or EOF before `turn.end` as an error, so an interrupted response cannot be returned as complete audio.
- Validate binary audio frame and header lengths before indexing.
- Test premature close and malformed audio headers.

`readio-speech` escapes text before passing it to the upstream SSML builder, limits response size, and handles retry, timeout, and cancellation outside this copy.
