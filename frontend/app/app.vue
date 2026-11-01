<script setup lang="ts">
import { invoke, isTauri, convertFileSrc } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useVirtualizer } from "@tanstack/vue-virtual";
import type { ComponentPublicInstance } from "vue";
import { pronunciation, speechChunks } from "./utils/speech";
import { replayIndex, runPlayback, synthesizeThenPlay } from "./utils/playback";
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
  ChevronDown,
  ChevronUp,
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
  Voice,
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
const completed = ref(false);
const compact = ref(false);
const loading = ref(true);
const notice = ref("");
const chapterMenu = ref(false);
const dictionaryWord = ref("");
const dictionaryValue = ref("");
const reader = ref<HTMLElement | null>(null);
const native = ref(false);
let generation = 0;
let audio: HTMLAudioElement | null = null;
let audioCancel: (() => void) | null = null;
let positionTimer: ReturnType<typeof setTimeout> | undefined;
let preferencesTimer: ReturnType<typeof setTimeout> | undefined;
let positionQueue = Promise.resolve();
let preferenceQueue = Promise.resolve();
let speechStop = Promise.resolve();
let followingUntil = 0;
let userScrollUntil = 0;
let openGeneration = 0;
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

const colors = [
  0xff69d9b1, 0xff5ba8ff, 0xff9b87f5, 0xffff6f91, 0xffff9f43, 0xffffd166,
  0xff4dd0e1, 0xffa3d977,
];
const colorNames = [
  "мятный",
  "синий",
  "фиолетовый",
  "розовый",
  "оранжевый",
  "жёлтый",
  "бирюзовый",
  "зелёный",
];
const accent = computed(
  () =>
    `#${(prefs.value.primaryColor & 0xffffff).toString(16).padStart(6, "0")}`,
);
const background = computed(() =>
  prefs.value.backgroundImagePath && native.value
    ? convertFileSrc(prefs.value.backgroundImagePath)
    : "",
);
const themeStyle = computed(() => ({
  "--accent": accent.value,
  "--panel-alpha": background.value ? 1 - prefs.value.surfaceTransparency : 1,
  "--reader-size": `${prefs.value.fontSize}px`,
  "--reader-line": prefs.value.lineHeight,
}));
function report(e: unknown) {
  notice.value = String(e);
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
  const snapshot = JSON.parse(JSON.stringify(prefs.value)) as Preferences;
  preferenceQueue = preferenceQueue
    .then(async () => {
      await invoke("save_preferences", { preferences: snapshot });
    })
    .catch(report);
  return preferenceQueue;
}
function savePrefs() {
  clearTimeout(preferencesTimer);
  preferencesTimer = setTimeout(() => {
    void savePrefsNow();
  }, 250);
}
function stop() {
  generation++;
  playing.value = false;
  audioCancel?.();
  audioCancel = null;
  if (audio) {
    audio.pause();
    audio.src = "";
    audio = null;
  }
  if (native.value)
    speechStop = speechStop
      .then(async () => {
        await invoke("stop_speech");
      })
      .catch(report);
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
  stop();
  active.value = index;
  b.offset = passage.start;
  completed.value = false;
  savePosition();
  void jump(index);
  if (resume) void play();
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
    active.value = index;
    book.value.offset = passage.start;
    completed.value = false;
    savePosition();
  }
}
async function speakChunk(text: string, speed: number, token: number) {
  await speechStop;
  if (token !== generation) return;
  await synthesizeThenPlay(
    () =>
      invoke<SpeechAudio>("synthesize", {
        text,
        voice: prefs.value.voice,
        speed,
      }),
    (result) =>
      new Promise<void>((resolve, reject) => {
        const player = new Audio(`data:${result.mime};base64,${result.data}`);
        audio = player;
        audioCancel = resolve;
        player.onended = () => {
          audioCancel = null;
          audio = null;
          resolve();
        };
        player.onerror = () => {
          audioCancel = null;
          audio = null;
          reject(new Error("Не удалось воспроизвести аудио"));
        };
        void player.play().catch(reject);
      }),
    () => token === generation,
  );
}
async function speak(text: string, speed: number, token: number) {
  for (const chunk of speechChunks(text)) {
    if (token !== generation) return;
    await speakChunk(chunk, speed, token);
  }
}
async function play() {
  const b = book.value;
  if (!native.value || !b) return;
  if (completed.value && !b.passages.length) {
    let chapter = b.chapters.length - 1;
    while (chapter >= 0 && !b.chapters[chapter]?.text.trim()) chapter--;
    if (chapter >= 0) {
      await openBook(b.id, chapter, 0);
      await play();
    }
    return;
  }
  if (!b.passages.length) return;
  if (completed.value) {
    active.value = replayIndex(completed.value, active.value);
    b.offset = b.passages[active.value]!.start;
    completed.value = false;
  }
  stop();
  const token = generation;
  playing.value = true;
  notice.value = "";
  try {
    await runPlayback({
      isCurrent: () => token === generation,
      current: () => {
        const current = book.value;
        const passage = current?.passages[active.value];
        return current && passage ? { current, passage } : null;
      },
      text: ({ passage }) =>
        pronunciation(passage.text, prefs.value.dictionary),
      before: ({ current, passage }) => {
        current.offset = passage.start;
        savePosition();
        void jump();
      },
      speak: (text) => speak(text, book.value!.speed, token),
      advance: async ({ current, passage }) => {
        current.offset = passage.end;
        savePosition();
        if (active.value + 1 < current.passages.length) {
          active.value++;
          return true;
        }
        if (current.chapter + 1 >= current.chapters.length) {
          current.offset =
            current.chapters[current.chapter]?.text.length ?? passage.end;
          completed.value = true;
          playing.value = false;
          await savePositionNow();
          return false;
        }
        clearTimeout(positionTimer);
        let nextChapter = current.chapter + 1;
        while (
          nextChapter + 1 < current.chapters.length &&
          !current.chapters[nextChapter]?.text.trim()
        )
          nextChapter++;
        const nextText = current.chapters[nextChapter]?.text ?? "";
        const nextPosition = {
          id: current.id,
          chapter: nextChapter,
          offset: nextText.trim() ? 0 : nextText.length,
          speed: current.speed,
        };
        positionQueue = positionQueue
          .then(async () => {
            if (token === generation)
              updateSummary(
                await invoke<BookSummary>("save_position", nextPosition),
              );
          })
          .catch(report);
        await positionQueue;
        if (token !== generation) return false;
        const next = await invoke<Book>("open_book", { id: current.id });
        if (token !== generation) return false;
        book.value = next;
        active.value = 0;
        await jump();
        if (!next.passages.length) {
          completed.value = true;
          playing.value = false;
          return false;
        }
        return true;
      },
    });
  } catch (e) {
    if (token === generation) {
      playing.value = false;
      report(`Ошибка озвучки: ${String(e)}`);
    }
  }
}
async function previewVoice() {
  stop();
  const token = generation;
  try {
    await speak("Здравствуйте! Так звучит выбранный голос.", 1, token);
  } catch (e) {
    if (token === generation) report(e);
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
  if (page.value === "reader" && target !== "reader" && playing.value) stop();
  page.value = target;
  if (target === "reader") await jump();
  if (target === "marks") await loadMarks();
}
async function importBooks() {
  if (!native.value) return;
  try {
    const paths = await open({
      multiple: true,
      filters: [{ name: "Книги", extensions: ["epub", "fb2", "txt"] }],
    });
    if (!paths?.length) return;
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
    if (!clear && !path) return;
    await savePrefsNow();
    prefs.value = await invoke<Preferences>("set_background", { path });
  } catch (e) {
    report(e);
  }
}
async function importLegacy() {
  try {
    const path = await open({ directory: true, multiple: false });
    if (!path) return;
    const count = await invoke<number>("import_legacy", { path });
    const state = await invoke<Bootstrap>("bootstrap");
    books.value = state.books;
    prefs.value = state.preferences;
    voices.value = state.voices;
    notice.value = `Импортировано книг: ${count}`;
  } catch (e) {
    report(e);
  }
}
function addDictionary() {
  const word = dictionaryWord.value.trim();
  const value = dictionaryValue.value.trim();
  if (!word || !value) return;
  prefs.value.dictionary[word] = value;
  dictionaryWord.value = "";
  dictionaryValue.value = "";
  savePrefs();
}
function changeVoice() {
  stop();
  savePrefs();
}
function changeSpeed(event: Event) {
  if (!book.value) return;
  const resume = playing.value;
  stop();
  book.value.speed = Number((event.target as HTMLInputElement).value);
  savePosition();
  if (resume) void play();
}
function coverUrl(cover: string | null) {
  return cover
    ? cover.startsWith("data:")
      ? cover
      : `data:image/*;base64,${cover}`
    : "";
}
onMounted(async () => {
  native.value = isTauri();
  if (native.value) {
    try {
      await getCurrentWindow().onCloseRequested(async (event) => {
        event.preventDefault();
        stop();
        await Promise.all([savePositionNow(), savePrefsNow(), speechStop]);
        await getCurrentWindow().destroy();
      });
      const state = await invoke<Bootstrap>("bootstrap");
      books.value = state.books;
      prefs.value = state.preferences;
      voices.value = state.voices;
      notice.value = state.warnings.join("\n");
    } catch (e) {
      report(e);
    }
  }
  loading.value = false;
});
onBeforeUnmount(() => {
  stop();
  void savePrefsNow();
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
    <aside class="rail panel">
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
    <main class="main">
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
                  >{{ Math.round(item.progress * 100) }}% прочитано</small
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
            <i :style="{ width: `${book.progress * 100}%` }" />
          </div>
          <div class="player-row">
            <div class="player-book">
              <span class="cover small"
                ><img
                  v-if="book.cover"
                  :src="coverUrl(book.cover)"
                  alt="" /><BookOpen v-else /></span
              ><span>{{ book.title }}</span>
            </div>
            <div class="controls">
              <button
                class="icon"
                title="Предыдущее предложение"
                aria-label="Предыдущее предложение"
                @click="skip(-1)"
              >
                <SkipBack /></button
              ><button
                class="play-button"
                :title="playing ? 'Пауза' : 'Воспроизвести'"
                :aria-label="playing ? 'Пауза' : 'Воспроизвести'"
                @click="playing ? stop() : play()"
              >
                <Pause v-if="playing" /><Play v-else /></button
              ><button
                class="icon"
                title="Следующее предложение"
                aria-label="Следующее предложение"
                @click="skip(1)"
              >
                <SkipForward /></button
              ><button
                class="icon"
                title="Стоп"
                aria-label="Стоп"
                @click="stop"
              >
                <Square />
              </button>
            </div>
            <div class="player-end">
              <span>{{ book.speed.toFixed(1) }}×</span
              ><button
                class="icon"
                title="Дополнительно"
                aria-label="Дополнительно"
                @click="compact = !compact"
              >
                <ChevronDown v-if="compact" /><ChevronUp v-else />
              </button>
            </div>
          </div>
          <label v-if="!compact" class="speed-row"
            ><span>Скорость</span
            ><input
              type="range"
              min="0.5"
              max="3"
              step="0.1"
              :value="book.speed"
              @change="changeSpeed"
            /><span>0,5–3,0× · голос может ограничить</span></label
          >
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
          <div class="colors">
            <button
              v-for="(color, index) in colors"
              :key="color"
              class="color"
              :class="{ chosen: prefs.primaryColor === color }"
              :style="{
                background: `#${(color & 0xffffff).toString(16).padStart(6, '0')}`,
              }"
              :aria-label="`Цвет ${colorNames[index]}`"
              :aria-pressed="prefs.primaryColor === color"
              @click="
                prefs.primaryColor = color;
                savePrefs();
              "
            >
              {{ prefs.primaryColor === color ? "✓" : "" }}
            </button>
          </div>
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
          ><label class="setting"
            >Сила размытия · {{ Math.round(prefs.backgroundBlur * 100) }}%<input
              v-model.number="prefs.backgroundBlur"
              type="range"
              min="0"
              max="1"
              step="0.05"
              :disabled="!prefs.backgroundBlurEnabled"
              @input="savePrefs" /></label
          ><img
            class="background-preview"
            :src="background"
            alt="Фоновое изображение" /><label class="setting"
            >Прозрачность фото ·
            {{ Math.round((1 - prefs.backgroundOpacity) * 100) }}%<input
              type="range"
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
            {{ Math.round(prefs.backgroundDim * 100) }}%<input
              v-model.number="prefs.backgroundDim"
              type="range"
              min="0"
              max="0.8"
              step="0.05"
              @input="savePrefs" /></label
          ><label class="setting"
            >Прозрачность панелей ·
            {{ Math.round(prefs.surfaceTransparency * 100) }}%<input
              v-model.number="prefs.surfaceTransparency"
              type="range"
              min="0"
              max="0.75"
              step="0.05"
              @input="savePrefs" /></label
        ></template>
        <label class="setting"
          >Размер текста · {{ Math.round(prefs.fontSize)
          }}<input
            v-model.number="prefs.fontSize"
            type="range"
            min="14"
            max="32"
            step="1"
            @input="savePrefs" /></label
        ><label class="setting"
          >Межстрочный интервал · {{ Number(prefs.lineHeight.toFixed(2))
          }}<input
            v-model.number="prefs.lineHeight"
            type="range"
            min="1.2"
            max="2.2"
            step="0.05"
            @input="savePrefs"
        /></label>
        <label class="setting"
          >Голос<select
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
            :disabled="!native"
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
        <label v-if="book" class="setting"
          >Скорость для «{{ book.title }}» · {{ book.speed.toFixed(1) }}×<input
            type="range"
            min="0.5"
            max="3"
            step="0.1"
            :value="book.speed"
            @change="changeSpeed"
        /></label>
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
                delete prefs.dictionary[word];
                savePrefs();
              "
            >
              <X />
            </button>
          </div>
        </div>
        <hr />
        <div class="setting">
          <strong>Перенос библиотеки</strong
          ><small>Импорт книг и настроек из папки данных Flutter Readio</small>
          <div>
            <button class="tonal" :disabled="!native" @click="importLegacy">
              Импортировать библиотеку
            </button>
          </div>
        </div>
      </section>
    </main>
  </div>
</template>
