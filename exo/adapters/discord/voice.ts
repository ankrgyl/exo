import { Buffer } from "node:buffer";
import { Readable } from "node:stream";

import {
  AudioPlayerStatus,
  createAudioPlayer,
  createAudioResource,
  EndBehaviorType,
  getVoiceConnection,
  joinVoiceChannel,
  StreamType,
  type AudioPlayer,
  type VoiceConnection,
} from "@discordjs/voice";
import {
  ChannelType,
  type Client,
  type Guild,
  type Interaction,
  type VoiceBasedChannel,
} from "discord.js";
import OpenAI, { toFile } from "openai";
import prism from "prism-media";

import { type WorkerInboundEvent } from "../protocol";

// `sodium-native` is imported only so @discordjs/voice detects it as the voice
// encryption backend at runtime; it ships a prebuilt binary and needs no setup.
import "sodium-native";

// Discord voice is 48kHz, 2-channel, signed 16-bit little-endian PCM.
const SAMPLE_RATE = 48_000;
const CHANNELS = 2;
const BYTES_PER_SAMPLE = 2;
const BYTES_PER_SECOND = SAMPLE_RATE * CHANNELS * BYTES_PER_SAMPLE;

// Defaults are hardcoded: voice is meant to be one knob (on/off), not a panel.
const SILENCE_END_MS = 1_000; // trailing silence that ends an utterance
const MIN_UTTERANCE_MS = 400; // drop coughs / sub-word blips
const STT_MODEL = "gpt-4o-mini-transcribe";
const TTS_MODEL = "gpt-4o-mini-tts";
const TTS_VOICE = "alloy";
const TTS_MAX_CHARS = 4_000;

type Emit = (event: WorkerInboundEvent) => void;

type VoiceSession = {
  guildId: string;
  channelId: string;
  connection: VoiceConnection;
  player: AudioPlayer;
  capturing: Set<string>;
};

const VOICE_COMMAND = {
  name: "voice",
  description: "Voice chat with the agent",
  options: [
    { type: 1, name: "join", description: "Join your current voice channel" },
    { type: 1, name: "leave", description: "Leave the voice channel" },
  ],
};

/**
 * Voice is a microphone and a speaker on the existing text-message pipe.
 * Inbound speech is transcribed and emitted as a normal `message` event whose
 * target is the voice channel id; an outbound `send_message` to that same
 * target is spoken back. Rust, the protocol, and the agent are unchanged.
 */
export class DiscordVoice {
  private readonly client: Client;
  private readonly openai: OpenAI;
  private readonly emit: Emit;
  private readonly sessions = new Map<string, VoiceSession>();
  private seq = 0;

  constructor(client: Client, openaiKey: string, emit: Emit) {
    this.client = client;
    this.openai = new OpenAI({ apiKey: openaiKey });
    this.emit = emit;
  }

  register(): void {
    this.client.on("interactionCreate", (interaction) => {
      void this.onInteraction(interaction);
    });
    this.client.on("voiceStateUpdate", () => {
      this.leaveEmptyChannels();
    });
    this.client.once("ready", () => {
      void this.registerCommands();
    });
  }

  /**
   * Speak `text` into the voice channel `target` if a session is active there.
   * Returns true if it was spoken. The caller still posts the text normally, so
   * voice turns are also mirrored as text in the channel.
   */
  async maybeSpeak(target: string | null, text: string): Promise<boolean> {
    if (target === null) {
      return false;
    }
    const session = this.sessions.get(target);
    if (!session) {
      return false;
    }
    const speech = await this.openai.audio.speech.create({
      model: TTS_MODEL,
      voice: TTS_VOICE,
      input: text.slice(0, TTS_MAX_CHARS),
      response_format: "opus",
    });
    // Stream the Ogg/Opus body straight into the player as it arrives instead
    // of buffering the whole clip first — playback starts on the first chunk,
    // cutting the lead-in to roughly time-to-first-byte.
    const body = speech.body;
    if (!body) {
      this.emit({ type: "error", message: "voice tts: empty response body" });
      return false;
    }
    const audioStream = Readable.fromWeb(
      body as Parameters<typeof Readable.fromWeb>[0],
    );
    const resource = createAudioResource(audioStream, {
      inputType: StreamType.OggOpus,
    });
    session.player.play(resource);
    return true;
  }

  private async registerCommands(): Promise<void> {
    for (const guild of this.client.guilds.cache.values()) {
      try {
        await guild.commands.set([VOICE_COMMAND]);
      } catch (error) {
        this.emit({
          type: "error",
          message: `failed to register /voice in guild ${guild.id}: ${errorMessage(error)}`,
        });
      }
    }
  }

  private async onInteraction(interaction: Interaction): Promise<void> {
    if (
      !interaction.isChatInputCommand() ||
      interaction.commandName !== "voice"
    ) {
      return;
    }
    const guild = interaction.guild;
    if (!guild) {
      await interaction.reply({
        content: "Voice only works in a server.",
        ephemeral: true,
      });
      return;
    }
    const subcommand = interaction.options.getSubcommand();
    if (subcommand === "leave") {
      this.leave(guild.id);
      await interaction.reply({
        content: "Left the voice channel.",
        ephemeral: true,
      });
      return;
    }
    const member =
      guild.members.cache.get(interaction.user.id) ?? interaction.member;
    const channel =
      member && "voice" in member
        ? (member.voice.channel as VoiceBasedChannel | null)
        : null;
    if (!channel) {
      await interaction.reply({
        content: "Join a voice channel first.",
        ephemeral: true,
      });
      return;
    }
    this.join(guild, channel);
    await interaction.reply({
      content: `Joined ${channel.name}. Talk to me.`,
      ephemeral: true,
    });
  }

  private join(guild: Guild, channel: VoiceBasedChannel): void {
    const existing = this.sessions.get(channel.id);
    if (existing) {
      return;
    }
    const connection = joinVoiceChannel({
      channelId: channel.id,
      guildId: guild.id,
      adapterCreator: guild.voiceAdapterCreator,
      selfDeaf: false,
      selfMute: false,
    });
    const player = createAudioPlayer();
    connection.subscribe(player);
    const session: VoiceSession = {
      guildId: guild.id,
      channelId: channel.id,
      connection,
      player,
      capturing: new Set(),
    };
    this.sessions.set(channel.id, session);
    this.setupReceiver(session);
    this.emit({
      type: "lifecycle",
      name: "voice_joined",
      metadata: { channelId: channel.id, guildId: guild.id },
    });
  }

  private leave(guildId: string): void {
    for (const [channelId, session] of this.sessions) {
      if (session.guildId !== guildId) {
        continue;
      }
      session.player.stop(true);
      this.sessions.delete(channelId);
      this.emit({
        type: "lifecycle",
        name: "voice_left",
        metadata: { channelId, guildId },
      });
    }
    getVoiceConnection(guildId)?.destroy();
  }

  private leaveEmptyChannels(): void {
    for (const session of this.sessions.values()) {
      const channel = this.client.channels.cache.get(session.channelId);
      if (!channel || !channel.isVoiceBased()) {
        continue;
      }
      const humans = channel.members.filter((m) => !m.user.bot);
      if (humans.size === 0) {
        this.leave(session.guildId);
      }
    }
  }

  private setupReceiver(session: VoiceSession): void {
    const receiver = session.connection.receiver;
    receiver.speaking.on("start", (userId) => {
      // Barge-in: a speaking user interrupts the agent's current playback.
      if (session.player.state.status === AudioPlayerStatus.Playing) {
        session.player.stop(true);
      }
      if (userId === this.client.user?.id || session.capturing.has(userId)) {
        return;
      }
      if (this.client.users.cache.get(userId)?.bot) {
        return;
      }
      this.captureUtterance(session, userId);
    });
  }

  private captureUtterance(session: VoiceSession, userId: string): void {
    session.capturing.add(userId);
    const opusStream = session.connection.receiver.subscribe(userId, {
      end: { behavior: EndBehaviorType.AfterSilence, duration: SILENCE_END_MS },
    });
    const decoder = new prism.opus.Decoder({
      rate: SAMPLE_RATE,
      channels: CHANNELS,
      frameSize: 960,
    });
    const chunks: Buffer[] = [];
    const pcm = opusStream.pipe(decoder);
    pcm.on("data", (chunk: Buffer) => chunks.push(chunk));
    pcm.on("error", (error: Error) => {
      session.capturing.delete(userId);
      this.emit({ type: "error", message: `voice receive: ${error.message}` });
    });
    pcm.on("end", () => {
      session.capturing.delete(userId);
      void this.finalizeUtterance(session, userId, Buffer.concat(chunks));
    });
  }

  private async finalizeUtterance(
    session: VoiceSession,
    userId: string,
    pcm: Buffer,
  ): Promise<void> {
    // Drop sub-word blips (coughs, key clicks) below the minimum utterance.
    if (pcm.length < (BYTES_PER_SECOND * MIN_UTTERANCE_MS) / 1000) {
      return;
    }
    let text: string;
    try {
      const wav = pcmToWav(pcm);
      const file = await toFile(wav, "utterance.wav", { type: "audio/wav" });
      const transcription = await this.openai.audio.transcriptions.create({
        file,
        model: STT_MODEL,
      });
      text = transcription.text.trim();
    } catch (error) {
      this.emit({
        type: "error",
        message: `transcription failed: ${errorMessage(error)}`,
      });
      return;
    }
    // STT returns empty for silence/near-silence; nothing to wake a turn with.
    if (text.length === 0) {
      return;
    }
    const username = this.client.users.cache.get(userId)?.tag ?? null;
    this.emit({
      type: "message",
      target: session.channelId,
      sender: userId,
      text,
      // Include a timestamp so ids stay unique across worker restarts: the
      // inbound-dedup store persists, but `seq` resets to 0 on each restart, so
      // a bare `voice-<channel>-<seq>` collides with an already-seen id and the
      // utterance is silently dropped.
      message_id: `voice-${session.channelId}-${Date.now()}-${this.seq++}`,
      metadata: {
        source: "voice",
        guildId: session.guildId,
        channelId: session.channelId,
        channelType: ChannelType.GuildVoice,
        authorUsername: username,
      },
    });
  }
}

/** Wrap raw s16le PCM in a minimal 44-byte WAV header for the STT API. */
function pcmToWav(pcm: Buffer): Buffer {
  const header = Buffer.alloc(44);
  const byteRate = SAMPLE_RATE * CHANNELS * BYTES_PER_SAMPLE;
  const blockAlign = CHANNELS * BYTES_PER_SAMPLE;
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + pcm.length, 4);
  header.write("WAVE", 8);
  header.write("fmt ", 12);
  header.writeUInt32LE(16, 16); // PCM fmt chunk size
  header.writeUInt16LE(1, 20); // audio format = PCM
  header.writeUInt16LE(CHANNELS, 22);
  header.writeUInt32LE(SAMPLE_RATE, 24);
  header.writeUInt32LE(byteRate, 28);
  header.writeUInt16LE(blockAlign, 32);
  header.writeUInt16LE(BYTES_PER_SAMPLE * 8, 34);
  header.write("data", 36);
  header.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([header, pcm]);
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
