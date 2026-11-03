<script setup lang="ts">
import { invoke, isTauri, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import RangeInput from "./components/RangeInput.vue";
import { useVirtualizer } from "@tanstack/vue-virtual";
import type { ComponentPublicInstance } from "vue";
import { speechChunks } from "./utils/speech";
import {
  billingUnitDirty,
  modelSettingsWithLimit,
  modelSettingsWithRevision,
  normalizePreferences,
  preferencesSnapshot,
  withCurrentBillingUnits,
} from "./utils/preferences";
import { PreviewOwner } from "./utils/preview-owner";
import {
  shouldRefreshPausedQuote,
  isStaleQuoteError,
} from "./utils/offline-refresh";
import {
  filterModels,
  selectionForModel,
  type ModelFilter,
} from "./utils/openrouter";
import {
  configurePlaybackRate,
  playbackRate,
  replayIndex,
  synthesizeThenPlay,
} from "./utils/playback";
import {
  AudioBlockLoader,
  audioErrorCode,
  audioErrorMessage,
  blockForOffset,
  blockOffsetFraction,
  blockPosition,
} from "./utils/audio-cache";
import { PlaybackInterest } from "./utils/playback-interest";
import { PlaybackController } from "./utils/playback-controller";
import { startingBlockSeconds } from "./utils/audio-position";
import { timedSeek, type TimedPart } from "./utils/audio-timing";
import {
  LibraryBig,
  BookOpen,
  Bookmark,
  BookmarkPlus,
  Settings,
  Plus,
  Trash2,
  List,
  SkipBack,
  SkipForward,
  Play,
  Pause,
  Square,
  Volume2,
  Download,
  CloudOff,
  Image as ImageIcon,
  X,
} from "lucide-vue-next";
import type {
  Book,
  BookSummary,
  Bootstrap,
  Mark,
  Preferences,
  SpeechAudio,
  OpenRouterStatus,
  OpenRouterModel,
  Voice,
  AudioBlock,
  AudioPlan,
  OfflineStatus,
  OfflineQuote,
  OfflineDiagnostic,
} from "./types/models";

type Page = "library" | "reader" | "marks" | "settings";
const defaults: Preferences = {
  dark: true,
  fontSize: 19,
  lineHeight: 1.65,
  primaryColor: 0xff69d9b1,
  backgroundImagePath: null,
  backgroundOpacity: 0.24,
  backgroundDim: 0.2,
  backgroundBlurEnabled: false,
  backgroundBlur: 0.4,
  surfaceTransparency: 0.18,
  voice: null,
  openRouterModel: null,
  openRouterBillingUnits: {},
  openRouterModelSettings: {},
  openRouterVoice: null,
  volume: 1,
  dictionary: {},
};
const page = ref<Page>("library");
const books = ref<BookSummary[]>([]);
const book = ref<Book | null>(null);
const prefs = ref<Preferences>(structuredClone(defaults));
const voices = ref<Voice[]>([]);
const marks = ref<{ book: BookSummary; mark: Mark; chapterTitle: string }[]>(
  [],
);
const active = ref(0);
const playing = ref(false);
const paused = ref(false);
const offlineStatuses = ref<Record<string, OfflineStatus>>({});
const offlineDiagnostics = ref<OfflineDiagnostic[]>([]);
const offlineSource = ref<Record<string, boolean>>({});
const offlineDialog = ref(false);
const offlineDialogElement = ref<HTMLElement | null>(null);
const offlineCloseButton = ref<HTMLButtonElement | null>(null);
let offlinePreviousFocus: HTMLElement | null = null;
const offlineQuote = ref<OfflineQuote | null>(null);
const offlineRetryConfirmed = ref(false);
const maxNewBlocks = ref<number | null>(null);
const maxCostUsd = ref("");
const validMaxCost = computed(
  () =>
    !maxCostUsd.value.trim() || /^\d+(?:\.\d+)?$/.test(maxCostUsd.value.trim()),
);
const allowUnknownCost = ref(false);
const offlineBusy = ref(false);
const offlineError = ref("");
const offlineInfo = ref("");
const billingUnitDraft = ref<"" | "character" | "utf8_byte">("");
const offlineBookId = ref("");
const billingDirty = computed(() =>
  billingUnitDirty(
    offlineQuote.value?.model ?? null,
    billingUnitDraft.value,
    prefs.value.openRouterBillingUnits,
  ),
);
const completed = ref(false);
const loading = ref(true);
const notice = ref("");
const chapterMenu = ref(false);
const dictionaryWord = ref("");
const dictionaryValue = ref("");
const reader = ref<HTMLElement | null>(null);
const native = ref(false);
const closing = ref(false);
const shutdownError = ref("");
const shutdownWarning = ref("");
const openRouter = ref<OpenRouterStatus>({ hasApiKey: false });
const apiKeyDraft = ref("");
const apiKeyBusy = ref(false);
const models = ref<OpenRouterModel[]>([]);
const modelsLoading = ref(false);
const modelSaving = ref(false);
const modelsLoaded = ref(false);
const modelsError = ref("");
const modelQuery = ref("");
const modelFilter = ref<ModelFilter>("all");
const filteredModels = computed(() =>
  filterModels(models.value, modelQuery.value, modelFilter.value),
);
const selectedModel = computed(() =>
  models.value.find((model) => model.id === prefs.value.openRouterModel),
);
const selectedInFilter = computed(() =>
  filteredModels.value.some(
    (model) => model.id === prefs.value.openRouterModel,
  ),
);
let loopRunning = false;
let currentAudioBlock: AudioBlock | null = null;
let currentAudioProfile: string | null = null;
let currentAudioReady = false;
let currentTimedParts: TimedPart[] | null = null;
const accurateTransitions = ref(false);
let playbackInterest: PlaybackInterest | null = null;
const retryPrompt = ref<{ message: string; attemptId: string } | null>(null);
const retryDialog = ref<HTMLElement | null>(null);
const retryCancelButton = ref<HTMLButtonElement | null>(null);
let retryPreviousFocus: HTMLElement | null = null;
let retryResolve: ((confirmed: boolean) => void) | null = null;
function resolveRetry(confirmed: boolean) {
  retryPrompt.value = null;
  const resolve = retryResolve;
  retryResolve = null;
  if (resolve) retryPreviousFocus?.focus();
  retryPreviousFocus = null;
  resolve?.(confirmed);
}
function trapRetryFocus(event: KeyboardEvent) {
  const buttons = retryDialog.value?.querySelectorAll<HTMLButtonElement>(
    "button:not(:disabled)",
  );
  if (!buttons?.length) return;
  const first = buttons[0]!;
  const last = buttons[buttons.length - 1]!;
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}
function confirmRetry(error: unknown): Promise<boolean> {
  if (
    typeof error !== "object" ||
    error === null ||
    !("attemptId" in error) ||
    typeof error.attemptId !== "string"
  )
    return Promise.resolve(false);
  retryPreviousFocus =
    document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
  retryPrompt.value = {
    message: audioErrorMessage(error),
    attemptId: error.attemptId,
  };
  void nextTick(() => retryCancelButton.value?.focus());
  return new Promise((resolve) => {
    retryResolve = resolve;
  });
}
function withdrawPlaybackInterest() {
  if (playbackInterest) void playbackInterest.update([]).catch(report);
}
function releaseAudio(audio: SpeechAudio) {
  if (audio.resourceId)
    void invoke("audio_resource_release", { token: audio.resourceId }).catch(
      report,
    );
}
const audioLoader = new AudioBlockLoader(
  async (key, retry, context) => {
    if (!context) throw new Error("Нет активной сессии воспроизведения");
    const { bookId, chapter, blockId, offline } = JSON.parse(key) as {
      bookId: string;
      chapter: number;
      blockId: string;
      offline: boolean;
    };
    return invoke<SpeechAudio>("audio_block", {
      request: {
        bookId,
        chapter,
        blockId,
        offline,
        speed: 1,
        retry,
        ...context,
      },
    });
  },
  3,
  releaseAudio,
);
const playback = new PlaybackController({
  fetch: ({
    key,
    retry,
    context,
  }: {
    key: string;
    retry: boolean;
    context: Parameters<AudioBlockLoader["get"]>[2];
  }) => audioLoader.get(key, retry, context),
  discard: ({ key }, result) => {
    audioLoader.takeResource(key, result);
    releaseAudio(result);
  },
  createAudio: (source: string) => new Audio(source),
  now: Date.now,
});
function audioKey(
  bookId: string,
  chapter: number,
  blockId: string,
  profileId: string,
  offline: boolean,
) {
  return JSON.stringify({ bookId, chapter, blockId, profileId, offline });
}
function waitForPlayback(token: number) {
  return playback.wait(token);
}
let audio: HTMLAudioElement | null = null;
let audioAttempt = 0;
let currentPrefetchKey: string | undefined;
let currentPrefetchBlockId: string | undefined;
let audioBaseRate = 1;
let audioSynthesizedSpeed = 1;
let audioCancel: (() => void) | null = null;
let positionTimer: ReturnType<typeof setTimeout> | undefined;
let preferencesTimer: ReturnType<typeof setTimeout> | undefined;
let positionQueue = Promise.resolve();
let preferenceQueue = Promise.resolve();
const preview = new PreviewOwner(async () => {
  if (native.value) await invoke("stop_speech");
}, report);
let followingUntil = 0;
let userScrollUntil = 0;
let openGeneration = 0;
let pendingModelRefresh = false;
let modelCatalogEpoch = 0;
const virtualizer = useVirtualizer(
  computed(() => ({
    count: (book.value?.passages.length ?? 0) + 1,
    getScrollElement: () => reader.value,
    estimateSize: (index: number) =>
      index === 0 ? 80 : prefs.value.fontSize * prefs.value.lineHeight + 10,
    getItemKey: (index: number) =>
      `${book.value?.id ?? ""}-${book.value?.chapter ?? 0}-${index}`,
    overscan: 8,
    paddingEnd: 80,
  })),
);
function measureRow(element: Element | ComponentPublicInstance | null) {
  if (element instanceof HTMLElement) virtualizer.value.measureElement(element);
}
watch(
  () => [
    prefs.value.fontSize,
    prefs.value.lineHeight,
    book.value?.id,
    book.value?.chapter,
  ],
  async () => {
    await nextTick();
    virtualizer.value.measure();
  },
);

const accent = computed(
  () =>
    `#${(prefs.value.primaryColor & 0xffffff).toString(16).padStart(6, "0")}`,
);
function luminance(rgb: number[]): number {
  const linear = (component: number) => {
    const value = component / 255;
    return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  };
  return (
    0.2126 * linear(rgb[0]!) +
    0.7152 * linear(rgb[1]!) +
    0.0722 * linear(rgb[2]!)
  );
}
const accentRgb = computed(() => {
  const rgb = prefs.value.primaryColor;
  return [(rgb >>> 16) & 255, (rgb >>> 8) & 255, rgb & 255];
});
const accentForeground = computed(() =>
  luminance(accentRgb.value) > 0.179 ? "#000000" : "#ffffff",
);
const accentInk = computed(() => {
  const rgb = accentRgb.value;
  const target = prefs.value.dark ? 255 : 0;
  const sufficient = (value: number) =>
    prefs.value.dark ? value >= 0.4 : value <= 0.1;
  for (let step = 0; step <= 100; step++) {
    const mixed = rgb.map((channel) =>
      Math.round(channel + ((target - channel) * step) / 100),
    );
    if (sufficient(luminance(mixed)))
      return `#${mixed.map((channel) => channel.toString(16).padStart(2, "0")).join("")}`;
  }
  return prefs.value.dark ? "#ffffff" : "#000000";
});
const background = computed(() =>
  prefs.value.backgroundImagePath && native.value
    ? convertFileSrc(prefs.value.backgroundImagePath)
    : "",
);
const themeStyle = computed(() => ({
  "--accent": accent.value,
  "--accent-fg": accentForeground.value,
  "--accent-ink": accentInk.value,
  "--panel-alpha": background.value ? 1 - prefs.value.surfaceTransparency : 1,
  "--reader-size": `${prefs.value.fontSize}px`,
  "--reader-line": prefs.value.lineHeight,
}));
function report(e: unknown) {
  notice.value = audioErrorMessage(e);
}
function updateSummary(summary: BookSummary) {
  const i = books.value.findIndex((b) => b.id === summary.id);
  if (i >= 0) books.value[i] = summary;
  if (book.value?.id === summary.id) book.value.progress = summary.progress;
}
function savePositionNow() {
  clearTimeout(positionTimer);
  const b = book.value;
  if (!b || !native.value) return positionQueue;
  const snapshot = {
    id: b.id,
    chapter: b.chapter,
    offset: b.offset,
    speed: b.speed,
    audioPosition: b.audioPosition,
  };
  positionQueue = positionQueue
    .then(async () => {
      const summary = await invoke<BookSummary>("save_position", snapshot);
      updateSummary(summary);
    })
    .catch(report);
  return positionQueue;
}
function savePosition() {
  clearTimeout(positionTimer);
  positionTimer = setTimeout(() => {
    void savePositionNow();
  }, 350);
}
function savePrefsNow() {
  clearTimeout(preferencesTimer);
  if (!native.value) return preferenceQueue;
  const snapshot = preferencesSnapshot(prefs.value);
  const request = preferenceQueue.then(async () => {
    await invoke("save_preferences", {
      preferences: withCurrentBillingUnits(snapshot, prefs.value),
    });
  });
  preferenceQueue = request.catch(report);
  return request;
}
function savePrefs() {
  clearTimeout(preferencesTimer);
  preferencesTimer = setTimeout(() => {
    void savePrefsNow().catch(() => {});
  }, 250);
}
function captureAudioPosition() {
  if (
    book.value &&
    audio &&
    currentAudioBlock &&
    currentAudioProfile &&
    currentAudioReady &&
    Number.isFinite(audio.currentTime)
  )
    book.value.audioPosition = {
      chapter: book.value.chapter,
      blockId: currentAudioBlock.id,
      profileId: currentAudioProfile,
      seconds: audio.currentTime,
    };
}
function stop() {
  captureAudioPosition();
  preview.cancel();
  playback.cancel();
  audioAttempt++;
  resolveRetry(false);
  withdrawPlaybackInterest();
  currentPrefetchKey = undefined;
  currentPrefetchBlockId = undefined;
  playing.value = false;
  paused.value = false;
  const player = audio;
  audioCancel?.();
  audioCancel = null;
  if (player) {
    player.pause();
    player.src = "";
    if (audio === player) audio = null;
  }
  currentAudioBlock = null;
  currentAudioProfile = null;
  currentAudioReady = false;
  currentTimedParts = null;
  accurateTransitions.value = false;
  void savePositionNow();
}
function tokenIsActive(player: HTMLAudioElement) {
  return audio === player;
}
function pausePlayback() {
  if (!playing.value) return;
  playing.value = false;
  paused.value = true;
  audioAttempt++;
  playback.pause();
  if (retryPrompt.value) resolveRetry(false);
  withdrawPlaybackInterest();
  audio?.pause();
  captureAudioPosition();
  void savePositionNow();
}
function indexAt(offset: number) {
  const ps = book.value?.passages ?? [];
  let low = 0,
    high = ps.length - 1;
  while (low <= high) {
    const mid = (low + high) >>> 1;
    if (ps[mid]!.start <= offset) low = mid + 1;
    else high = mid - 1;
  }
  return Math.max(0, high);
}
async function jump(index = active.value) {
  await nextTick();
  followingUntil = Date.now() + 700;
  virtualizer.value.scrollToIndex(index + 1, { align: "start" });
}
async function openBook(id: string, chapter?: number, offset?: number) {
  const token = ++openGeneration;
  stop();
  await savePositionNow();
  try {
    let loaded = await invoke<Book>("open_book", { id });
    if (token !== openGeneration) return;
    if (chapter !== undefined || offset !== undefined) {
      const snapshot = {
        id,
        chapter: chapter ?? loaded.chapter,
        offset: offset ?? loaded.offset,
        speed: loaded.speed,
        audioPosition: null,
      };
      positionQueue = positionQueue
        .then(async () => {
          updateSummary(await invoke<BookSummary>("save_position", snapshot));
        })
        .catch(report);
      await positionQueue;
      if (token !== openGeneration) return;
      loaded = await invoke<Book>("open_book", { id });
      if (token !== openGeneration) return;
    }
    book.value = loaded;
    active.value = indexAt(loaded.offset);
    completed.value = false;
    page.value = "reader";
    chapterMenu.value = false;
    await jump();
  } catch (e) {
    report(e);
  }
}
async function changeChapter(chapter: number) {
  if (!book.value) return;
  const id = book.value.id;
  await openBook(id, chapter, 0);
}
function selectPassage(index: number, resume = false) {
  const b = book.value;
  const passage = b?.passages[index];
  if (!b || !passage) return;
  const shouldResume = resume || playing.value;
  if (
    currentAudioBlock &&
    audio &&
    currentAudioReady &&
    currentAudioBlock.start <= passage.start &&
    passage.start < currentAudioBlock.end &&
    Number.isFinite(audio.duration)
  ) {
    audio.currentTime =
      (currentTimedParts && timedSeek(currentTimedParts, passage.start)) ??
      blockOffsetFraction(currentAudioBlock, passage.start) * audio.duration;
    active.value = index;
    b.offset = passage.start;
    captureAudioPosition();
    completed.value = false;
    savePosition();
    void jump(index);
    return;
  }
  stop();
  active.value = index;
  b.offset = passage.start;
  b.audioPosition = null;
  completed.value = false;
  savePosition();
  void jump(index);
  if (shouldResume) void play();
}
function readerIntent() {
  userScrollUntil = Date.now() + 1200;
}
function onReaderScroll() {
  if (
    playing.value ||
    Date.now() < followingUntil ||
    Date.now() > userScrollUntil ||
    !book.value ||
    !reader.value
  )
    return;
  const row = virtualizer.value.getVirtualItemForOffset(
    reader.value.scrollTop + 8,
  );
  if (!row) return;
  const index = Math.max(0, row.index - 1);
  const passage = book.value.passages[index];
  if (passage && index !== active.value) {
    if (paused.value) {
      selectPassage(index);
      return;
    }
    active.value = index;
    book.value.offset = passage.start;
    completed.value = false;
    savePosition();
  }
}
async function speakChunk(text: string, speed: number, token: number) {
  await preview.wait();
  if (token !== playback.token) return;
  await synthesizeThenPlay(
    () =>
      invoke<SpeechAudio>("synthesize", {
        text,
        voice: prefs.value.voice,
        speed,
      }),
    (result) =>
      new Promise<void>((resolve, reject) => {
        const player = new Audio(
          result.resourceId
            ? convertFileSrc(result.resourceId, "readio-audio")
            : `data:${result.mime};base64,${result.data}`,
        );
        audioBaseRate = result.playbackRate ?? 1;
        audioSynthesizedSpeed = 1;
        configurePlaybackRate(player, audioBaseRate);
        player.volume = Math.min(1, Math.max(0, prefs.value.volume));
        audio = player;
        let settled = false;
        const finish = (error?: Error) => {
          if (settled) return;
          settled = true;
          player.onended = null;
          player.onerror = null;
          if (audio === player) {
            audio = null;
            audioCancel = null;
          }
          if (error) reject(error);
          else resolve();
        };
        audioCancel = () => finish();
        player.onended = () => finish();
        player.onerror = () =>
          finish(new Error("Не удалось воспроизвести аудио"));
        void player
          .play()
          .catch((error: unknown) =>
            finish(error instanceof Error ? error : new Error(String(error))),
          );
      }),
    () => token === playback.token,
  );
}
async function speak(text: string, speed: number, token: number) {
  for (const chunk of speechChunks(text)) {
    if (token !== playback.token) return;
    await speakChunk(chunk, speed, token);
  }
}
async function play() {
  const initial = book.value;
  if (
    closing.value ||
    !native.value ||
    !initial ||
    apiKeyBusy.value ||
    modelSaving.value
  )
    return;
  if (paused.value && loopRunning) {
    paused.value = false;
    playing.value = true;
    playback.resume();
    if (audio) {
      const player = audio;
      const attempt = ++audioAttempt;
      try {
        await player.play();
        if (
          attempt === audioAttempt &&
          playing.value &&
          !paused.value &&
          currentAudioBlock &&
          currentPrefetchBlockId &&
          playbackInterest
        ) {
          const context = await playbackInterest.update([
            currentAudioBlock.id,
            currentPrefetchBlockId,
          ]);
          if (
            context &&
            attempt === audioAttempt &&
            playing.value &&
            !paused.value
          )
            audioLoader.prefetch(currentPrefetchKey, true, context);
        }
      } catch (e) {
        if (attempt === audioAttempt && tokenIsActive(player)) {
          playing.value = false;
          paused.value = true;
          playback.pause();
          report(e);
        }
      }
    }
    return;
  }
  if (!initial.passages.length) return;
  if (completed.value) {
    active.value = replayIndex(true, active.value);
    initial.offset = initial.passages[active.value]!.start;
    initial.audioPosition = null;
    completed.value = false;
  }
  stop();
  const token = playback.token;
  playing.value = true;
  loopRunning = true;
  notice.value = "";
  let ownedAudio: SpeechAudio | null = null;
  try {
    await savePrefsNow();
    let explicitRetry = true;
    while (token === playback.token && book.value) {
      if (!(await waitForPlayback(token))) return;
      const current = book.value;
      const offline = !!offlineSource.value[current.id];
      const plan = await invoke<AudioPlan>("audio_plan", {
        bookId: current.id,
        chapter: current.chapter,
        offline,
      });
      if (!(await waitForPlayback(token))) return;
      let index = blockForOffset(plan.blocks, current.offset);
      for (; index < plan.blocks.length; index++) {
        if (!(await waitForPlayback(token))) return;
        const block = plan.blocks[index]!;
        const key = audioKey(
          current.id,
          current.chapter,
          block.id,
          plan.profileId,
          offline,
        );
        let result: SpeechAudio;
        let confirmedAttempt: string | undefined;
        while (true) {
          if (!(await waitForPlayback(token))) return;
          if (token !== playback.token) return;
          if (paused.value) continue;
          if (!playbackInterest)
            throw new Error("Нет активной сессии воспроизведения");
          const context = await playbackInterest.update([block.id]);
          if (!context || token !== playback.token || paused.value) continue;
          try {
            result = await playback.load(
              {
                key,
                retry: explicitRetry || !!confirmedAttempt,
                context: { ...context, confirmedAttempt },
              },
              token,
            );
            audioLoader.takeResource(key, result);
            ownedAudio = result;
            explicitRetry = false;
            break;
          } catch (error) {
            if (token !== playback.token) return;
            if (audioErrorCode(error) === "cancelled") continue;
            if (audioErrorCode(error) !== "retry_required") throw error;
            do {
              if (!(await waitForPlayback(token))) return;
              if (token !== playback.token) return;
            } while (paused.value);
            const attemptId =
              typeof error === "object" &&
              error !== null &&
              "attemptId" in error &&
              typeof error.attemptId === "string"
                ? error.attemptId
                : undefined;
            const confirmed = await confirmRetry(error);
            if (
              token !== playback.token ||
              book.value?.id !== current.id ||
              book.value?.chapter !== current.chapter
            )
              return;
            if (!confirmed || !attemptId) {
              stop();
              return;
            }
            confirmedAttempt = attemptId;
          }
        }
        do {
          if (!(await waitForPlayback(token))) return;
          if (token !== playback.token) return;
        } while (paused.value);
        const startingOffset = Math.max(
          block.start,
          Math.min(current.offset, block.end),
        );
        audioBaseRate = result.playbackRate ?? 1;
        audioSynthesizedSpeed = 1;
        const next = plan.blocks[index + 1];
        currentPrefetchKey = next
          ? audioKey(
              current.id,
              current.chapter,
              next.id,
              plan.profileId,
              offline,
            )
          : undefined;
        currentPrefetchBlockId = next?.id;
        const attempt = ++audioAttempt;
        const session = playback.block(
          result.resourceId
            ? convertFileSrc(result.resourceId, "readio-audio")
            : `data:${result.mime};base64,${result.data}`,
          {
            block,
            startingOffset,
            startingSeconds: startingBlockSeconds(
              current.audioPosition,
              current.chapter,
              block.id,
              plan.profileId,
            ),
            timings: result.timings,
            onTimingMode: (parts) => {
              currentTimedParts = parts;
              accurateTransitions.value = !!parts;
            },
            rate: audioBaseRate * current.speed,
            volume: prefs.value.volume,
            isCurrent: () => token === playback.token,
            isPlaying: () => playing.value && !paused.value,
            isStartCurrent: () => attempt === audioAttempt,
            onReady: () => {
              currentAudioReady = true;
            },
            onPosition: (offset, seconds, shouldSave) => {
              current.audioPosition = {
                chapter: current.chapter,
                blockId: block.id,
                profileId: plan.profileId,
                seconds,
              };
              const index = indexAt(offset);
              const changed = index !== active.value;
              active.value = index;
              current.offset = offset;
              if (changed || shouldSave) savePosition();
              if (changed) void jump();
            },
            onStarted: () => {
              if (!next || !playbackInterest) return;
              void playbackInterest
                .update([block.id, next.id])
                .then((context) => {
                  if (
                    context &&
                    token === playback.token &&
                    attempt === audioAttempt &&
                    playing.value &&
                    !paused.value
                  )
                    audioLoader.prefetch(currentPrefetchKey, true, context);
                })
                .catch(report);
            },
          },
        );
        audio = session.player;
        currentAudioBlock = block;
        currentAudioProfile = plan.profileId;
        audioCancel = session.cancel;
        session.start();
        try {
          await session.finished;
        } finally {
          if (audio === session.player) {
            audio = null;
            audioCancel = null;
            currentAudioBlock = null;
            currentAudioProfile = null;
            currentAudioReady = false;
            currentTimedParts = null;
            accurateTransitions.value = false;
          }
        }
        releaseAudio(result);
        ownedAudio = null;
        if (token !== playback.token) return;
        current.offset = block.end;
        current.audioPosition = null;
        savePosition();
      }
      if (current.chapter + 1 >= current.chapters.length) {
        current.offset =
          current.chapters[current.chapter]?.length ?? current.offset;
        completed.value = true;
        playing.value = false;
        await savePositionNow();
        return;
      }
      clearTimeout(positionTimer);
      const nextPosition = {
        id: current.id,
        chapter: current.chapter + 1,
        offset: 0,
        speed: current.speed,
        audioPosition: null,
      };
      positionQueue = positionQueue
        .then(async () => {
          if (token === playback.token)
            updateSummary(
              await invoke<BookSummary>("save_position", nextPosition),
            );
        })
        .catch(report);
      await positionQueue;
      if (token !== playback.token) return;
      const next = await invoke<Book>("open_book", { id: current.id });
      if (token !== playback.token) return;
      book.value = next;
      active.value = 0;
      await jump();
    }
  } catch (e) {
    if (token === playback.token) {
      playing.value = false;
      paused.value = false;
      report(`Ошибка озвучки: ${audioErrorMessage(e)}`);
    }
  } finally {
    if (ownedAudio) releaseAudio(ownedAudio);
    if (token === playback.token) {
      loopRunning = false;
      withdrawPlaybackInterest();
    }
  }
}
async function previewVoice() {
  if (closing.value || apiKeyBusy.value || modelSaving.value) return;
  stop();
  const token = playback.token;
  if (prefs.value.voice === "openrouter" && !openRouter.value.hasApiKey) {
    notice.value = "Добавьте ключ OpenRouter в настройках";
    return;
  }
  preview.start(token);
  try {
    await savePrefsNow();
    if (token !== playback.token) return;
    await speak("Здравствуйте! Так звучит выбранный голос.", 1, token);
  } catch (e) {
    if (token === playback.token) report(e);
  } finally {
    preview.finish(token);
  }
}
function skip(delta: number) {
  const b = book.value;
  if (b?.passages.length)
    selectPassage(
      Math.max(0, Math.min(b.passages.length - 1, active.value + delta)),
      playing.value,
    );
}
async function navigate(target: Page) {
  if (target === "reader" && !book.value) return;
  if (
    (page.value === "reader" && target !== "reader" && playing.value) ||
    (page.value === "settings" && target !== "settings" && preview.active)
  )
    stop();
  page.value = target;
  if (target === "reader") await jump();
  if (target === "marks") await loadMarks();
  if (target === "settings" && !modelsLoaded.value && !modelsLoading.value)
    void loadModels();
}
async function importBooks() {
  if (closing.value || !native.value) return;
  try {
    const paths = await open({
      multiple: true,
      filters: [{ name: "Книги", extensions: ["epub", "fb2", "txt"] }],
    });
    if (closing.value || !paths?.length) return;
    const added = await invoke<BookSummary[]>("import_books", { paths });
    for (const b of added) {
      const i = books.value.findIndex((x) => x.id === b.id);
      if (i < 0) books.value.push(b);
      else books.value[i] = b;
    }
  } catch (e) {
    report(e);
  }
}
async function removeBook(b: BookSummary) {
  if (!confirm(`Удалить «${b.title}» и её закладки?`)) return;
  try {
    if (book.value?.id === b.id) {
      stop();
      await savePositionNow();
      book.value = null;
      page.value = "library";
    }
    await invoke("delete_book", { id: b.id });
    books.value = books.value.filter((x) => x.id !== b.id);
  } catch (e) {
    report(e);
  }
}
async function addMark() {
  const b = book.value;
  const p = b?.passages[active.value];
  if (!b || !p) return;
  try {
    const mark = await invoke<Mark>("add_bookmark", {
      id: b.id,
      chapter: b.chapter,
      offset: p.start,
    });
    b.marks.push(mark);
    notice.value = "Закладка добавлена";
  } catch (e) {
    report(e);
  }
}
async function loadMarks() {
  try {
    marks.value = await invoke<typeof marks.value>("list_bookmarks");
  } catch (e) {
    report(e);
  }
}
async function removeMark(id: string, markId: string) {
  try {
    await invoke("delete_bookmark", { id, markId });
    marks.value = marks.value.filter(
      (x) => x.mark.id !== markId || x.book.id !== id,
    );
    if (book.value?.id === id)
      book.value.marks = book.value.marks.filter((x) => x.id !== markId);
  } catch (e) {
    report(e);
  }
}
async function setBackground(clear = false) {
  if (closing.value) return;
  try {
    const path = clear
      ? null
      : await open({
          multiple: false,
          filters: [
            {
              name: "Изображения",
              extensions: ["jpg", "jpeg", "png", "webp", "bmp"],
            },
          ],
        });
    if (closing.value || (!clear && !path)) return;
    await savePrefsNow();
    if (closing.value) return;
    const updated = await invoke<Preferences>("set_background", { path });
    updated.openRouterBillingUnits = { ...prefs.value.openRouterBillingUnits };
    prefs.value = normalizePreferences(defaults, updated);
  } catch (e) {
    report(e);
  }
}
let diagnosticsRequest = 0;
let diagnosticsAfterPollFailure = false;
async function refreshOfflineDiagnostics(dialogToken?: number) {
  const request = ++diagnosticsRequest;
  try {
    const issues = await invoke<OfflineDiagnostic[]>("offline_diagnostics");
    if (
      request !== diagnosticsRequest ||
      closing.value ||
      !mounted ||
      (dialogToken !== undefined &&
        (dialogToken !== offlineDialogGeneration || !offlineDialog.value))
    )
      return;
    offlineDiagnostics.value = issues;
  } catch (error) {
    if (
      request === diagnosticsRequest &&
      mounted &&
      !closing.value &&
      (dialogToken === undefined ||
        (dialogToken === offlineDialogGeneration && offlineDialog.value))
    )
      report(error);
  }
}
function diagnosticTitle(issue: OfflineDiagnostic) {
  return (
    books.value.find((item) => item.id === issue.bookId)?.title ??
    "Неизвестное издание"
  );
}
function diagnosticFilename(file: string) {
  return file.split(/[\\/]/).pop() || file;
}
async function restoreOfflineManifest(issue: OfflineDiagnostic) {
  if (closing.value || offlineBusy.value || !offlineDialog.value) return;
  const token = offlineDialogGeneration;
  offlineBusy.value = true;
  offlineError.value = "";
  try {
    const backupPath = await open({
      multiple: false,
      filters: [{ name: "JSON", extensions: ["json"] }],
    });
    if (
      token !== offlineDialogGeneration ||
      !offlineDialog.value ||
      closing.value ||
      !backupPath
    )
      return;
    await invoke("offline_restore_manifest", { file: issue.file, backupPath });
    if (
      token !== offlineDialogGeneration ||
      !offlineDialog.value ||
      closing.value
    )
      return;
    await refreshOfflineDiagnostics(token);
    if (
      token !== offlineDialogGeneration ||
      !offlineDialog.value ||
      closing.value
    )
      return;
    await refreshOfflineStatuses(
      () =>
        token === offlineDialogGeneration &&
        offlineDialog.value &&
        !closing.value,
    );
    if (
      token === offlineDialogGeneration &&
      offlineDialog.value &&
      !closing.value
    )
      offlineInfo.value = "Резервная копия восстановлена.";
  } catch (error) {
    if (
      token === offlineDialogGeneration &&
      offlineDialog.value &&
      !closing.value
    )
      offlineError.value = audioErrorMessage(error);
  } finally {
    if (token === offlineDialogGeneration && offlineDialog.value)
      offlineBusy.value = false;
  }
}
let offlinePolling = false;
let offlinePollPending = false;
let offlineEventWhileBusy = false;
watch(offlineBusy, (busy) => {
  if (!busy && offlineEventWhileBusy) {
    offlineEventWhileBusy = false;
    void refreshOfflineStatuses();
  }
});
async function refreshOfflineStatuses(isCurrent: () => boolean = () => true) {
  if (!native.value) return;
  if (offlinePolling) {
    offlinePollPending = true;
    return;
  }
  offlinePolling = true;
  try {
    const statuses = await invoke<OfflineStatus[]>("offline_list");
    if (!isCurrent() || closing.value || !mounted) return;
    diagnosticsAfterPollFailure = false;
    offlineStatuses.value = Object.fromEntries(
      statuses.map((status) => [status.bookId, status]),
    );
    const selected = offlineStatuses.value[offlineBookId.value];
    if (
      shouldRefreshPausedQuote(
        selected,
        offlineQuote.value,
        offlineDialog.value,
        offlineBusy.value,
      )
    ) {
      const requestedBook = offlineBookId.value;
      const pendingQuote = quoteOffline();
      const quoteToken = offlineDialogGeneration;
      void pendingQuote.then(() => {
        if (
          quoteToken === offlineDialogGeneration &&
          offlineDialog.value &&
          offlineBookId.value === requestedBook &&
          offlineQuote.value
        )
          offlineInfo.value =
            "Расчёт обновлён: завершилась отправленная до паузы часть. Проверьте его перед запуском.";
      });
    }
  } catch (e) {
    if (!isCurrent() || closing.value || !mounted) return;
    report(e);
    if (!diagnosticsAfterPollFailure) {
      diagnosticsAfterPollFailure = true;
      void refreshOfflineDiagnostics();
    }
  } finally {
    offlinePolling = false;
    if (offlinePollPending) {
      offlinePollPending = false;
      void refreshOfflineStatuses();
    }
  }
}
function saveSourceChoice() {
  try {
    localStorage.setItem(
      "readio.offlineSource",
      JSON.stringify(offlineSource.value),
    );
  } catch {
    notice.value = "Не удалось сохранить выбор источника озвучки";
  }
}
function toggleOfflineSource() {
  const current = book.value;
  if (!current) return;
  if (
    !offlineSource.value[current.id] &&
    offlineStatuses.value[current.id]?.state !== "complete"
  )
    return;
  stop();
  offlineSource.value[current.id] = !offlineSource.value[current.id];
  saveSourceChoice();
}
let offlineDialogGeneration = 0;
function closeOffline() {
  offlineDialogGeneration++;
  offlineDialog.value = false;
  offlineBusy.value = false;
  if (native.value && !closing.value) void refreshOfflineDiagnostics();
  offlinePreviousFocus?.focus();
}
function trapDialogFocus(event: KeyboardEvent) {
  const items = [
    ...(offlineDialogElement.value?.querySelectorAll<HTMLElement>(
      'button:not(:disabled), input:not(:disabled), select:not(:disabled), [tabindex]:not([tabindex="-1"])',
    ) ?? []),
  ];
  if (!items.length) return;
  const first = items[0]!;
  const last = items[items.length - 1]!;
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}
async function quoteOffline() {
  const bookId = offlineBookId.value;
  if (!bookId) return;
  const previousModel = offlineQuote.value?.model ?? null;
  const preserveDraft = billingUnitDirty(
    previousModel,
    billingUnitDraft.value,
    prefs.value.openRouterBillingUnits,
  );
  const token = ++offlineDialogGeneration;
  offlineBusy.value = true;
  offlineError.value = "";
  offlineQuote.value = null;
  offlineRetryConfirmed.value = false;
  try {
    const quote = await invoke<OfflineQuote>("offline_quote", { bookId });
    if (token === offlineDialogGeneration) {
      offlineQuote.value = quote;
      if (!preserveDraft || quote.model !== previousModel)
        billingUnitDraft.value = quote.model
          ? (prefs.value.openRouterBillingUnits[quote.model] ?? "")
          : "";
    }
  } catch (e) {
    if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(e);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
async function showOfflineDialog(bookId: string) {
  if (closing.value) return;
  offlinePreviousFocus =
    document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
  const openToken = ++offlineDialogGeneration;
  offlineRetryConfirmed.value = false;
  maxNewBlocks.value = null;
  maxCostUsd.value = "";
  allowUnknownCost.value = false;
  offlineBookId.value = bookId;
  offlineDialog.value = true;
  await nextTick();
  if (
    openToken !== offlineDialogGeneration ||
    !offlineDialog.value ||
    closing.value
  )
    return;
  offlineCloseButton.value?.focus();
  offlineBusy.value = false;
  offlineError.value = "";
  offlineInfo.value = "";
  offlineQuote.value = null;
  const state = offlineStatuses.value[bookId]?.state;
  const quote =
    bookId && state !== "complete" && state !== "running"
      ? quoteOffline()
      : Promise.resolve();
  void refreshOfflineDiagnostics(offlineDialogGeneration);
  await quote;
}
async function showOffline() {
  if (book.value) await showOfflineDialog(book.value.id);
}
async function saveBillingUnit() {
  const model = offlineQuote.value?.model;
  const bookId = offlineBookId.value;
  if (!model || !bookId || offlineBusy.value) return;
  const token = offlineDialogGeneration;
  const unit = billingUnitDraft.value || null;
  offlineBusy.value = true;
  offlineError.value = "";
  offlineInfo.value = "";
  try {
    await savePrefsNow();
    if (closing.value) return;
    const request = preferenceQueue.then(async () => {
      if (closing.value) return;
      const saved = await invoke<Preferences>("set_openrouter_billing_unit", {
        model,
        unit,
      });
      prefs.value.openRouterBillingUnits = { ...saved.openRouterBillingUnits };
    });
    preferenceQueue = request.catch(report);
    await request;
    if (token === offlineDialogGeneration && offlineBookId.value === bookId) {
      await quoteOffline();
      if (
        offlineDialog.value &&
        offlineBookId.value === bookId &&
        offlineQuote.value
      )
        offlineInfo.value = "Единица сохранена. Проверьте обновлённую оценку.";
    }
  } catch (e) {
    if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(e);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
async function startOffline() {
  if (
    closing.value ||
    !offlineQuote.value ||
    !offlineBookId.value ||
    offlineBusy.value ||
    (offlineQuote.value.retryRequiredBlocks > 0 &&
      !offlineRetryConfirmed.value) ||
    (offlineQuote.value.engine === "openrouter" &&
      offlineQuote.value.totalBlocks > offlineQuote.value.cachedBlocks &&
      (!maxNewBlocks.value ||
        !Number.isSafeInteger(maxNewBlocks.value) ||
        maxNewBlocks.value < 1 ||
        (offlineQuote.value.costUnit === "unknown" &&
          !allowUnknownCost.value))) ||
    (offlineQuote.value.costUnit !== "unknown" && !validMaxCost.value)
  )
    return;
  const bookId = offlineBookId.value;
  const token = offlineDialogGeneration;
  const quoteId = offlineQuote.value.quoteId;
  offlineBusy.value = true;
  offlineError.value = "";
  try {
    const status = await invoke<OfflineStatus>("offline_start", {
      bookId,
      quoteId,
      confirmRetries: offlineRetryConfirmed.value,
      budget: {
        maxCostUsd:
          offlineQuote.value?.costUnit === "unknown"
            ? null
            : maxCostUsd.value.trim() || null,
        maxNewBlocks: maxNewBlocks.value || null,
        allowUnknownCost: allowUnknownCost.value,
      },
    });
    offlineStatuses.value[status.bookId] = status;
    if (token === offlineDialogGeneration) {
      offlineQuote.value = null;
      offlineInfo.value = "";
    }
    void refreshOfflineStatuses();
  } catch (e) {
    if (token === offlineDialogGeneration && isStaleQuoteError(e)) {
      const pendingQuote = quoteOffline();
      const quoteToken = offlineDialogGeneration;
      await pendingQuote;
      if (
        quoteToken === offlineDialogGeneration &&
        offlineDialog.value &&
        offlineBookId.value === bookId &&
        offlineQuote.value
      )
        offlineInfo.value = "Расчёт обновлён. Проверьте его перед запуском.";
    } else if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(e);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
async function reconcileOfflineCost() {
  if (!offlineBookId.value || offlineBusy.value) return;
  const id = offlineBookId.value;
  const token = offlineDialogGeneration;
  offlineBusy.value = true;
  offlineError.value = "";
  try {
    const status = await invoke<OfflineStatus>("reconcile_book_cost", {
      bookId: id,
    });
    offlineStatuses.value[id] = status;
    if (token === offlineDialogGeneration)
      offlineInfo.value = status.unknownCostRequests
        ? "Не все расходы ещё доступны у провайдера."
        : "Расходы сверены с провайдером.";
  } catch (error) {
    if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(error);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
async function pauseOffline() {
  if (!offlineBookId.value || offlineBusy.value) return;
  const bookId = offlineBookId.value;
  const token = offlineDialogGeneration;
  offlineBusy.value = true;
  offlineError.value = "";
  try {
    const status = await invoke<OfflineStatus>("offline_pause", { bookId });
    offlineStatuses.value[status.bookId] = status;
    void refreshOfflineStatuses();
    if (token === offlineDialogGeneration) await quoteOffline();
  } catch (e) {
    if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(e);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
async function removeOffline() {
  if (!offlineBookId.value || offlineBusy.value) return;
  if (!confirm("Удалить сохранённую офлайн-озвучку этой книги?")) return;
  const bookId = offlineBookId.value;
  const token = offlineDialogGeneration;
  offlineBusy.value = true;
  offlineError.value = "";
  try {
    if (book.value?.id === bookId && offlineSource.value[bookId]) stop();
    await invoke("offline_remove", { bookId });
    delete offlineStatuses.value[bookId];
    delete offlineSource.value[bookId];
    saveSourceChoice();
    audioLoader.clearBook(bookId);
    if (token === offlineDialogGeneration) await quoteOffline();
  } catch (e) {
    if (token === offlineDialogGeneration)
      offlineError.value = audioErrorMessage(e);
  } finally {
    if (token === offlineDialogGeneration) offlineBusy.value = false;
  }
}
function addDictionary() {
  const word = dictionaryWord.value.trim();
  const value = dictionaryValue.value.trim();
  if (!word || !value) return;
  stop();
  prefs.value.dictionary[word] = value;
  dictionaryWord.value = "";
  dictionaryValue.value = "";
  savePrefs();
}
async function refreshOpenRouter() {
  const state = await invoke<Bootstrap>("bootstrap");
  voices.value = state.voices;
  openRouter.value = state.openRouter;
}
async function saveApiKey() {
  const apiKey = apiKeyDraft.value.trim();
  if (closing.value || !apiKey || apiKeyBusy.value) return;
  stop();
  apiKeyBusy.value = true;
  try {
    await preview.wait();
    if (closing.value) return;
    openRouter.value = await invoke<OpenRouterStatus>("set_openrouter_key", {
      apiKey,
    });
    apiKeyDraft.value = "";
    invalidateModelCatalog();
    notice.value = "Ключ OpenRouter сохранён";
    try {
      await refreshOpenRouter();
      await loadModels(true);
    } catch {
      notice.value = "Ключ сохранён, но список голосов не обновлён";
    }
  } catch (e) {
    report(e);
  } finally {
    apiKeyBusy.value = false;
  }
}
async function deleteApiKey() {
  if (closing.value || apiKeyBusy.value) return;
  stop();
  apiKeyBusy.value = true;
  try {
    await preview.wait();
    if (closing.value) return;
    openRouter.value = await invoke<OpenRouterStatus>("delete_openrouter_key");
    apiKeyDraft.value = "";
    invalidateModelCatalog();
    notice.value = "Ключ OpenRouter удалён";
    try {
      await refreshOpenRouter();
      await loadModels(true);
    } catch {
      notice.value = "Ключ удалён, но список голосов не обновлён";
    }
  } catch (e) {
    report(e);
  } finally {
    apiKeyBusy.value = false;
  }
}
function invalidateModelCatalog() {
  modelCatalogEpoch++;
  models.value = [];
  modelsLoaded.value = false;
  modelsError.value = "";
}
async function loadModels(refresh = false) {
  if (!native.value) return;
  if (modelsLoading.value) {
    if (refresh) pendingModelRefresh = true;
    return;
  }
  modelsLoading.value = true;
  modelsError.value = "";
  const epoch = modelCatalogEpoch;
  try {
    const result = await invoke<OpenRouterModel[]>("list_openrouter_models", {
      refresh,
    });
    if (epoch === modelCatalogEpoch) {
      models.value = result;
      modelsLoaded.value = true;
    }
  } catch (e) {
    if (epoch === modelCatalogEpoch) modelsError.value = audioErrorMessage(e);
  } finally {
    modelsLoading.value = false;
    if (pendingModelRefresh) {
      pendingModelRefresh = false;
      void loadModels(true);
    }
  }
}
async function changeModel(event: Event) {
  const id = (event.target as HTMLSelectElement).value;
  if (id === prefs.value.openRouterModel || modelSaving.value) return;
  const model = models.value.find((item) => item.id === id);
  if (id && !model) return;
  stop();
  modelSaving.value = true;
  const previous = {
    model: prefs.value.openRouterModel,
    voice: prefs.value.openRouterVoice,
    engine: prefs.value.voice,
    settings: prefs.value.openRouterModelSettings ?? {},
  };
  let committed = false;
  try {
    const selection = selectionForModel(model);
    prefs.value.openRouterModel = selection.model;
    if (selection.model && model?.canonicalSlug) {
      prefs.value.openRouterModelSettings = modelSettingsWithRevision(
        prefs.value.openRouterModelSettings,
        selection.model,
        model.canonicalSlug,
      );
    }
    prefs.value.openRouterVoice = null;
    if (selection.model) prefs.value.voice = "openrouter";
    await savePrefsNow();
    committed = true;
    if (selection.voice) {
      prefs.value.openRouterVoice = selection.voice;
      await savePrefsNow();
    }
  } catch (e) {
    if (committed) {
      prefs.value.openRouterVoice = null;
    } else {
      prefs.value.openRouterModel = previous.model;
      prefs.value.openRouterVoice = previous.voice;
      prefs.value.voice = previous.engine;
      prefs.value.openRouterModelSettings = previous.settings;
      (event.target as HTMLSelectElement).value = previous.model ?? "";
    }
    report(e);
  } finally {
    modelSaving.value = false;
  }
}
const modelTransport = ref<"mp3" | "pcm">("mp3");
const pcmRate = ref<number | null>(null);
const pcmChannels = ref<number | null>(null);
const inputLimitUnit = ref<"" | "character" | "utf8_byte" | "xml_escaped_byte">(
  "",
);
const inputLimitValue = ref<number | null>(null);
watch(
  () =>
    [prefs.value.openRouterModel, prefs.value.openRouterModelSettings] as const,
  ([model, settings]) => {
    const current = model ? settings?.[model] : undefined;
    modelTransport.value = current?.transport?.format ?? "mp3";
    pcmRate.value = current?.transport?.pcmSampleRate ?? null;
    pcmChannels.value = current?.transport?.pcmChannels ?? null;
    inputLimitUnit.value = current?.inputLimit?.unit ?? "";
    inputLimitValue.value = current?.inputLimit?.value ?? null;
  },
  { immediate: true },
);
async function saveModelSettings() {
  const model = prefs.value.openRouterModel;
  if (!model || modelSaving.value) return;
  if (
    inputLimitUnit.value &&
    (!inputLimitValue.value || inputLimitValue.value < 1)
  ) {
    notice.value = "Укажите положительный лимит ввода модели";
    return;
  }
  stop();
  const previous = prefs.value.openRouterModelSettings ?? {};
  prefs.value.openRouterModelSettings = {
    ...previous,
    [model]: modelSettingsWithLimit(
      previous[model],
      {
        format: modelTransport.value,
        pcmSampleRate: modelTransport.value === "pcm" ? pcmRate.value : null,
        pcmChannels: modelTransport.value === "pcm" ? pcmChannels.value : null,
      },
      inputLimitUnit.value && inputLimitValue.value
        ? {
            unit: inputLimitUnit.value,
            value: inputLimitValue.value,
            source: "user",
          }
        : null,
    ),
  };
  modelSaving.value = true;
  try {
    await savePrefsNow();
  } catch (error) {
    prefs.value.openRouterModelSettings = previous;
    report(error);
  } finally {
    modelSaving.value = false;
  }
}
function changeOpenRouterVoice(event: Event) {
  stop();
  const value = (event.target as HTMLInputElement).value.trim();
  prefs.value.openRouterVoice = value || null;
  savePrefs();
}
function changeVoice() {
  stop();
  savePrefs();
  if (
    prefs.value.voice === "openrouter" &&
    !modelsLoaded.value &&
    !modelsLoading.value
  )
    void loadModels();
}
function changeSpeed(event: Event) {
  if (!book.value) return;
  book.value.speed = Number((event.target as HTMLInputElement).value);
  if (audio)
    audio.playbackRate = playbackRate(
      audioBaseRate,
      audioSynthesizedSpeed,
      book.value.speed,
    );
  savePosition();
}
function changeVolume(event: Event) {
  const value = Math.min(
    1,
    Math.max(0, Number((event.target as HTMLInputElement).value)),
  );
  prefs.value.volume = value;
  if (audio) audio.volume = value;
  savePrefs();
}
function coverUrl(cover: string | null) {
  return cover
    ? cover.startsWith("data:")
      ? cover
      : `data:image/*;base64,${cover}`
    : "";
}
let shutdownPromise: Promise<void> | null = null;
let mounted = false;
let unlistenShutdown: (() => void) | undefined;
let unlistenClose: (() => void) | undefined;
let unlistenOffline: (() => void) | undefined;
function flushForShutdown(): Promise<void> {
  if (shutdownPromise) return shutdownPromise;
  closing.value = true;
  shutdownError.value = "";
  openGeneration++;
  offlineDialogGeneration++;
  stop();
  shutdownPromise = Promise.allSettled([
    savePositionNow(),
    savePrefsNow(),
    preview.wait(),
    audioLoader.settle(),
  ])
    .then(async (results) => {
      const errors = results.filter(
        (result): result is PromiseRejectedResult =>
          result.status === "rejected",
      );
      if (errors.length) {
        shutdownWarning.value = `Не удалось сохранить данные: ${errors.map((error) => audioErrorMessage(error.reason)).join("; ")}`;
        report(shutdownWarning.value);
      }
      await invoke("prepare_shutdown");
    })
    .catch((error: unknown) => {
      shutdownPromise = null;
      shutdownError.value = audioErrorMessage(error);
      throw error;
    });
  return shutdownPromise;
}
async function closeWindow() {
  try {
    await flushForShutdown();
    await getCurrentWindow().destroy();
  } catch (error) {
    shutdownError.value = audioErrorMessage(error);
  }
}
onMounted(async () => {
  mounted = true;
  native.value = isTauri();
  if (native.value) {
    try {
      const removeShutdown = await listen("shutdown-requested", () => {
        void flushForShutdown().catch(report);
      });
      if (!mounted) {
        removeShutdown();
        return;
      }
      unlistenShutdown = removeShutdown;
      const removeClose = await getCurrentWindow().onCloseRequested((event) => {
        event.preventDefault();
        void closeWindow();
      });
      if (!mounted) {
        removeClose();
        return;
      }
      unlistenClose = removeClose;
      const removeOffline = await listen<OfflineStatus>(
        "offline-status-changed",
        () => {
          if (offlineBusy.value) offlineEventWhileBusy = true;
          void refreshOfflineStatuses();
        },
      );
      if (!mounted) {
        removeOffline();
        return;
      }
      unlistenOffline = removeOffline;
      const session = await invoke<string>("audio_session");
      if (!mounted || closing.value) return;
      playbackInterest = new PlaybackInterest(
        session,
        (session, revision, blockIds) =>
          invoke<boolean>("audio_interest", { session, revision, blockIds }),
      );
      const state = await invoke<Bootstrap>("bootstrap");
      if (!mounted || closing.value) return;
      books.value = state.books;
      prefs.value = normalizePreferences(defaults, state.preferences);
      voices.value = state.voices;
      openRouter.value = state.openRouter;
      notice.value = state.warnings.join("\n");
      try {
        const saved: unknown = JSON.parse(
          localStorage.getItem("readio.offlineSource") ?? "{}",
        );
        if (saved && typeof saved === "object" && !Array.isArray(saved))
          offlineSource.value = Object.fromEntries(
            Object.entries(saved).filter(([, value]) => value === true),
          );
      } catch {
        offlineSource.value = {};
      }
      void refreshOfflineStatuses();
      void refreshOfflineDiagnostics();
    } catch (e) {
      report(e);
    }
  }
  loading.value = false;
});
onBeforeUnmount(() => {
  mounted = false;
  unlistenShutdown?.();
  unlistenClose?.();
  unlistenOffline?.();
  stop();
  void savePrefsNow().catch(() => {});
});
</script>
<template>
  <div class="app" :class="{ light: !prefs.dark }" :style="themeStyle">
    <div
      v-if="background"
      class="backdrop"
      :style="{
        backgroundImage: `url('${background}')`,
        opacity: prefs.backgroundOpacity,
        filter: `blur(${prefs.backgroundBlurEnabled ? prefs.backgroundBlur * 30 : 0}px)`,
      }"
    />
    <div
      v-if="background"
      class="backdrop dim"
      :style="{ opacity: prefs.backgroundDim }"
    />
    <aside class="rail panel" :inert="closing">
      <div class="brand">READIO</div>
      <nav aria-label="Главная навигация">
        <button
          :class="{ selected: page === 'library' }"
          @click="navigate('library')"
        >
          <LibraryBig />Библиотека
        </button>
        <button
          :class="{ selected: page === 'reader' }"
          :disabled="!book"
          @click="navigate('reader')"
        >
          <BookOpen />Читать
        </button>
        <button
          :class="{ selected: page === 'marks' }"
          @click="navigate('marks')"
        >
          <Bookmark />Закладки
        </button>
        <button
          :class="{ selected: page === 'settings' }"
          @click="navigate('settings')"
        >
          <Settings />Настройки
        </button>
      </nav>
      <button class="primary add-book" :disabled="!native" @click="importBooks">
        <Plus />Добавить книгу
      </button>
    </aside>
    <main class="main" :inert="closing">
      <div v-if="notice" class="notice" role="status">
        <span>{{ notice }}</span
        ><button
          class="icon"
          aria-label="Закрыть сообщение"
          @click="notice = ''"
        >
          <X />
        </button>
      </div>
      <div v-if="offlineDiagnostics.length" class="notice" role="status">
        <span
          >Обнаружены повреждённые офлайн-издания. Аудиофайлы защищены от
          очистки.</span
        >
        <button class="tonal" @click="showOfflineDialog('')">
          Восстановить
        </button>
      </div>
      <section v-if="page === 'library'" class="view">
        <header class="heading">
          <h1>Моя библиотека</h1>
          <p>Книги всегда доступны без интернета</p>
        </header>
        <div v-if="loading" class="empty">Загрузка библиотеки…</div>
        <div v-else-if="!books.length" class="empty">
          <BookOpen :size="68" />
          <p>
            {{
              native
                ? "Здесь появятся ваши книги"
                : "Откройте Readio как десктопное приложение"
            }}
          </p>
          <button v-if="native" class="primary" @click="importBooks">
            Добавить EPUB, FB2 или TXT
          </button>
        </div>
        <div v-else class="library-list scroll">
          <article v-for="item in books" :key="item.id" class="book-row panel">
            <button class="book-open" @click="openBook(item.id)">
              <span class="cover"
                ><img
                  v-if="item.cover"
                  :src="coverUrl(item.cover)"
                  alt="" /><BookOpen v-else
              /></span>
              <span class="book-info"
                ><strong>{{ item.title }}</strong
                ><span>{{ item.author }}</span
                ><span class="progress"
                  ><i
                    :style="{
                      width: `${Math.max(0, Math.min(100, item.progress * 100))}%`,
                    }" /></span
                ><small
                  >{{ Math.round(item.progress * 100) }}% прочитано
                  <span
                    v-if="offlineStatuses[item.id]?.state === 'complete'"
                    class="offline-badge"
                    ><CloudOff :size="14" /> Офлайн-аудио</span
                  ><span
                    v-else-if="offlineStatuses[item.id]?.state === 'running'"
                    class="offline-badge"
                  >
                    · Сохраняется
                    {{ offlineStatuses[item.id]?.cachedBlocks }}/{{
                      offlineStatuses[item.id]?.totalBlocks
                    }}</span
                  ></small
                ></span
              ></button
            ><button
              class="icon"
              :aria-label="`Удалить ${item.title}`"
              title="Удалить книгу"
              @click="removeBook(item)"
            >
              <Trash2 />
            </button>
          </article>
        </div>
      </section>
      <section v-else-if="page === 'reader' && book" class="view reader-view">
        <header class="reader-head">
          <div class="reader-title">
            <strong>{{ book.title }}</strong
            ><span>{{ book.author }}</span>
          </div>
          <button
            v-if="
              offlineSource[book.id] ||
              offlineStatuses[book.id]?.state === 'complete'
            "
            class="text-button source-button"
            :title="
              offlineSource[book.id]
                ? 'Использовать текущий голос'
                : 'Использовать сохранённую озвучку'
            "
            @click="toggleOfflineSource"
          >
            <CloudOff />{{
              offlineSource[book.id] ? "Сохранённая озвучка" : "Текущий голос"
            }}
          </button>
          <button
            class="tonal"
            title="Сохранить озвучку офлайн"
            aria-label="Сохранить озвучку офлайн"
            @click="showOffline"
          >
            <Download /> Офлайн
          </button>
          <button
            class="icon"
            title="Добавить закладку"
            aria-label="Добавить закладку"
            @click="addMark"
          >
            <BookmarkPlus />
          </button>
          <div class="chapter-select">
            <button
              class="icon"
              title="Главы"
              aria-label="Главы"
              :aria-expanded="chapterMenu"
              @click="chapterMenu = !chapterMenu"
            >
              <List />
            </button>
            <div v-if="chapterMenu" class="chapter-menu panel">
              <button
                v-for="(chapter, index) in book.chapters"
                :key="index"
                :class="{ selected: index === book.chapter }"
                @click="changeChapter(index)"
              >
                {{ chapter.title }}
              </button>
            </div>
          </div>
        </header>
        <div
          ref="reader"
          class="reader-scroll scroll"
          tabindex="0"
          @scroll="onReaderScroll"
          @wheel="readerIntent"
          @pointerdown="readerIntent"
          @pointermove="$event.buttons & 1 && readerIntent()"
          @keydown="readerIntent"
        >
          <div
            class="reader-content"
            :style="{ height: `${virtualizer.getTotalSize()}px` }"
          >
            <div
              v-for="row in virtualizer.getVirtualItems()"
              :key="String(row.key)"
              :ref="measureRow"
              class="virtual-row"
              :data-index="row.index"
              :style="{ transform: `translateY(${row.start}px)` }"
            >
              <template v-if="row.index === 0">
                <h2>{{ book.chapters[book.chapter]?.title }}</h2>
                <p v-if="!book.passages.length" class="empty-chapter">
                  В этой главе нет текста
                </p>
              </template>
              <button
                v-else-if="book.passages[row.index - 1]"
                class="passage"
                :class="{ highlighted: row.index - 1 === active }"
                @click="selectPassage(row.index - 1)"
              >
                {{ book.passages[row.index - 1]!.text }}
              </button>
            </div>
          </div>
        </div>
        <footer class="player panel">
          <div class="progress thin">
            <i
              :style="{
                width: `${Math.max(0, Math.min(100, book.progress * 100))}%`,
              }"
            />
          </div>
          <div class="player-row">
            <div class="player-book">
              <span class="cover small"
                ><img
                  v-if="book.cover"
                  :src="coverUrl(book.cover)"
                  alt="" /><BookOpen v-else
              /></span>
              <span class="player-book-info"
                ><strong>{{ book.title }}</strong
                ><small
                  >{{ book.author }} ·
                  {{
                    accurateTransitions
                      ? "Метки провайдера"
                      : "Переходы по тексту приблизительные"
                  }}</small
                ></span
              >
            </div>
            <div class="controls">
              <button
                class="icon"
                title="Предыдущее предложение"
                aria-label="Предыдущее предложение"
                @click="skip(-1)"
              >
                <SkipBack />
              </button>
              <button
                class="play-button"
                :title="playing ? 'Пауза' : 'Воспроизвести'"
                :aria-label="playing ? 'Пауза' : 'Воспроизвести'"
                @click="playing ? pausePlayback() : play()"
              >
                <Pause v-if="playing" /><Play v-else />
              </button>
              <button
                class="icon"
                title="Следующее предложение"
                aria-label="Следующее предложение"
                @click="skip(1)"
              >
                <SkipForward />
              </button>
              <button class="icon" title="Стоп" aria-label="Стоп" @click="stop">
                <Square />
              </button>
            </div>
            <div class="player-end">
              <div
                class="player-progress"
                :aria-label="`Прочитано ${Math.round(book.progress * 100)}%`"
              >
                <small>{{ Math.round(book.progress * 100) }}%</small>
              </div>
              <label class="player-slider" title="Скорость"
                ><span>{{ book.speed.toFixed(1) }}×</span
                ><RangeInput
                  min="0.5"
                  max="3"
                  step="0.1"
                  :value="book.speed"
                  aria-label="Скорость"
                  @input="changeSpeed"
              /></label>
              <label class="player-slider volume-slider" title="Громкость"
                ><Volume2 aria-hidden="true" /><RangeInput
                  min="0"
                  max="1"
                  step="0.05"
                  :value="prefs.volume"
                  aria-label="Громкость"
                  @input="changeVolume"
                /><span>{{ Math.round(prefs.volume * 100) }}%</span></label
              >
            </div>
          </div>
        </footer>
      </section>
      <section v-else-if="page === 'marks'" class="view">
        <header class="heading">
          <h1>Закладки</h1>
          <p>Сохранённые предложения</p>
        </header>
        <div v-if="!marks.length" class="empty">Закладок пока нет</div>
        <div v-else class="library-list scroll">
          <article
            v-for="entry in marks"
            :key="entry.book.id + entry.mark.id"
            class="mark-row panel"
          >
            <button
              class="mark-open"
              @click="
                openBook(entry.book.id, entry.mark.chapter, entry.mark.offset)
              "
            >
              <span>{{ entry.mark.excerpt }}</span
              ><small
                >{{ entry.book.title }} · {{ entry.chapterTitle }}</small
              ></button
            ><button
              class="icon"
              aria-label="Удалить закладку"
              @click="removeMark(entry.book.id, entry.mark.id)"
            >
              <Trash2 />
            </button>
          </article>
        </div>
      </section>
      <section v-else-if="page === 'settings'" class="settings scroll">
        <header class="heading">
          <h1>Настройки</h1>
          <p>Подстройте чтение и озвучку под себя</p>
        </header>
        <label class="setting switch"
          ><span>Тёмная тема</span
          ><input v-model="prefs.dark" type="checkbox" @change="savePrefs"
        /></label>
        <hr />
        <div class="setting">
          <strong>Оформление</strong
          ><small>Основной цвет и фоновое изображение</small>
          <AccentColorPicker
            v-model="prefs.primaryColor"
            @update:model-value="savePrefs"
          />
          <div class="actions">
            <button class="tonal" :disabled="!native" @click="setBackground()">
              <ImageIcon />{{
                background ? "Заменить фото" : "Выбрать фото"
              }}</button
            ><button
              v-if="background"
              class="text-button"
              @click="setBackground(true)"
            >
              <Trash2 />Убрать фон
            </button>
          </div>
        </div>
        <template v-if="background"
          ><label class="setting switch"
            ><span>Размытие фона</span
            ><input
              v-model="prefs.backgroundBlurEnabled"
              type="checkbox"
              @change="savePrefs" /></label
          ><label v-if="prefs.backgroundBlurEnabled" class="setting"
            >Сила размытия ·
            {{ Math.round(prefs.backgroundBlur * 100) }}%<RangeInput
              v-model.number="prefs.backgroundBlur"
              min="0"
              max="1"
              step="0.05"
              @input="savePrefs" /></label
          ><img
            class="background-preview"
            :src="background"
            alt="Фоновое изображение" /><label class="setting"
            >Прозрачность фото ·
            {{ Math.round((1 - prefs.backgroundOpacity) * 100) }}%<RangeInput
              min="0.15"
              max="0.95"
              step="0.05"
              :value="1 - prefs.backgroundOpacity"
              @input="
                prefs.backgroundOpacity =
                  1 - Number(($event.target as HTMLInputElement).value);
                savePrefs();
              " /></label
          ><label class="setting"
            >Затемнение фото ·
            {{ Math.round(prefs.backgroundDim * 100) }}%<RangeInput
              v-model.number="prefs.backgroundDim"
              min="0"
              max="0.8"
              step="0.05"
              @input="savePrefs" /></label
          ><label class="setting"
            >Прозрачность панелей ·
            {{ Math.round(prefs.surfaceTransparency * 100) }}%<RangeInput
              v-model.number="prefs.surfaceTransparency"
              min="0"
              max="0.75"
              step="0.05"
              @input="savePrefs" /></label
        ></template>
        <label class="setting"
          >Размер текста · {{ Math.round(prefs.fontSize)
          }}<RangeInput
            v-model.number="prefs.fontSize"
            min="14"
            max="32"
            step="1"
            @input="savePrefs" /></label
        ><label class="setting"
          >Межстрочный интервал · {{ Number(prefs.lineHeight.toFixed(2))
          }}<RangeInput
            v-model.number="prefs.lineHeight"
            min="1.2"
            max="2.2"
            step="0.05"
            @input="savePrefs"
        /></label>
        <hr />
        <div class="setting openrouter-settings">
          <strong>OpenRouter</strong>
          <small
            >Нужен собственный ключ OpenRouter. Доступность и лимиты зависят от
            выбранной модели.</small
          >
          <small
            >Текст передаётся OpenRouter и провайдеру выбранной модели. Платная
            модель автоматически не выбирается.</small
          >
          <small>Создать ключ: openrouter.ai/settings/keys</small>
          <p class="key-status" role="status">
            {{ openRouter.hasApiKey ? "Ключ сохранён" : "Ключ не добавлен" }}
          </p>
          <form class="key-form" @submit.prevent="saveApiKey">
            <label
              >Ключ API OpenRouter<input
                v-model="apiKeyDraft"
                type="password"
                autocomplete="off"
                spellcheck="false"
                :disabled="!native || apiKeyBusy"
            /></label>
            <button
              class="tonal"
              type="submit"
              :disabled="!native || apiKeyBusy || !apiKeyDraft.trim()"
            >
              Сохранить ключ
            </button>
            <button
              class="text-button"
              type="button"
              :disabled="!native || apiKeyBusy || !openRouter.hasApiKey"
              @click="deleteApiKey"
            >
              Удалить ключ
            </button>
          </form>
          <div class="model-search">
            <label
              >Поиск модели<input
                v-model="modelQuery"
                type="search"
                placeholder="Название или ID"
                :disabled="!native"
            /></label>
            <label
              >Стоимость<select v-model="modelFilter" :disabled="!native">
                <option value="all">Все</option>
                <option value="free">Бесплатные</option>
                <option value="paid">Платные</option>
              </select></label
            >
            <button
              class="tonal"
              type="button"
              :disabled="!native || modelsLoading"
              @click="loadModels(true)"
            >
              {{ modelsLoading ? "Загрузка…" : "Обновить каталог" }}
            </button>
          </div>
          <small v-if="modelsLoading" role="status"
            >Загрузка каталога моделей…</small
          >
          <small v-else-if="modelsError" role="alert"
            >Не удалось загрузить каталог: {{ modelsError }}. Нажмите «Обновить
            каталог», чтобы повторить.</small
          >
          <small v-else-if="modelsLoaded"
            >Моделей: {{ models.length }} · показано:
            {{ filteredModels.length }}</small
          >
          <label class="model-field"
            >Модель OpenRouter
            <select
              :value="prefs.openRouterModel ?? ''"
              :disabled="!native || modelSaving"
              @change="changeModel"
            >
              <option value="">Выберите модель</option>
              <option
                v-if="prefs.openRouterModel && !selectedInFilter"
                :value="prefs.openRouterModel"
              >
                {{ selectedModel?.name ?? prefs.openRouterModel
                }}{{ selectedModel ? " · текущая" : " · нет в каталоге" }}
              </option>
              <option
                v-for="model in filteredModels"
                :key="model.id"
                :value="model.id"
              >
                {{ model.name }} · {{ model.free ? "бесплатно" : "платно" }}
              </option>
            </select>
          </label>
          <small v-if="selectedModel"
            >{{ selectedModel.id }} ·
            {{ selectedModel.free ? "бесплатно" : "платно" }}</small
          >
          <small
            v-if="
              selectedModel &&
              (selectedModel.pricing.prompt || selectedModel.pricing.completion)
            "
            >Тариф API, USD: вход
            {{ selectedModel.pricing.prompt || "не указан" }} · выход
            {{ selectedModel.pricing.completion || "не указан" }}. Единицы
            тарификации зависят от модели.</small
          >
          <small
            v-if="
              prefs.openRouterModel &&
              modelsLoaded &&
              !modelsLoading &&
              !modelsError &&
              !selectedModel
            "
            >Сохранённая модель отсутствует в текущем каталоге. Выбор
            сохранён.</small
          >
          <label v-if="selectedModel?.voices.length" class="model-field"
            >Голос модели
            <select
              :value="prefs.openRouterVoice ?? ''"
              :disabled="!native || modelSaving"
              @change="changeOpenRouterVoice"
            >
              <option value="">По умолчанию</option>
              <option
                v-if="
                  prefs.openRouterVoice &&
                  !selectedModel.voices.includes(prefs.openRouterVoice)
                "
                :value="prefs.openRouterVoice"
              >
                {{ prefs.openRouterVoice }} · сохранённый
              </option>
              <option
                v-for="voice in selectedModel.voices"
                :key="voice"
                :value="voice"
              >
                {{ voice }}
              </option>
            </select>
          </label>
          <label v-else-if="prefs.openRouterModel" class="model-field"
            >ID голоса модели (необязательно)<input
              type="text"
              :value="prefs.openRouterVoice ?? ''"
              autocomplete="off"
              spellcheck="false"
              :disabled="!native || modelSaving"
              @input="changeOpenRouterVoice"
          /></label>
          <form
            v-if="prefs.openRouterModel"
            class="model-settings"
            @submit.prevent="saveModelSettings"
          >
            <strong>Формат и лимит модели</strong>
            <label class="model-settings-format"
              >Формат аудио<select
                v-model="modelTransport"
                :disabled="modelSaving"
              >
                <option value="mp3">MP3</option>
                <option value="pcm">PCM</option>
              </select></label
            >
            <template v-if="modelTransport === 'pcm'">
              <label
                >Частота PCM, Гц<input
                  v-model.number="pcmRate"
                  type="number"
                  min="1"
                  step="1"
                  :disabled="modelSaving"
              /></label>
              <label
                >Каналы PCM<input
                  v-model.number="pcmChannels"
                  type="number"
                  min="1"
                  step="1"
                  :disabled="modelSaving"
              /></label>
              <small
                >Если ответ не содержит характеристики PCM, укажите частоту и
                число каналов.</small
              >
            </template>
            <label
              >Единица лимита ввода<select
                v-model="inputLimitUnit"
                :disabled="modelSaving"
              >
                <option value="">Не указан</option>
                <option value="character">Символы</option>
                <option value="utf8_byte">Байты UTF-8</option>
                <option value="xml_escaped_byte">
                  Байты после XML-экранирования
                </option>
              </select></label
            >
            <label v-if="inputLimitUnit"
              >Лимит ввода<input
                v-model.number="inputLimitValue"
                type="number"
                min="1"
                step="1"
                required
                :disabled="modelSaving"
            /></label>
            <small
              >Укажите лимит по документации модели. Изменение формата или
              лимита пересоздаёт профиль аудио.</small
            >
            <button class="tonal" type="submit" :disabled="modelSaving">
              Сохранить параметры модели
            </button>
          </form>
          <small
            >Без ID используется голос модели по умолчанию. Выберите OpenRouter
            в общем списке голосов ниже.</small
          >
        </div>
        <hr />
        <label class="setting"
          >Движок и голос<select
            v-model="prefs.voice"
            :disabled="!native"
            @change="changeVoice"
          >
            <option :value="null">Голос по умолчанию</option>
            <option v-for="voice in voices" :key="voice.id" :value="voice.id">
              {{ voice.name }} ({{ voice.locale }})
            </option>
          </select></label
        >
        <div class="setting setting-row">
          <span
            ><strong>Прослушать голос</strong
            ><small>Проверить выбранный голос</small></span
          ><button
            class="icon"
            :disabled="!native || apiKeyBusy || modelSaving"
            aria-label="Прослушать голос"
            @click="previewVoice"
          >
            <Play />
          </button>
        </div>
        <p
          v-if="!voices.some((v) => v.locale.toLowerCase().startsWith('ru'))"
          class="warning"
        >
          Русский голос не найден в списке доступных голосов.
        </p>
        <hr />
        <div class="setting">
          <strong>Словарь произношения</strong
          ><small>Подмена слов действует только при озвучке</small>
          <form class="dictionary-form" @submit.prevent="addDictionary">
            <label
              >Слово в книге<input v-model="dictionaryWord" required /></label
            ><label
              >Как произносить<input
                v-model="dictionaryValue"
                required /></label
            ><button class="tonal" type="submit"><Plus />Добавить</button>
          </form>
          <div
            v-for="(value, word) in prefs.dictionary"
            :key="word"
            class="dictionary-entry"
          >
            <span
              >{{ word }}<small>Произнести: {{ value }}</small></span
            ><button
              class="icon"
              :aria-label="`Удалить ${word}`"
              @click="
                stop();
                delete prefs.dictionary[word];
                savePrefs();
              "
            >
              <X />
            </button>
          </div>
        </div>
      </section>
      <div
        v-if="retryPrompt"
        class="dialog-overlay"
        @keydown.esc="resolveRetry(false)"
        @click.self="resolveRetry(false)"
      >
        <section
          ref="retryDialog"
          class="offline-dialog"
          role="dialog"
          aria-modal="true"
          aria-labelledby="retry-heading"
          aria-describedby="retry-description"
          @keydown.tab="trapRetryFocus"
        >
          <header class="dialog-heading">
            <h2 id="retry-heading">Повторить запрос?</h2>
          </header>
          <p id="retry-description">
            {{ retryPrompt.message }} Повторный запрос может привести к
            дополнительному списанию.
          </p>
          <div class="dialog-actions">
            <button class="tonal" @click="resolveRetry(true)">
              Повторить запрос
            </button>
            <button
              ref="retryCancelButton"
              class="text-button"
              @click="resolveRetry(false)"
            >
              Отмена
            </button>
          </div>
        </section>
      </div>
      <div
        v-if="offlineDialog"
        class="dialog-overlay"
        @keydown.esc="closeOffline"
        @click.self="closeOffline"
      >
        <section
          ref="offlineDialogElement"
          class="offline-dialog"
          role="dialog"
          @keydown.tab="trapDialogFocus"
          aria-modal="true"
          aria-labelledby="offline-heading"
        >
          <header class="dialog-heading">
            <h2 id="offline-heading">
              {{
                offlineBookId ? "Сохранить озвучку офлайн" : "Офлайн-издания"
              }}
            </h2>
            <button
              ref="offlineCloseButton"
              class="icon"
              aria-label="Закрыть"
              @click="closeOffline"
            >
              <X />
            </button>
          </header>
          <p v-if="offlineBookId">
            Книга отправляется на озвучку частями. Для облачного голоса текст
            передаётся выбранному сервису.
          </p>
          <section
            v-if="offlineDiagnostics.length"
            aria-labelledby="offline-diagnostics-heading"
          >
            <h3 id="offline-diagnostics-heading">
              Повреждённые офлайн-издания
            </h3>
            <p class="warning">
              Аудиофайлы защищены от очистки. Выберите резервную копию описания
              издания для восстановления.
            </p>
            <div v-for="issue in offlineDiagnostics" :key="issue.file">
              <p>
                <strong>{{ diagnosticTitle(issue) }}</strong> ·
                {{ diagnosticFilename(issue.file) }}<br /><small>{{
                  issue.message
                }}</small>
              </p>
              <button
                class="tonal"
                :disabled="offlineBusy"
                @click="restoreOfflineManifest(issue)"
              >
                Восстановить из резервной копии
              </button>
            </div>
          </section>
          <template
            v-if="
              offlineStatuses[offlineBookId] &&
              offlineStatuses[offlineBookId]?.state !== 'none'
            "
          >
            <p class="offline-state">
              {{
                (
                  {
                    running: "Сохранение",
                    paused: "Приостановлено",
                    complete: "Сохранено",
                    error: "Ошибка",
                    none: "",
                  } as const
                )[offlineStatuses[offlineBookId]!.state]
              }}
              · {{ offlineStatuses[offlineBookId]!.cachedBlocks }} /
              {{ offlineStatuses[offlineBookId]!.totalBlocks }} частей
            </p>
            <div class="progress">
              <i
                :style="{
                  width: `${offlineStatuses[offlineBookId]!.totalBlocks ? Math.min(100, (offlineStatuses[offlineBookId]!.cachedBlocks / offlineStatuses[offlineBookId]!.totalBlocks) * 100) : 0}%`,
                }"
              />
            </div>
            <p
              v-if="offlineStatuses[offlineBookId]?.pendingRequests"
              class="warning"
            >
              Отправленных запросов в работе:
              {{ offlineStatuses[offlineBookId]?.pendingRequests }}. Их
              результат может сохраниться после паузы.
            </p>
            <p v-if="offlineStatuses[offlineBookId]?.submittedRequests">
              Запросов по этой книге:
              {{ offlineStatuses[offlineBookId]?.submittedRequests }}. Расход по
              данным провайдера:
              {{
                offlineStatuses[offlineBookId]?.actualCostUsd === null
                  ? "неизвестен"
                  : `$${offlineStatuses[offlineBookId]?.actualCostUsd}`
              }}. Оценочный резерв:
              {{
                offlineStatuses[offlineBookId]?.estimatedReservedUsd === null
                  ? "неизвестен"
                  : `$${offlineStatuses[offlineBookId]?.estimatedReservedUsd}`
              }}.
              <template
                v-if="offlineStatuses[offlineBookId]?.unknownCostRequests"
                >Запросов с неизвестной стоимостью:
                {{
                  offlineStatuses[offlineBookId]?.unknownCostRequests
                }}.</template
              >
            </p>
            <button
              v-if="offlineStatuses[offlineBookId]?.unknownCostRequests"
              class="tonal"
              :disabled="offlineBusy"
              @click="reconcileOfflineCost"
            >
              Сверить расходы
            </button>
            <p v-if="offlineStatuses[offlineBookId]?.error" class="warning">
              {{ offlineStatuses[offlineBookId]?.error }}
            </p>
          </template>
          <p v-if="offlineBusy" role="status">Загрузка…</p>
          <p v-if="offlineInfo" role="status">{{ offlineInfo }}</p>
          <p v-if="offlineError" class="warning" role="alert">
            {{ offlineError }}
          </p>
          <template v-if="offlineQuote">
            <dl class="quote-details">
              <div>
                <dt>Движок</dt>
                <dd>{{ offlineQuote.engine }}</dd>
              </div>
              <div>
                <dt>Голос</dt>
                <dd>
                  {{
                    offlineQuote.engine === "openrouter"
                      ? offlineQuote.modelVoice || "По умолчанию"
                      : offlineQuote.voice
                  }}
                </dd>
              </div>
              <div>
                <dt>Модель</dt>
                <dd>
                  {{ offlineQuote.model ?? "Системный или локальный голос" }}
                </dd>
              </div>
              <div>
                <dt>Всего частей</dt>
                <dd>{{ offlineQuote.totalBlocks }}</dd>
              </div>
              <div>
                <dt>Уже сохранено</dt>
                <dd>{{ offlineQuote.cachedBlocks }}</dd>
              </div>
              <div>
                <dt>Осталось</dt>
                <dd>
                  {{
                    Math.max(
                      0,
                      offlineQuote.totalBlocks - offlineQuote.cachedBlocks,
                    )
                  }}
                </dd>
              </div>
              <div>
                <dt>Символов к озвучке</dt>
                <dd>{{ offlineQuote.characters.toLocaleString("ru-RU") }}</dd>
              </div>
              <div
                v-if="
                  offlineQuote.billableUnits !== null &&
                  offlineQuote.estimateSource === 'user'
                "
              >
                <dt>
                  {{
                    offlineQuote.costUnit === "utf8_byte"
                      ? "UTF-8 байтов к озвучке"
                      : "Тарифицируемых символов"
                  }}
                </dt>
                <dd>
                  {{ offlineQuote.billableUnits.toLocaleString("ru-RU") }}
                </dd>
              </div>
              <div>
                <dt>Стоимость</dt>
                <dd>
                  {{
                    offlineQuote.costUnit === "free"
                      ? "Бесплатно"
                      : offlineQuote.estimateSource === "user" &&
                          offlineQuote.estimatedCost !== null
                        ? `≈ $${offlineQuote.estimatedCost}`
                        : "Неизвестна"
                  }}
                </dd>
              </div>
            </dl>
            <p>
              Приложение отправляет запросами до
              {{ offlineQuote.maxPassages }} предложений и
              {{ offlineQuote.maxBlockBytes }} байт после экранирования. Это
              внутренний предел; ограничения провайдера могут отличаться.
            </p>
            <p
              v-if="offlineQuote.retryRequiredBlocks > 0"
              class="warning"
              id="offline-retry-warning"
            >
              Для {{ offlineQuote.retryRequiredBlocks }} блоков предыдущая
              генерация не сохранилась или её результат неизвестен. Повтор может
              быть оплачен ещё раз.
            </p>
            <label v-if="offlineQuote.retryRequiredBlocks > 0">
              <input
                v-model="offlineRetryConfirmed"
                type="checkbox"
                :disabled="offlineBusy"
                aria-describedby="offline-retry-warning"
              />
              Я согласен на повторную генерацию и возможное дополнительное
              списание
            </label>
            <div
              v-if="offlineQuote.engine === 'openrouter'"
              class="billing-unit"
            >
              <label
                >Максимум новых блоков<input
                  v-model.number="maxNewBlocks"
                  type="number"
                  min="1"
                  step="1"
                  :disabled="offlineBusy"
              /></label>
              <label
                >Ориентировочный предел расходов, USD (необязательно)<input
                  v-model="maxCostUsd"
                  type="text"
                  inputmode="decimal"
                  pattern="[0-9]+([.][0-9]+)?"
                  :disabled="
                    offlineBusy || offlineQuote.costUnit === 'unknown'
                  "
              /></label>
              <small
                v-if="!validMaxCost && offlineQuote.costUnit !== 'unknown'"
                class="warning"
                >Укажите сумму цифрами с точкой, например 0.25.</small
              >
              <small
                >Предел расходов проверяется до следующего запроса; стоимость
                уже отправленного запроса может его превысить.</small
              >
              <label v-if="offlineQuote.costUnit === 'unknown'"
                ><input
                  v-model="allowUnknownCost"
                  type="checkbox"
                  :disabled="offlineBusy"
                />
                Разрешить ограниченное число блоков при неизвестной
                стоимости</label
              >
              <small v-if="offlineQuote.costUnit === 'unknown'"
                >Без данных о фактических расходах следующий платный блок не
                запускается; предел в USD не гарантирует итоговое
                списание.</small
              >
            </div>
            <p>
              Без меток провайдера переходы внутри аудиофрагмента
              приблизительные.
            </p>
            <p v-if="offlineQuote.estimateSource === 'user'">
              Оценка рассчитана по единице, указанной вами; фактическое списание
              может отличаться.
            </p>
            <p v-if="offlineQuote.costUnit === 'unknown'">
              Точная стоимость неизвестна.
              <template
                v-if="offlineQuote.costReason === 'providerunitunavailable'"
                >OpenRouter не сообщает подтверждённую единицу
                тарификации.</template
              >
              <template
                v-else-if="
                  offlineQuote.costReason === 'additionalrateunitunavailable'
                "
                >Для дополнительной ставки единица тарификации
                неизвестна.</template
              >
              <template v-else-if="offlineQuote.costReason === 'invalidrate'"
                >Тариф API не удалось прочитать.</template
              >
              Проверьте тариф на странице модели до запуска.
            </p>
            <p v-if="offlineQuote.pricing">
              Тариф API, USD: вход
              {{ offlineQuote.pricing.prompt || "не указан" }}, выход
              {{ offlineQuote.pricing.completion || "не указан" }}.
              <template
                v-if="
                  offlineQuote.pricingSource === 'openrouter_api' &&
                  offlineQuote.pricingVerifiedAt
                "
                >Получен из OpenRouter
                {{
                  new Date(offlineQuote.pricingVerifiedAt).toLocaleString(
                    "ru-RU",
                  )
                }}.</template
              >
            </p>
            <div
              v-if="
                offlineQuote.engine === 'openrouter' &&
                offlineQuote.model &&
                offlineQuote.costUnit !== 'free'
              "
              class="billing-unit"
            >
              <label
                >Единица тарифа модели
                <select v-model="billingUnitDraft" :disabled="offlineBusy">
                  <option value="">Не указана</option>
                  <option value="character">Символы</option>
                  <option value="utf8_byte">Байты UTF-8</option>
                </select>
              </label>
              <button
                type="button"
                class="tonal"
                :disabled="offlineBusy"
                @click="saveBillingUnit"
              >
                Сохранить и пересчитать
              </button>
              <small
                >Укажите единицу по странице модели. Приложение не определяет её
                автоматически.<template v-if="billingDirty">
                  Сохраните выбор и проверьте обновлённый расчёт перед
                  запуском.</template
                ></small
              >
            </div>
          </template>
          <div class="dialog-actions">
            <button
              v-if="offlineQuote"
              class="primary"
              :disabled="
                offlineBusy ||
                billingDirty ||
                (offlineQuote.costUnit !== 'unknown' && !validMaxCost) ||
                (offlineQuote.engine === 'openrouter' &&
                  offlineQuote.totalBlocks > offlineQuote.cachedBlocks &&
                  (!maxNewBlocks ||
                    !Number.isSafeInteger(maxNewBlocks) ||
                    maxNewBlocks < 1 ||
                    (offlineQuote.costUnit === 'unknown' &&
                      !allowUnknownCost))) ||
                (offlineQuote.retryRequiredBlocks > 0 && !offlineRetryConfirmed)
              "
              @click="startOffline"
            >
              {{
                offlineStatuses[offlineBookId]?.state === "paused"
                  ? "Продолжить сохранение"
                  : "Начать сохранение"
              }}</button
            ><button
              v-if="offlineStatuses[offlineBookId]?.state === 'running'"
              class="tonal"
              :disabled="offlineBusy"
              @click="pauseOffline"
            >
              Приостановить</button
            ><button
              v-if="
                offlineStatuses[offlineBookId]?.state === 'complete' ||
                offlineStatuses[offlineBookId]?.state === 'paused' ||
                offlineStatuses[offlineBookId]?.state === 'error'
              "
              class="text-button"
              :disabled="offlineBusy"
              @click="removeOffline"
            >
              Удалить сохранённую озвучку</button
            ><button
              v-if="offlineError && !offlineBusy"
              class="tonal"
              @click="quoteOffline"
            >
              Повторить расчёт</button
            ><button class="text-button" @click="closeOffline">Закрыть</button>
          </div>
        </section>
      </div>
    </main>
    <div v-if="closing" class="dialog-overlay" role="status" aria-live="polite">
      <section class="offline-dialog">
        <p v-if="!shutdownError">Сохранение данных и завершение работы…</p>
        <p v-if="shutdownWarning" class="warning">{{ shutdownWarning }}</p>
        <template v-if="shutdownError">
          <p class="warning">
            Не удалось завершить работу: {{ shutdownError }}
          </p>
          <button class="tonal" @click="closeWindow">
            Повторить завершение
          </button>
        </template>
      </section>
    </div>
  </div>
</template>
