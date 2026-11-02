export interface PlaybackRequest {
  session: string;
  revision: number;
  confirmedAttempt?: string;
}

export class PlaybackInterest {
  private revision = 0;

  private readonly session: string;
  private readonly send: (
    session: string,
    revision: number,
    blockIds: string[],
  ) => Promise<boolean>;

  constructor(
    session: string,
    send: (
      session: string,
      revision: number,
      blockIds: string[],
    ) => Promise<boolean>,
  ) {
    this.session = session;
    this.send = send;
  }

  async update(blockIds: string[]): Promise<PlaybackRequest | null> {
    const revision = ++this.revision;
    const accepted = await this.send(this.session, revision, blockIds);
    if (revision !== this.revision) return null;
    if (!accepted) throw new Error("Сессия воспроизведения больше не активна");
    return { session: this.session, revision };
  }
}
