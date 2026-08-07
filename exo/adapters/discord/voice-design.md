# Discord Voice (Design)

Join a Discord voice channel and hold a spoken conversation with Exo: speak
to it, hear it reply. Code in `exo/adapters/discord/voice.ts`;
setup in that adapter's `README.md`.

## Core decision

Voice is a microphone and speaker on the existing text pipe — not a second
brain. A spoken turn becomes a normal inbound `message` event; a spoken reply is
a normal outbound `send_adapter_message`. So **all audio stays in the Discord
worker**: the Rust runtime, worker protocol, agent tools, and turn loop are
untouched, and voice reuses the agent's tools, identity, and history because by
the time Rust sees a turn it's identical to a typed one.

Flow: speak → worker captures Opus, segments on silence, OpenAI STT → `message`
event (`target` = voice channel id, `metadata.source = "voice"`) → normal agent
turn → `send_adapter_message` to that target → worker OpenAI TTS → plays into the
channel. Because the reply targets the voice channel id and the agent is told to
reply to the inbound target, audio routes back with no new tool or protocol
change. The reply text is also posted in the channel as a transcript.

## Tradeoffs

- **Pipeline (STT → agent → TTS), not realtime speech-to-speech.** Buys tool/
  identity/history reuse and zero substrate changes; costs latency — seconds per
  turn, more with tools. A realtime bridge would be lower-latency but a separate
  brain; out of scope, addable later as its own mode.
- **One knob, not a panel.** Only `voice` on/off and `openaiSecretId` are
  configurable; models (`gpt-4o-mini-transcribe`, `gpt-4o-mini-tts`/`alloy`) and
  the silence window are hardcoded.
- **Reuse the `openai` secret, no new key.** STT/TTS run against it, bound into
  the worker only when voice is on, so text-only adapters need no OpenAI key.
- **Streamed TTS playback.** The reply is piped into the player as the Ogg/Opus
  bytes arrive from OpenAI rather than buffering the whole clip first, so speech
  starts at roughly time-to-first-byte. (The dominant latency is still the agent
  turn itself, not TTS.)
- **Barge-in:** if you speak during playback, playback stops.

## Implementation notes

- **Restart-unique message ids.** A spoken utterance is emitted as a `message`
  whose id includes a timestamp (`voice-<channel>-<ms>-<seq>`). The adapter's
  inbound-dedup store is durable, but the worker's `seq` resets to 0 on restart;
  without the timestamp, the first utterance after a restart reuses an
  already-seen id and is silently dropped.
- **Empty/near-silent utterances are dropped.** Sub-`MIN_UTTERANCE_MS` captures
  are discarded, and OpenAI STT returns empty text for silence — in both cases
  no turn is woken (no spurious replies to coughs or background noise).

## Known limitation

Voice sessions live only in the worker's memory. A hard worker restart (e.g. a
guardian rebuild) orphans the Discord-side voice connection: the bot stays
visible in the channel and `/voice leave` on the new worker can't remove it,
because the new process has no record of the session. The orphan clears when the
bot's gateway drops (Discord times it out). A follow-up would drop stale voice
connections on worker startup, or leave gracefully before a planned restart.

## Control

- `/voice join` — join the caller's voice channel; `/voice leave` — disconnect;
  auto-leaves when the channel empties.
- Slash commands run entirely in the worker, never the model. They need the
  `applications.commands` scope and `Connect`/`Speak` permissions.

## Config and requirements

- `voice` (bool, default `false`) and `openaiSecretId` (default `"openai"` when
  voice is on), both on `create_adapter`.
- The `GuildVoiceStates` intent is added automatically when voice is on
  (non-privileged).
- An OpenAI secret: `exo secret set openai --env OPENAI_API_KEY`.

Both fields are in the `create_adapter` Discord schema's `required[]`: OpenAI
strict function-calling rejects a tool that declares a property without listing
it in `required`, which 400s every model turn. `openaiSecretId` is nullable so
the model passes `null` when voice is off.

## Not in scope (v1)

Realtime speech-to-speech, ambient/multi-source audio mixing, and wake-word
gating (while joined, every utterance is a turn).

## Dependencies

`@discordjs/voice`, `prism-media`, `opusscript` (pure-JS Opus, no native build),
`sodium-native` (prebuilt). No ffmpeg — inbound Opus is decoded to PCM and
wrapped as WAV in-process; TTS is fetched as OGG/Opus and played via
`StreamType.OggOpus`.
