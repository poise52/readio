import { PlaybackGate } from "./audio-cache.ts";
import {
  playAudioBlock,
  type BlockAudio,
  type BlockPlaybackOptions,
} from "./block-playback.ts";

export class PlaybackController<Request, Result, TPlayer extends BlockAudio> {
  private gate = new PlaybackGate();
  private epoch = 0;
  private readonly fetch: (request: Request) => Promise<Result>;
  private readonly discard: (request: Request, result: Result) => void;
  private readonly createAudio: (source: string) => TPlayer;
  private readonly now: () => number;
  constructor(dependencies: {
    fetch: (request: Request) => Promise<Result>;
    discard: (request: Request, result: Result) => void;
    createAudio: (source: string) => TPlayer;
    now: () => number;
  }) {
    this.fetch = dependencies.fetch;
    this.discard = dependencies.discard;
    this.createAudio = dependencies.createAudio;
    this.now = dependencies.now;
  }
  get token(): number {
    return this.epoch;
  }
  pause(): void {
    this.gate.pause();
  }
  resume(): void {
    this.gate.resume();
  }
  cancel(): void {
    this.epoch++;
    this.gate.cancel();
  }
  wait(token: number): Promise<boolean> {
    return this.gate.wait(() => token === this.epoch);
  }
  async load(request: Request, token: number): Promise<Result> {
    const result = await this.fetch(request);
    if (!(await this.wait(token))) {
      this.discard(request, result);
      throw { code: "cancelled", message: "Воспроизведение отменено" };
    }
    return result;
  }
  block(
    source: string,
    options: Omit<BlockPlaybackOptions<TPlayer>, "createAudio" | "now">,
  ) {
    return playAudioBlock({
      ...options,
      createAudio: () => this.createAudio(source),
      now: this.now,
    });
  }
}
